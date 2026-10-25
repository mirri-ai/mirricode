//! SQLite session index — the rust engine's `session-index.db` read model.
//!
//! The TS engine keeps its session summary index in
//! `<cacheDir>/session-index.db` (see
//! `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts`); the
//! rust engine mirrors that design with its own database at
//! `<home>/session-index.db` (NOT `<home>/cache/` — that file belongs to the
//! TS engine; the two never collide). The database is engine-local: its
//! schema uses snake_case and is not shared with the TS side.
//!
//! Storage model (single-writer, WAL readers — this engine is effectively
//! single-process-per-home, but the WAL + `busy_timeout` keeps a second
//! process sharing the home from blocking outright, exactly the TS
//! rationale):
//!
//! - [`SessionIndex::open`] creates parent directories, opens the database
//!   with `journal_mode = WAL` and `busy_timeout = 5000`, folds any leftover
//!   WAL from a crashed previous process with a best-effort
//!   `wal_checkpoint(TRUNCATE)`, applies the schema, and seeds the
//!   `workspaces` table from the cross-engine `workspaces.json` registry (see
//!   [`registry`]).
//! - Session rows are a read-model cache over the on-disk session
//!   directories (`state.json` / `wire.jsonl`): writes keep the model warm
//!   with `state_mtime` stamped for the incremental reconcile, and a cold or
//!   stale row is rebuilt from the same validated disk read the plain
//!   directory scan performs.
//! - Workspace rows are additionally written through to `workspaces.json`
//!   so a TS engine sharing the home still sees rust-registered workspaces
//!   (the registry stays the cross-engine source of truth for workspace
//!   identity).
//!
//! Timestamps are stored as **fixed-width** RFC 3339 text
//! (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`) rather than epoch milliseconds: the
//! wire views this index serves carry nanosecond-precision RFC 3339 strings,
//! and a fixed-width encoding round-trips those bytes exactly while still
//! ordering lexically (= chronologically) in SQL.
//!
//! This crate is pure storage: it does **not** depend on `mirri-core`. The
//! integration (converting between engine metadata and these rows) lives in
//! the embedded server.

mod registry;

pub use registry::{
    LockGuard, RegistryLock, RegistryWriter, WORKSPACE_REGISTRY_FILENAME, add_tombstone,
    lock_file_path, read_workspace_registry, write_workspace_registry,
};

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// How long a statement waits for another process's write lock before
/// failing (the TS `BUSY_TIMEOUT_MS`).
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// The fixed-width RFC 3339 timestamp encoding stored in every timestamp
/// column — always nanosecond precision so the text both round-trips the
/// wire's exact bytes and sorts lexically.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.9fZ";

/// The schema version this build of the index writes — stamped into
/// `PRAGMA user_version` on open, stepped by the migration chain in
/// [`SessionIndex::open`].
///
/// History:
///
/// - `0` — the pre-W5 database (no `last_prompt` column).
/// - `1` — adds `sessions.last_prompt TEXT` (the `exclude_empty` filter's
///   data source, SDR-004: the most recent user input, `NULL` for a session
///   that never received one).
/// - `2` — adds the retention domain's columns (§3.1 / ADR-006):
///   `sessions.archived_at TEXT`, `sessions.compressed INTEGER`, and
///   `sessions.compressed_at TEXT` — the scan/candidate-supply columns for
///   the auto-compress pipeline (rows derive from `state.json` and are
///   rebuilt by the converge, so the migration only needs to add the
///   columns: existing rows read back with the defaults).
const SCHEMA_VERSION: i32 = 2;

/// The sessions + workspaces schema (snake_case — this database is
/// engine-local, never shared with the TS side). The column order matches
/// what a v0→v1 `ALTER TABLE` migration produces (`last_prompt` appended
/// last), so a fresh database and a migrated one are byte-identical.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    cwd         TEXT,
    title       TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0,
    tags        TEXT,
    custom      TEXT,
    state_mtime INTEGER,
    last_prompt TEXT,
    archived_at  TEXT,
    compressed   INTEGER,
    compressed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_ws_updated
    ON sessions (workspace_id, updated_at DESC);
CREATE TABLE IF NOT EXISTS workspaces (
    id             TEXT PRIMARY KEY NOT NULL,
    root           TEXT,
    name           TEXT,
    created_at     TEXT NOT NULL,
    last_opened_at TEXT NOT NULL,
    sort_order     INTEGER NOT NULL DEFAULT 0
);
";

/// One indexed session — the summary a listing resolves without touching the
/// session directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The session id (the on-disk directory name).
    pub id: String,
    /// The workspace bucket key (`wd_<slug>_<hash12>`) the session belongs to.
    pub workspace_id: String,
    /// The session's recorded working directory, when known.
    pub cwd: Option<String>,
    /// The session's display title, when known.
    pub title: Option<String>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last updated.
    pub updated_at: DateTime<Utc>,
    /// Whether the session is archived (default listings exclude it).
    pub archived: bool,
    /// The session's free-form tags (the wire `SessionView.tags` set).
    pub tags: Vec<String>,
    /// Free-form TS extension payload (`custom`), when present.
    pub custom: Option<serde_json::Value>,
    /// The text of the session's most recent **user** input, when it ever
    /// received one (SDR-004: regardless of the turn's outcome — "the
    /// conversation failed" is not empty; "never conversed" is). The
    /// `exclude_empty` listing filter reads exactly this column.
    pub last_prompt: Option<String>,
    /// The `state.json` mtime (epoch milliseconds) this row was built from —
    /// the stamp the incremental reconcile compares against.
    pub state_mtime: Option<i64>,
    /// When the session was archived (`archivedAt` in `state.json`, the
    /// retention domain's §3.1 stamp), when recorded — the scan's
    /// "already archived for N days" anchor and candidate-supply input.
    /// `None` for live, restored, and legacy sessions (the scanner
    /// backfills those on first sight, SDR-008).
    pub archived_at: Option<DateTime<Utc>>,
    /// Whether the wire log is gzip-compressed on disk — the `state.json`
    /// **cached** flag (ADR-007: the disk shape is the truth source; the
    /// scan reconciles against it before acting).
    pub compressed: bool,
    /// When the wire log was compressed (`compressedAt`), when recorded —
    /// the tier-cleanup "already compressed for N days" anchor.
    pub compressed_at: Option<DateTime<Utc>>,
}

/// One registered workspace — the `workspaces` table row (seeded from and
/// written through to the cross-engine `workspaces.json` registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    /// The workspace key (`wd_<slug>_<hash12>`).
    pub id: String,
    /// The workspace root path, when known.
    pub root: Option<String>,
    /// The workspace display name, when known.
    pub name: Option<String>,
    /// When the workspace was first registered.
    pub created_at: DateTime<Utc>,
    /// When the workspace was last opened.
    pub last_opened_at: DateTime<Utc>,
    /// Manual ordering position (the drag-reorder seam; 0 = default).
    pub sort_order: i64,
}

/// Listing options for [`SessionIndex::list_sessions`] — the TS `list`
/// option surface minus the `childOf` filter (no fork-parent custom payload
/// exists on the rust side yet).
#[derive(Debug, Clone, Default)]
pub struct ListOpts<'a> {
    /// Restrict the listing to these workspace keys; `None` or an empty
    /// slice lists every workspace (the TS `workspaceIds` semantics).
    pub workspace_ids: Option<&'a [String]>,
    /// Include archived sessions; `false` (the default) excludes them.
    pub include_archived: bool,
    /// Return at most this many rows.
    pub limit: Option<u64>,
    /// Skip this many rows before the first returned one.
    pub offset: u64,
    /// Keyset cursor: return only rows strictly OLDER than this id's row
    /// in the `(updated_at DESC, id DESC)` order — the forward/default
    /// paging direction (the TS `before_id`).
    pub before_id: Option<&'a str>,
    /// Keyset cursor: return only rows strictly NEWER than this id's row
    /// (the TS `after_id`).
    pub after_id: Option<&'a str>,
    /// Exclude sessions that never received a user input (SDR-004): rows
    /// whose `last_prompt` is `NULL` or the empty string (the TS judgment
    /// `(summary.lastPrompt ?? '').length === 0`).
    pub exclude_empty: bool,
}

/// A reconcile reference — one indexed session's identity plus the stamp the
/// incremental reconcile compares against (the TS `listAllRows` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRowRef {
    pub id: String,
    pub workspace_id: String,
    pub state_mtime: Option<i64>,
}

/// The SQLite session/workspace index. One open connection behind a mutex —
/// a single writer with WAL readers (see the module docs).
#[derive(Debug)]
pub struct SessionIndex {
    /// The single connection every method funnels through (short critical
    /// sections; SQLite serializes the writes itself).
    conn: Mutex<Connection>,
    /// The `workspaces.json` registry this index writes through to — always
    /// the database file's sibling (the Mirri home's registry).
    registry_path: Option<PathBuf>,
}

impl SessionIndex {
    /// Open (creating when absent) the index at `path`.
    ///
    /// Creates the parent directories (a fresh home may not exist yet — the
    /// TS open has the same guard), enables WAL with a 5 s busy timeout,
    /// folds any leftover WAL from a crashed previous process with a
    /// best-effort `wal_checkpoint(TRUNCATE)`, applies the schema, and seeds
    /// the `workspaces` table from the sibling `workspaces.json` registry
    /// when one exists (an upsert that never clobbers an existing row's
    /// `sort_order`).
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"
        ))?;
        // Best-effort startup checkpoint: fold the previous process's
        // leftover WAL into the main database so a cold read does not replay
        // a multi-MB WAL on the first query. A busy database skips it and
        // performs normally.
        let _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", []);
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        let registry_path = path
            .parent()
            .map(|parent| parent.join(registry::WORKSPACE_REGISTRY_FILENAME));
        let index = Self {
            conn: Mutex::new(conn),
            registry_path,
        };
        index.seed_workspaces_from_registry();
        Ok(index)
    }

    /// Step the database's `PRAGMA user_version` forward — the migration
    /// chain (see [`SCHEMA_VERSION`]'s history). Runs on every open and is
    /// a no-op on an already-current database (re-opening never re-runs a
    /// step). A v0 database predates W5 and carries the `sessions` table
    /// **without** the `last_prompt` column: the fresh-database schema above
    /// already includes it (`CREATE TABLE IF NOT EXISTS` is a no-op on the
    /// existing table), so the step checks `pragma_table_info` and adds the
    /// column only when it is genuinely missing — a fresh database never
    /// runs the `ALTER TABLE`.
    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= i64::from(SCHEMA_VERSION) {
            return Ok(());
        }
        if version == 0 {
            let has_last_prompt = conn
                .prepare(
                    "SELECT 1 FROM pragma_table_info('sessions') \
                     WHERE name = 'last_prompt'",
                )?
                .exists([])?;
            if !has_last_prompt {
                conn.execute("ALTER TABLE sessions ADD COLUMN last_prompt TEXT", [])?;
            }
            conn.pragma_update(None, "user_version", 1)?;
        }
        if version <= 1 {
            // The retention columns (v2): guarded per-column so a fresh
            // database (whose SCHEMA already carries them — CREATE TABLE
            // IF NOT EXISTS is a no-op on an existing table) never runs the
            // ALTER, and a partially-migrated file (a crash between two
            // ALTERs) re-runs only the missing one.
            for (column, decl) in [
                ("archived_at", "TEXT"),
                ("compressed", "INTEGER"),
                ("compressed_at", "TEXT"),
            ] {
                let has_column = conn
                    .prepare(&format!(
                        "SELECT 1 FROM pragma_table_info('sessions') \
                         WHERE name = '{column}'"
                    ))?
                    .exists([])?;
                if !has_column {
                    conn.execute(
                        &format!("ALTER TABLE sessions ADD COLUMN {column} {decl}"),
                        [],
                    )?;
                }
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Upsert one session row. A `None` `state_mtime` keeps the stamp the
    /// row already carries (the TS upsert's subselect behaviour).
    pub fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sessions
                 (id, workspace_id, cwd, title, created_at, updated_at,
                  archived, tags, custom, state_mtime, last_prompt,
                  archived_at, compressed, compressed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(id) DO UPDATE SET
                 workspace_id = excluded.workspace_id,
                 cwd = excluded.cwd,
                 title = excluded.title,
                 created_at = excluded.created_at,
                 updated_at = excluded.updated_at,
                 archived = excluded.archived,
                 tags = excluded.tags,
                 custom = excluded.custom,
                 state_mtime = COALESCE(excluded.state_mtime, sessions.state_mtime),
                 last_prompt = excluded.last_prompt,
                 archived_at = excluded.archived_at,
                 compressed = excluded.compressed,
                 compressed_at = excluded.compressed_at",
            params![
                row.id,
                row.workspace_id,
                row.cwd,
                row.title,
                encode_timestamp(&row.created_at),
                encode_timestamp(&row.updated_at),
                row.archived as i64,
                serde_json::to_string(&row.tags).ok(),
                row.custom
                    .as_ref()
                    .and_then(|custom| serde_json::to_string(custom).ok()),
                row.state_mtime,
                row.last_prompt,
                row.archived_at.map(|at| encode_timestamp(&at)),
                row.compressed as i64,
                row.compressed_at.map(|at| encode_timestamp(&at)),
            ],
        )?;
        Ok(())
    }

    /// Delete one session row (the orphan pass of the reconcile).
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Read one session row, when present. A row whose timestamps no longer
    /// parse is treated as absent (a corrupt row is a cold miss, not a
    /// failure).
    pub fn get_session(&self, id: &str) -> Option<SessionRow> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, workspace_id, cwd, title, created_at, updated_at,
                    archived, tags, custom, state_mtime, last_prompt,
                    archived_at, compressed, compressed_at
             FROM sessions WHERE id = ?1",
            params![id],
            map_session_row,
        )
        .ok()
        .flatten()
    }

    /// List session rows — the TS `list` filter surface: workspace scoping,
    /// archived opt-in, `exclude_empty`, keyset cursors, and `updated_at
    /// DESC, id DESC` ordering with limit/offset.
    ///
    /// The cursors are **keyset** pagination over the `(updated_at, id)`
    /// composite order: `before_id` keeps only rows strictly older than the
    /// cursor row (the forward/default direction), `after_id` only rows
    /// strictly newer. An unknown cursor id answers the empty terminal page
    /// (`{items: [], has_more: false}`'s source — a client cannot spin on a
    /// cursor the index cannot advance).
    pub fn list_sessions(&self, opts: ListOpts<'_>) -> Result<Vec<SessionRow>> {
        let mut sql = String::from(
            "SELECT id, workspace_id, cwd, title, created_at, updated_at,
                    archived, tags, custom, state_mtime, last_prompt,
                    archived_at, compressed, compressed_at
             FROM sessions",
        );
        let mut clauses: Vec<String> = Vec::new();
        let mut bindings: Vec<String> = Vec::new();
        if let Some(ids) = opts.workspace_ids.filter(|ids| !ids.is_empty()) {
            let placeholders = ids
                .iter()
                .map(|id| {
                    bindings.push(id.clone());
                    "?".to_string()
                })
                .collect::<Vec<_>>()
                .join(",");
            clauses.push(format!("workspace_id IN ({placeholders})"));
        }
        if !opts.include_archived {
            clauses.push("archived = 0".to_string());
        }
        if opts.exclude_empty {
            // SDR-004 / TS parity: a NULL last_prompt *or the empty string*
            // marks a never-conversed session.
            clauses.push("(last_prompt IS NOT NULL AND last_prompt <> '')".to_string());
        }
        // The keyset cursor: resolve the cursor row first (unknown → the
        // empty terminal page), then compare the composite order. SQLite row
        // values make the tuple comparison exact.
        if let Some(cursor_id) = opts.before_id.or(opts.after_id) {
            let conn = self.lock();
            let cursor: Option<(String, String)> = conn
                .query_row(
                    "SELECT updated_at, id FROM sessions WHERE id = ?1",
                    params![cursor_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            drop(conn);
            match cursor {
                None => return Ok(Vec::new()),
                Some((updated_at, id)) => {
                    let comparison = if opts.before_id.is_some() { "<" } else { ">" };
                    clauses.push(format!("(updated_at, id) {comparison} (?, ?)"));
                    bindings.push(updated_at);
                    bindings.push(id);
                }
            }
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC");
        if let Some(limit) = opts.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if opts.offset > 0 {
            sql.push_str(&format!(" OFFSET {}", opts.offset));
        }

        let conn = self.lock();
        let mut statement = conn.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(bindings.iter()), map_session_row)?
            .collect::<Vec<_>>();
        Ok(rows.into_iter().filter_map(Result::ok).flatten().collect())
    }

    /// Count the non-archived session rows, optionally scoped to workspaces
    /// (the TS `count`).
    pub fn count_active(&self, workspace_ids: Option<&[String]>) -> Result<usize> {
        let mut sql = String::from("SELECT COUNT(*) FROM sessions WHERE archived = 0");
        let mut bindings: Vec<String> = Vec::new();
        if let Some(ids) = workspace_ids.filter(|ids| !ids.is_empty()) {
            let placeholders = ids
                .iter()
                .map(|id| {
                    bindings.push(id.clone());
                    "?".to_string()
                })
                .collect::<Vec<_>>()
                .join(",");
            sql.push_str(&format!(" AND workspace_id IN ({placeholders})"));
        }
        let conn = self.lock();
        let count: i64 =
            conn.query_row(&sql, rusqlite::params_from_iter(bindings.iter()), |row| {
                row.get(0)
            })?;
        Ok(count as usize)
    }

    /// Every distinct `workspace_id` the sessions table carries, paired with
    /// one representative `cwd` of that bucket (the last one seen, newest
    /// first — any is enough for the root-key comparison). The alias-set
    /// folding (D3) reads exactly this: an unregistered split bucket (a
    /// legacy spelling of a registered workspace's root) joins the group
    /// whose registered root it lexically equals.
    pub fn workspace_id_cwds(&self) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT workspace_id, cwd FROM (
                 SELECT workspace_id, cwd,
                        ROW_NUMBER() OVER (
                            PARTITION BY workspace_id
                            ORDER BY updated_at DESC, id DESC
                        ) AS rank
                 FROM sessions
             ) WHERE rank = 1",
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Vec<_>>();
        Ok(rows.into_iter().flatten().collect())
    }

    /// Every row's identity plus its `state_mtime` — the input of the
    /// incremental reconcile (the TS `listAllRows`).
    pub fn list_all_rows(&self) -> Result<Vec<SessionRowRef>> {
        let conn = self.lock();
        let mut statement = conn.prepare("SELECT id, workspace_id, state_mtime FROM sessions")?;
        let rows = statement
            .query_map([], |row| {
                Ok(SessionRowRef {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    state_mtime: row.get(2)?,
                })
            })?
            .collect::<Vec<_>>();
        Ok(rows.into_iter().flatten().collect())
    }

    /// Upsert one workspace row — the database row **and** the
    /// `workspaces.json` registry write-through (so a TS engine sharing the
    /// home sees the rust-registered workspace).
    pub fn upsert_workspace(&self, row: &WorkspaceRow) -> Result<()> {
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO workspaces
                     (id, root, name, created_at, last_opened_at, sort_order)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                     root = excluded.root,
                     name = excluded.name,
                     created_at = excluded.created_at,
                     last_opened_at = excluded.last_opened_at,
                     sort_order = excluded.sort_order",
                params![
                    row.id,
                    row.root,
                    row.name,
                    encode_timestamp(&row.created_at),
                    encode_timestamp(&row.last_opened_at),
                    row.sort_order,
                ],
            )?;
        }
        self.write_registry_through();
        Ok(())
    }

    /// Read one workspace row, when present.
    pub fn get_workspace(&self, id: &str) -> Option<WorkspaceRow> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, root, name, created_at, last_opened_at, sort_order
             FROM workspaces WHERE id = ?1",
            params![id],
            map_workspace_row,
        )
        .ok()
        .flatten()
    }

    /// Delete one workspace row (the W6 unregister path — the registry
    /// tombstone keeps the id from re-seeding on the next open). The
    /// workspace's session rows are untouched: purge deletes them separately
    /// through [`Self::delete_sessions_for_workspaces`], remove keeps them
    /// (the data-preserved mode — a re-registration restores the listing).
    pub fn delete_workspace(&self, id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM workspaces WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Delete every session row of the given workspace buckets, returning
    /// the number of rows removed (the purge result's `deletedSessions`
    /// count). An empty slice deletes nothing.
    pub fn delete_sessions_for_workspaces(&self, workspace_ids: &[String]) -> Result<usize> {
        if workspace_ids.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; workspace_ids.len()].join(",");
        let sql = format!("DELETE FROM sessions WHERE workspace_id IN ({placeholders})");
        let conn = self.lock();
        let removed = conn.execute(&sql, rusqlite::params_from_iter(workspace_ids.iter()))?;
        Ok(removed)
    }

    /// List the workspace rows, ordered by `sort_order` ascending then
    /// `last_opened_at` descending (the Phase 4 drag-reorder seam).
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT id, root, name, created_at, last_opened_at, sort_order
             FROM workspaces
             ORDER BY sort_order ASC, last_opened_at DESC",
        )?;
        let rows = statement
            .query_map([], map_workspace_row)?
            .collect::<Vec<_>>();
        Ok(rows.into_iter().filter_map(Result::ok).flatten().collect())
    }

    /// Seed the `workspaces` table from the registry file, when one exists —
    /// an upsert that refreshes the descriptive fields but never clobbers an
    /// existing row's `sort_order` (a manual reorder must survive a restart).
    fn seed_workspaces_from_registry(&self) {
        let Some(registry_path) = &self.registry_path else {
            return;
        };
        for row in registry::read_workspace_registry(registry_path) {
            let result = {
                let conn = self.lock();
                conn.execute(
                    "INSERT INTO workspaces
                         (id, root, name, created_at, last_opened_at, sort_order)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)
                     ON CONFLICT(id) DO UPDATE SET
                         root = excluded.root,
                         name = excluded.name,
                         created_at = excluded.created_at,
                         last_opened_at = excluded.last_opened_at",
                    params![
                        row.id,
                        row.root,
                        row.name,
                        encode_timestamp(&row.created_at),
                        encode_timestamp(&row.last_opened_at),
                    ],
                )
            };
            if let Err(error) = result {
                tracing::warn!(
                    workspace = %row.id,
                    %error,
                    "seeding the workspace index from workspaces.json failed for one entry"
                );
            }
        }
    }

    /// Mirror the `workspaces` table into the registry file — through the
    /// D8 merged write (design 08 §2.3): the database's rows ∪ the file's
    /// current rows, tombstones winning. A plain whole-document mirror would
    /// drop every row a TS engine sharing the home wrote to
    /// `workspaces.json` after the last seed — the exact cross-engine loss
    /// the merge rule exists to prevent (§7.3: this engine's writes never
    /// lose a row the read already saw). Best-effort: a write failure logs
    /// and is swallowed — the database row stays the engine-local truth, and
    /// the registry catches up on the next upsert.
    fn write_registry_through(&self) {
        let Some(registry_path) = &self.registry_path else {
            return;
        };
        let rows = match self.list_workspaces() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "reading the workspace rows for the registry mirror failed");
                return;
            }
        };
        if let Err(error) = registry::RegistryWriter::new(registry_path.clone()).write_merged(&rows)
        {
            tracing::warn!(
                registry = %registry_path.display(),
                %error,
                "writing through to workspaces.json failed"
            );
        }
    }

    /// Lock the connection — poisoned locks recover (the engine never
    /// deliberately poisons one; a panicked statement must not brick the
    /// index for the process lifetime).
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Encode a timestamp in the fixed-width storage format — always nanosecond
/// precision so the text sorts lexically and round-trips the wire bytes.
fn encode_timestamp(value: &DateTime<Utc>) -> String {
    value.format(TIMESTAMP_FORMAT).to_string()
}

/// Decode a stored timestamp; `None` for anything unparseable (a corrupt
/// row degrades to a cold miss, never a listing failure).
fn decode_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// Map one `sessions` table row. `Ok(None)` marks a row whose timestamps no
/// longer parse — treated as absent by every read path.
fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<SessionRow>> {
    let created_at: String = row.get(4)?;
    let updated_at: String = row.get(5)?;
    let (Some(created_at), Some(updated_at)) =
        (decode_timestamp(&created_at), decode_timestamp(&updated_at))
    else {
        return Ok(None);
    };
    let tags: Option<String> = row.get(7)?;
    let custom: Option<String> = row.get(8)?;
    Ok(Some(SessionRow {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        cwd: row.get(2)?,
        title: row.get(3)?,
        created_at,
        updated_at,
        archived: row.get::<_, i64>(6)? != 0,
        tags: tags
            .and_then(|tags| serde_json::from_str(&tags).ok())
            .unwrap_or_default(),
        custom: custom.and_then(|custom| serde_json::from_str(&custom).ok()),
        last_prompt: row.get(10)?,
        state_mtime: row.get(9)?,
        archived_at: row
            .get::<_, Option<String>>(11)?
            .and_then(|text| decode_timestamp(&text)),
        compressed: row.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0,
        compressed_at: row
            .get::<_, Option<String>>(13)?
            .and_then(|text| decode_timestamp(&text)),
    }))
}

/// Map one `workspaces` table row. `Ok(None)` marks a row whose timestamps no
/// longer parse — treated as absent by every read path.
fn map_workspace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<WorkspaceRow>> {
    let created_at: String = row.get(3)?;
    let last_opened_at: String = row.get(4)?;
    let (Some(created_at), Some(last_opened_at)) = (
        decode_timestamp(&created_at),
        decode_timestamp(&last_opened_at),
    ) else {
        return Ok(None);
    };
    Ok(Some(WorkspaceRow {
        id: row.get(0)?,
        root: row.get(1)?,
        name: row.get(2)?,
        created_at,
        last_opened_at,
        sort_order: row.get(5)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open an index inside a fresh tempdir (the database file itself names
    /// the "home" the registry mirror lives in).
    fn open_index() -> (SessionIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let index = SessionIndex::open(&dir.path().join("session-index.db")).unwrap();
        (index, dir)
    }

    /// One row builder with defaults, so each test spells only its deltas.
    fn session_row(id: &str, updated_at: DateTime<Utc>) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            workspace_id: "wd_project_abc123def456".to_string(),
            cwd: Some("/Users/dev/project".to_string()),
            title: Some(format!("session {id}")),
            created_at: updated_at - chrono::Duration::hours(1),
            updated_at,
            archived: false,
            tags: vec!["alpha".to_string()],
            custom: None,
            last_prompt: Some("what is up".to_string()),
            state_mtime: Some(1_700_000_000_000),
            archived_at: None,
            compressed: false,
            compressed_at: None,
        }
    }

    /// Given an upserted row, When it is fetched by id, Then every field
    /// round-trips — including the nanosecond timestamps and the tags.
    #[test]
    fn should_roundtrip_every_field_when_upserting_then_getting_a_session_row() {
        let (index, _dir) = open_index();
        let updated_at = DateTime::parse_from_rfc3339("2026-02-06T10:20:30.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        index
            .upsert_session(&session_row("sess-1", updated_at))
            .unwrap();

        let row = index.get_session("sess-1").expect("the row is present");

        assert_eq!(row, session_row("sess-1", updated_at));
    }

    /// Given no such row, When it is fetched by id, Then the answer is a
    /// miss, not a failure.
    #[test]
    fn should_miss_when_getting_an_unknown_session_row() {
        let (index, _dir) = open_index();
        assert!(index.get_session("nope").is_none());
    }

    /// Given rows updated at different times across two workspaces, When
    /// they are listed, Then the order is `updated_at` DESC (and `id` DESC
    /// breaks the ties), workspace-scoped when asked.
    #[test]
    fn should_order_rows_by_updated_at_desc_when_listing() {
        let (index, _dir) = open_index();
        let base = DateTime::parse_from_rfc3339("2026-02-06T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut other = session_row("sess-other", base);
        other.workspace_id = "wd_other_000000000000".to_string();
        let mut tie_a = session_row("sess-b", base);
        let mut tie_b = session_row("sess-a", base);
        tie_a.updated_at = base + chrono::Duration::seconds(5);
        tie_b.updated_at = base + chrono::Duration::seconds(5);
        for row in [&session_row("sess-old", base), &tie_a, &tie_b, &other] {
            index.upsert_session(row).unwrap();
        }

        let listed = index.list_sessions(ListOpts::default()).unwrap();
        let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
        // updated_at DESC; the two same-second rows tie-break by id DESC
        // (and "sess-other" beats "sess-old" the same way).
        assert_eq!(ids, vec!["sess-b", "sess-a", "sess-other", "sess-old"]);

        let scoped = index
            .list_sessions(ListOpts {
                workspace_ids: Some(&["wd_other_000000000000".to_string()]),
                ..ListOpts::default()
            })
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "sess-other");
    }

    /// Given one archived and one live row, When listing (and counting)
    /// without the opt-in, Then the archived row is excluded — and when the
    /// listing opts in, Then it is included.
    #[test]
    fn should_filter_archived_rows_when_listing_without_the_opt_in() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let live = session_row("sess-live", now);
        let mut archived = session_row("sess-archived", now);
        archived.archived = true;
        index.upsert_session(&live).unwrap();
        index.upsert_session(&archived).unwrap();

        let plain = index.list_sessions(ListOpts::default()).unwrap();
        assert_eq!(
            plain.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["sess-live"]
        );

        let opted_in = index
            .list_sessions(ListOpts {
                include_archived: true,
                ..ListOpts::default()
            })
            .unwrap();
        assert_eq!(opted_in.len(), 2);
    }

    /// Given one archived and two live rows, When counting active sessions,
    /// Then only the non-archived rows are counted, scoped by workspace
    /// when asked.
    #[test]
    fn should_exclude_archived_rows_when_counting_active_sessions() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let mut other = session_row("sess-other", now);
        other.workspace_id = "wd_other_000000000000".to_string();
        let mut archived = session_row("sess-archived", now);
        archived.archived = true;
        for row in [
            &session_row("sess-live-1", now),
            &session_row("sess-live-2", now),
            &archived,
            &other,
        ] {
            index.upsert_session(row).unwrap();
        }

        assert_eq!(index.count_active(None).unwrap(), 3);
        let scoped = ["wd_project_abc123def456".to_string()];
        assert_eq!(index.count_active(Some(&scoped)).unwrap(), 2);
    }

    /// Given three rows, When they are listed with limit and offset, Then
    /// the page slices the `updated_at` DESC order (the TS pagination
    /// semantics).
    #[test]
    fn should_page_rows_when_listing_with_limit_and_offset() {
        let (index, _dir) = open_index();
        let base = Utc::now();
        for (n, id) in ["sess-1", "sess-2", "sess-3"].iter().enumerate() {
            let mut row = session_row(id, base + chrono::Duration::seconds(n as i64));
            if n == 0 {
                row.updated_at = base;
            }
            index.upsert_session(&row).unwrap();
        }

        let page = index
            .list_sessions(ListOpts {
                limit: Some(1),
                offset: 1,
                ..ListOpts::default()
            })
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "sess-2");
    }

    /// Given an upserted row, When it is deleted and re-fetched, Then it is
    /// gone from every read (the orphan-pass primitive).
    #[test]
    fn should_remove_the_row_when_deleting_a_session() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        index
            .upsert_session(&session_row("sess-gone", now))
            .unwrap();

        index.delete_session("sess-gone").unwrap();

        assert!(index.get_session("sess-gone").is_none());
        assert!(index.list_all_rows().unwrap().is_empty());
    }

    /// Given upserted rows, When the reconcile references are listed, Then
    /// every row's id, workspace, and `state_mtime` come back — the inputs
    /// the incremental converge compares.
    #[test]
    fn should_list_every_row_reference_for_the_reconcile() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        index.upsert_session(&session_row("sess-1", now)).unwrap();

        let refs = index.list_all_rows().unwrap();

        assert_eq!(
            refs,
            vec![SessionRowRef {
                id: "sess-1".to_string(),
                workspace_id: "wd_project_abc123def456".to_string(),
                state_mtime: Some(1_700_000_000_000),
            }]
        );
    }

    /// Given two workspace rows with different `sort_order` and recency,
    /// When they are listed, Then the order is `sort_order` ASC first and
    /// `last_opened_at` DESC within the same order.
    #[test]
    fn should_order_workspaces_by_sort_order_then_recency_when_listing() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let pinned = WorkspaceRow {
            id: "wd_pinned_000000000000".to_string(),
            root: Some("/pinned".to_string()),
            name: Some("pinned".to_string()),
            created_at: now,
            last_opened_at: now - chrono::Duration::days(1),
            sort_order: -1,
        };
        let recent = WorkspaceRow {
            id: "wd_recent_000000000000".to_string(),
            root: None,
            name: None,
            created_at: now,
            last_opened_at: now,
            sort_order: 0,
        };
        let stale = WorkspaceRow {
            id: "wd_stale_000000000000".to_string(),
            root: None,
            name: None,
            created_at: now,
            last_opened_at: now - chrono::Duration::days(30),
            sort_order: 0,
        };
        index.upsert_workspace(&stale).unwrap();
        index.upsert_workspace(&recent).unwrap();
        index.upsert_workspace(&pinned).unwrap();

        let listed = index.list_workspaces().unwrap();
        let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "wd_pinned_000000000000",
                "wd_recent_000000000000",
                "wd_stale_000000000000"
            ]
        );

        assert_eq!(index.get_workspace("wd_recent_000000000000"), Some(recent));
        assert!(index.get_workspace("wd_missing_000000000000").is_none());
    }

    /// Given a `workspaces.json` registry beside a not-yet-opened database,
    /// When the index is opened, Then the registry's workspaces are seeded
    /// into the table — and re-opening never clobbers a `sort_order` a later
    /// upsert wrote.
    #[test]
    fn should_seed_workspaces_from_the_registry_without_clobbering_sort_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(WORKSPACE_REGISTRY_FILENAME),
            r#"{"version":1,"workspaces":{"wd_project_abc123def456":{"root":"/Users/dev/project","name":"project","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-06-01T00:00:00.000Z"}},"deleted_workspace_ids":[]}"#,
        )
        .unwrap();

        let index = SessionIndex::open(&dir.path().join("session-index.db")).unwrap();
        let seeded = index
            .get_workspace("wd_project_abc123def456")
            .expect("the registry entry is seeded");
        assert_eq!(seeded.root.as_deref(), Some("/Users/dev/project"));
        assert_eq!(seeded.sort_order, 0);

        // A manual reorder survives a re-open (the registry carries no
        // sort_order — the database's value wins).
        let mut reordered = seeded.clone();
        reordered.sort_order = 7;
        index.upsert_workspace(&reordered).unwrap();
        let index = SessionIndex::open(&dir.path().join("session-index.db")).unwrap();
        assert_eq!(
            index
                .get_workspace("wd_project_abc123def456")
                .expect("still seeded")
                .sort_order,
            7
        );
    }

    /// Given an upserted workspace, When the registry file is read back, Then
    /// the write-through landed in the TS on-disk shape.
    #[test]
    fn should_write_through_to_the_registry_when_upserting_a_workspace() {
        let (index, dir) = open_index();
        let now = Utc::now();
        index
            .upsert_workspace(&WorkspaceRow {
                id: "wd_project_abc123def456".to_string(),
                root: Some("/Users/dev/project".to_string()),
                name: Some("project".to_string()),
                created_at: now,
                last_opened_at: now,
                sort_order: 0,
            })
            .unwrap();

        let registry_rows = read_workspace_registry(&dir.path().join(WORKSPACE_REGISTRY_FILENAME));
        assert_eq!(registry_rows.len(), 1);
        assert_eq!(registry_rows[0].id, "wd_project_abc123def456");
        assert_eq!(registry_rows[0].name.as_deref(), Some("project"));
    }

    /// Given a row a TS engine wrote to `workspaces.json` that the database
    /// never saw, When a rust upsert mirrors through, Then the TS row
    /// survives (the W9 fix: the mirror is a D8 merged write — a plain
    /// whole-document mirror would drop it, violating design 08 §7.3's
    /// "this engine's writes never lose an already-seen row").
    #[test]
    fn should_keep_a_foreign_registry_row_when_the_mirror_writes_through() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(WORKSPACE_REGISTRY_FILENAME),
            r#"{"version":1,"workspaces":{"wd_ts_engine_00000001":{"root":"/ts/engine/row","name":"ts-engine","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":[]}"#,
        )
        .unwrap();

        let index = SessionIndex::open(&dir.path().join("session-index.db")).unwrap();
        let now = Utc::now();
        index
            .upsert_workspace(&WorkspaceRow {
                id: "wd_rust_engine_000001".to_string(),
                root: Some("/rust/engine/row".to_string()),
                name: Some("rust-engine".to_string()),
                created_at: now,
                last_opened_at: now,
                sort_order: 0,
            })
            .unwrap();

        let registry_rows = read_workspace_registry(&dir.path().join(WORKSPACE_REGISTRY_FILENAME));
        let mut ids: Vec<&str> = registry_rows.iter().map(|row| row.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["wd_rust_engine_000001", "wd_ts_engine_00000001"],
            "the merged mirror keeps the TS-written row it never saw in the database"
        );
    }

    // -----------------------------------------------------------------------
    // W6 — the unregister path's index deletions
    // -----------------------------------------------------------------------

    /// Given an upserted workspace row, When the workspace is deleted, Then
    /// the database row is gone (the registry tombstone keeps the id from
    /// re-seeding on the next open) while the workspace's session rows are
    /// untouched (remove preserves data; purge deletes them explicitly).
    #[test]
    fn should_remove_only_the_workspace_row_when_deleting_a_workspace() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        index
            .upsert_workspace(&WorkspaceRow {
                id: "wd_project_abc123def456".to_string(),
                root: Some("/Users/dev/project".to_string()),
                name: Some("project".to_string()),
                created_at: now,
                last_opened_at: now,
                sort_order: 0,
            })
            .unwrap();
        index.upsert_session(&session_row("sess-1", now)).unwrap();

        index.delete_workspace("wd_project_abc123def456").unwrap();

        assert!(index.get_workspace("wd_project_abc123def456").is_none());
        // The session row survives — deleting it is the purge mode's separate
        // explicit step, never a side effect of the workspace-row delete.
        assert!(index.get_session("sess-1").is_some());
        // An unknown id is a no-op, not an error.
        index.delete_workspace("wd_unknown_0000000000").unwrap();
    }

    /// Given session rows across two workspaces (an alias group and an
    /// unrelated one), When the workspace's sessions are bulk-deleted, Then
    /// exactly the addressed buckets' rows are removed and the count of
    /// removals is returned (the purge result's `deletedSessions`).
    #[test]
    fn should_delete_only_the_addressed_workspaces_sessions_in_bulk() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let mut split_bucket = session_row("sess-split", now);
        split_bucket.workspace_id = "wd_split_000000000001".to_string();
        let mut other = session_row("sess-other", now);
        other.workspace_id = "wd_other_000000000000".to_string();
        for row in [
            session_row("sess-1", now),
            session_row("sess-2", now),
            split_bucket,
            other,
        ] {
            index.upsert_session(&row).unwrap();
        }

        let removed = index
            .delete_sessions_for_workspaces(&[
                "wd_project_abc123def456".to_string(),
                "wd_split_000000000001".to_string(),
            ])
            .unwrap();

        assert_eq!(removed, 3, "the two bucket's rows are all removed");
        assert_eq!(
            index
                .list_sessions(ListOpts::default())
                .unwrap()
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sess-other"],
            "the unrelated workspace's rows survive"
        );
        // An empty slice is a no-op.
        assert_eq!(index.delete_sessions_for_workspaces(&[]).unwrap(), 0);
    }

    // -----------------------------------------------------------------------
    // W5 — last_prompt migration, cursors, exclude_empty
    // -----------------------------------------------------------------------

    /// Given a pre-W5 (v0) database file with rows and no `last_prompt`
    /// column, When the index is opened, Then the migration chain steps
    /// it fully current (v0→v1→v2): the column exists, the old rows read
    /// back with `last_prompt` NULL, and a re-open is a no-op (never a
    /// second ALTER).
    #[test]
    fn should_migrate_a_v0_database_to_v1_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id          TEXT PRIMARY KEY NOT NULL,
                    workspace_id TEXT NOT NULL,
                    cwd         TEXT,
                    title       TEXT,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    archived    INTEGER NOT NULL DEFAULT 0,
                    tags        TEXT,
                    custom      TEXT,
                    state_mtime INTEGER
                );",
            )
            .unwrap();
            let updated_at = Utc::now().format(TIMESTAMP_FORMAT).to_string();
            conn.execute(
                "INSERT INTO sessions (id, workspace_id, created_at, updated_at)
                 VALUES ('sess-old', 'wd_project_abc123def456', ?1, ?1)",
                [&updated_at],
            )
            .unwrap();
        }

        let index = SessionIndex::open(&db_path).unwrap();

        let row = index.get_session("sess-old").expect("the old row survived");
        assert_eq!(row.last_prompt, None);
        let version: i64 = {
            let conn = index.lock();
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, i64::from(SCHEMA_VERSION));

        // A re-open never re-runs the step (the ALTER would fail outright —
        // this doubling as the idempotence assertion).
        let reopened = SessionIndex::open(&db_path).unwrap();
        assert!(reopened.get_session("sess-old").is_some());
        // And a new write carries last_prompt through the migrated column.
        let now = Utc::now();
        let mut fresh = session_row("sess-new", now);
        fresh.last_prompt = Some("hello".to_string());
        index.upsert_session(&fresh).unwrap();
        assert_eq!(
            index
                .get_session("sess-new")
                .unwrap()
                .last_prompt
                .as_deref(),
            Some("hello")
        );
    }

    /// Given a fresh database, When it is opened, Then the schema is already
    /// v1-shaped and the version stamp is 1 (the fresh path never runs the
    /// ALTER — only a genuinely pre-W5 file does).
    #[test]
    fn should_stamp_a_fresh_database_at_the_current_schema_version() {
        let (index, _dir) = open_index();
        let version: i64 = {
            let conn = index.lock();
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, i64::from(SCHEMA_VERSION));
        // And the column order the SCHEMA spells matches what a migrated
        // v0 database would have (the v1 `last_prompt` tail followed by
        // the v2 retention columns).
        let columns: Vec<String> = {
            let conn = index.lock();
            let mut statement = conn
                .prepare("SELECT name FROM pragma_table_info('sessions')")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        let tail: Vec<&str> = columns.iter().rev().take(4).map(String::as_str).collect();
        assert_eq!(
            tail,
            vec!["compressed_at", "compressed", "archived_at", "last_prompt"]
        );
    }

    /// Given three rows in the global recency order, When listing with a
    /// `before_id` cursor, Then the page starts strictly after the cursor's
    /// position — and `after_id` answers the strictly newer head.
    #[test]
    fn should_page_with_keyset_cursors_when_listing() {
        let (index, _dir) = open_index();
        let base = Utc::now();
        for (n, id) in ["sess-a", "sess-b", "sess-c"].iter().enumerate() {
            index
                .upsert_session(&session_row(id, base + chrono::Duration::seconds(n as i64)))
                .unwrap();
        }
        // Order: sess-c (newest), sess-b, sess-a.

        let forward = index
            .list_sessions(ListOpts {
                before_id: Some("sess-b"),
                ..ListOpts::default()
            })
            .unwrap();
        assert_eq!(
            forward
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sess-a"]
        );

        let backward = index
            .list_sessions(ListOpts {
                after_id: Some("sess-b"),
                ..ListOpts::default()
            })
            .unwrap();
        assert_eq!(
            backward
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["sess-c"]
        );

        // An unknown cursor is the terminal empty page, never an error.
        let unknown = index
            .list_sessions(ListOpts {
                before_id: Some("no-such-session"),
                ..ListOpts::default()
            })
            .unwrap();
        assert!(unknown.is_empty());
    }

    /// Given one never-conversed row (NULL / empty last_prompt) beside
    /// conversed ones, When listing with `exclude_empty`, Then only the rows
    /// that carry a non-empty `last_prompt` answer.
    #[test]
    fn should_exclude_never_conversed_rows_when_listing_with_exclude_empty() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let mut null_prompt = session_row("sess-null", now);
        null_prompt.last_prompt = None;
        let mut empty_prompt = session_row("sess-empty", now);
        empty_prompt.last_prompt = Some(String::new());
        index.upsert_session(&null_prompt).unwrap();
        index.upsert_session(&empty_prompt).unwrap();
        index
            .upsert_session(&session_row("sess-talked", now))
            .unwrap();

        let listed = index
            .list_sessions(ListOpts {
                exclude_empty: true,
                ..ListOpts::default()
            })
            .unwrap();

        assert_eq!(
            listed.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["sess-talked"]
        );
    }

    /// Given rows of two workspace buckets, When the distinct (workspace,
    /// cwd) pairs are read, Then each bucket appears exactly once with its
    /// cwd — the alias folding's index arm.
    #[test]
    fn should_read_one_cwd_per_workspace_bucket_when_folding_aliases() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        for id in ["sess-1", "sess-2"] {
            index.upsert_session(&session_row(id, now)).unwrap();
        }
        let mut other = session_row("sess-other", now);
        other.workspace_id = "wd_project_000000000000".to_string();
        other.cwd = Some("/Users/dev/PROJECT".to_string());
        index.upsert_session(&other).unwrap();

        let mut pairs = index.workspace_id_cwds().unwrap();
        pairs.sort();

        assert_eq!(
            pairs,
            vec![
                (
                    "wd_project_000000000000".to_string(),
                    Some("/Users/dev/PROJECT".to_string())
                ),
                (
                    "wd_project_abc123def456".to_string(),
                    Some("/Users/dev/project".to_string())
                ),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // Retention domain (§3.1 / ADR-006) — archived_at / compressed /
    // compressed_at columns
    // -----------------------------------------------------------------------

    /// Given a row carrying the retention stamps, When it is upserted then
    /// fetched (and listed), Then every stamp round-trips — the scan's
    /// candidate inputs survive the SQLite boundary.
    #[test]
    fn should_roundtrip_the_retention_columns_when_upserting_then_getting_a_session_row() {
        let (index, _dir) = open_index();
        let now = Utc::now();
        let mut row = session_row("sess-retention", now);
        row.archived_at = Some(now - chrono::Duration::days(40));
        row.compressed = true;
        row.compressed_at = Some(now - chrono::Duration::days(39));
        index.upsert_session(&row).unwrap();

        let fetched = index.get_session("sess-retention").expect("the row is present");
        assert_eq!(fetched, row);

        // And the listing path carries the columns through the same SELECT.
        let listed = index.list_sessions(ListOpts::default()).unwrap();
        assert_eq!(listed, vec![row]);
    }

    /// Given a pre-v2 (v1) database file with rows and no retention
    /// columns, When the index is opened, Then the migration steps it to
    /// v2: the columns exist, the old rows read back with the retention
    /// defaults (`archived_at`/`compressed_at` NULL, `compressed` false),
    /// a re-open is a no-op, and a new write carries the columns through
    /// the migrated table.
    #[test]
    fn should_migrate_a_v1_database_to_v2_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id          TEXT PRIMARY KEY NOT NULL,
                    workspace_id TEXT NOT NULL,
                    cwd         TEXT,
                    title       TEXT,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    archived    INTEGER NOT NULL DEFAULT 0,
                    tags        TEXT,
                    custom      TEXT,
                    state_mtime INTEGER,
                    last_prompt TEXT
                );",
            )
            .unwrap();
            let updated_at = Utc::now().format(TIMESTAMP_FORMAT).to_string();
            conn.execute(
                "INSERT INTO sessions (id, workspace_id, created_at, updated_at)
                 VALUES ('sess-old', 'wd_project_abc123def456', ?1, ?1)",
                [&updated_at],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let index = SessionIndex::open(&db_path).unwrap();

        let row = index.get_session("sess-old").expect("the old row survived");
        assert_eq!(row.archived_at, None);
        assert!(!row.compressed);
        assert_eq!(row.compressed_at, None);
        let version: i64 = {
            let conn = index.lock();
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, 2);

        // A re-open never re-runs the step (the second ALTER would fail
        // outright — this doubling as the idempotence assertion).
        let reopened = SessionIndex::open(&db_path).unwrap();
        assert!(reopened.get_session("sess-old").is_some());

        // And a new write carries the retention columns through the
        // migrated table.
        let now = Utc::now();
        let mut fresh = session_row("sess-new", now);
        fresh.archived_at = Some(now);
        fresh.compressed = true;
        fresh.compressed_at = Some(now);
        index.upsert_session(&fresh).unwrap();
        assert_eq!(
            index.get_session("sess-new").unwrap().archived_at,
            Some(now),
            "the migrated table stores the archive stamp"
        );
        assert!(index.get_session("sess-new").unwrap().compressed);
    }

    /// Given a fresh database, When it is opened, Then the schema is already
    /// v2-shaped and the version stamp is 2 (the fresh path never runs the
    /// ALTERs — the column order matches what the migration chain produces).
    #[test]
    fn should_stamp_a_fresh_database_at_the_v2_schema_version() {
        let (index, _dir) = open_index();
        let version: i64 = {
            let conn = index.lock();
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(version, i64::from(SCHEMA_VERSION));
        // And the column order the SCHEMA spells matches what a migrated
        // v1 database would have (the retention columns last, in order).
        let columns: Vec<String> = {
            let conn = index.lock();
            let mut statement = conn
                .prepare("SELECT name FROM pragma_table_info('sessions')")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        let tail: Vec<&str> = columns
            .iter()
            .rev()
            .take(3)
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            vec!["compressed_at", "compressed", "archived_at"],
            "the retention columns land last, in the migration chain's order"
        );
    }
}
