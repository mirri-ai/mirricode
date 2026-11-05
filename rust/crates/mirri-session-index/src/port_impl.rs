//! The SQLite implementation of [`MetadataStorePort`] — ADR-006's production
//! metadata store.
//!
//! [`SessionIndexPort`] wraps the shared [`SessionIndex`] connection and
//! projects its `sessions` row onto the port's semantics:
//!
//! - **`apply`** compiles, per patch, an `INSERT … ON CONFLICT(id) DO UPDATE`
//!   whose insert branch spells all 19 columns and whose update branch SETs
//!   **only** the fields the patch names (`None` = skip, `Some(None)` = SET
//!   NULL, `Some(Some(v))` = SET value — the tri-state overlay of
//!   `MetadataPatch`). The inserting `apply` stamps `created_at`; the update
//!   branch never names `created_at`, so no later patch can move it.
//! - **`delete`** removes the row and records the tombstone in **one**
//!   transaction (a crash can never leave the deletion ledger blind).
//! - Row existence is the only existence judgement — [`MetadataStorePort::exists`]
//!   and [`list_ids`] never touch the session directory.
//!
//! ## Async shape
//!
//! `SessionIndex` is a synchronous rusqlite connection behind a mutex; the
//! port is an `#[async_trait]` object shared into async handlers. Every
//! method runs the storage call inline — the statements are all
//! sub-millisecond and the connection mutex serializes writers, so the
//! executor is blocked only for one quick call (`block_in_place` would
//! panic on the single-threaded runtime every `#[tokio::test]` defaults
//! to, which the shared test fixtures exercise heavily).
//!
//! [`list_ids`]: MetadataStorePort::list_ids

use std::sync::Arc;

use mirri_core::scope::SessionId;
use mirri_core::session::metadata_port::{
    MetadataPatch, MetadataStoreError, MetadataStorePort, SessionMetadataRecord,
};

use crate::{SessionIndex, encode_timestamp};

impl SessionIndex {
    /// Open the index with the ADR-006 startup self-check (裁决 2: a corrupt
    /// database is **reported**, never silently rebuilt and never migrated on
    /// top of).
    ///
    /// The order is deliberate:
    ///
    /// 1. `PRAGMA integrity_check` on the raw file — anything but `ok` is
    ///    [`OpenOutcome::Corrupt`] with the first diagnosis line.
    /// 2. The `meta` table gate — a database that carries one states its
    ///    `schema_format` era. A **newer** era than this build's floor is
    ///    [`OpenOutcome::SchemaFloor`] (a downgrade would corrupt it).
    ///    No `meta` table is an old-era or fresh database: `Ok`, and the
    ///    caller decides whether to migrate (the migration chain itself is
    ///    expand-only and safe to run).
    /// 3. The plain [`SessionIndex::open`] (schema + migrations + seeding).
    pub fn open_with_check(path: &std::path::Path) -> anyhow::Result<OpenOutcome> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        // A corrupt page can make even this pragma fail outright — the error
        // text is the diagnosis, same as a non-ok verdict.
        let mut statement = match conn.prepare("PRAGMA integrity_check") {
            Ok(statement) => statement,
            Err(error) => return Ok(OpenOutcome::Corrupt(error.to_string())),
        };
        let mut rows = match statement.query([]) {
            Ok(rows) => rows,
            Err(error) => return Ok(OpenOutcome::Corrupt(error.to_string())),
        };
        let first = match rows.next() {
            Ok(Some(row)) => row.get::<_, String>(0),
            Err(error) => return Ok(OpenOutcome::Corrupt(error.to_string())),
            // integrity_check always answers at least one row; an empty one
            // is itself an unreadable-file symptom — treat it as corrupt.
            Ok(None) => {
                return Ok(OpenOutcome::Corrupt(
                    "integrity_check returned no rows".into(),
                ));
            }
        };
        // The first row's verdict is the diagnosis; a read failure on it is
        // as unreadable as a non-ok answer.
        let verdict = match first {
            Ok(verdict) => verdict,
            Err(error) => return Ok(OpenOutcome::Corrupt(error.to_string())),
        };
        if verdict != "ok" {
            return Ok(OpenOutcome::Corrupt(verdict));
        }

        let has_meta: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'")?
            .exists([])?;
        if has_meta {
            // A missing key or an unparseable stamp both read as "not above
            // the floor" — only a genuinely newer integer is a floor.
            let schema_format: Option<i64> = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_format'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|value| value.parse().ok());
            if let Some(format) = schema_format
                && format > i64::from(KNOWN_SCHEMA_FORMAT)
            {
                return Ok(OpenOutcome::SchemaFloor {
                    db_version: format as i32,
                });
            }
        }
        drop(rows);
        drop(statement);
        drop(conn);

        Ok(OpenOutcome::Ok(Arc::new(SessionIndex::open(path)?)))
    }
}

/// The outcome of [`SessionIndex::open_with_check`].
#[derive(Debug)]
pub enum OpenOutcome {
    /// The database is usable (or fresh) — the caller decides whether to
    /// migrate.
    Ok(Arc<SessionIndex>),
    /// The file failed the integrity check; the string is SQLite's own first
    /// diagnosis line. Never rebuilt over, never migrated on top of.
    Corrupt(String),
    /// The database's `schema_format` era is newer than this build's floor —
    /// an older binary must refuse rather than strip what it cannot read.
    SchemaFloor {
        /// The database's `meta.schema_format` value.
        db_version: i32,
    },
}

/// The highest `meta.schema_format` era this build knows. `2` is the
/// metadata-source-of-truth era (the port + v4 columns); a database stamped
/// above it was written by a future build.
const KNOWN_SCHEMA_FORMAT: i32 = 2;

/// The SQL binding a touched column carries (the column names themselves
/// are compile-time constants — the whitelist that keeps patch-shaped SQL
/// injection-free).
#[derive(Clone)]
enum PatchValue {
    /// `Some(v)` — bind `v`.
    Set(String),
    /// `Some(None)` — bind NULL (the clear, never a skip).
    Null,
}

impl PatchValue {
    /// Encode one tri-state patch value as its SQL binding shape: an
    /// explicit `Some(None)` is the **clear** (a NULL binding), never a skip.
    fn tri_state(value: Option<String>) -> Self {
        match value {
            Some(value) => PatchValue::Set(value),
            None => PatchValue::Null,
        }
    }

    /// The default a column takes when the patch leaves it alone and the
    /// insert branch must still spell it.
    fn column_default(column: &str) -> Self {
        match column {
            "title" => PatchValue::Set(String::new()),
            "tags" => PatchValue::Set("[]".to_string()),
            "archived" | "compressed" | "pinned" => PatchValue::Set("0".to_string()),
            // Every other column (cwd, last_prompt, last_used_at, archived_at,
            // compressed_at, pre_compressed_size, pinned_at, pin_color) is
            // nullable and defaults to NULL; `updated_at`'s now-fallback is
            // handled by the caller.
            _ => PatchValue::Null,
        }
    }

    fn into_param(self) -> Option<String> {
        match self {
            PatchValue::Set(value) => Some(value),
            PatchValue::Null => None,
        }
    }
}

/// The compiled `apply` statement — the full 17-column INSERT with an
/// `ON CONFLICT(id) DO UPDATE SET <touched columns>` clause whose SET list
/// spells exactly the patch's named columns.
pub(crate) struct PatchSql {
    pub(crate) sql: String,
    /// The parameters in placeholder order — `None` binds NULL (both the
    /// `Some(None)` clear and an untouched nullable column's insert default).
    pub(crate) params: Vec<Option<String>>,
}

/// Compile the patch's SQL — the one place the tri-state overlay becomes
/// column bindings.
///
/// The insert branch always spells all 17 columns: the untouched fields take
/// their storage defaults (`title` `''`, `tags` `'[]'`, booleans `0`, the
/// rest NULL) and the touched ones carry the patch's value (`updated_at`
/// defaults to now — a session is inserted as "just modified"). The update
/// branch SETs **exactly** the touched columns — including
/// `col = excluded.col` where the insert bound NULL for `Some(None)`, which
/// is the whole point of the tri-state patch: a clear is a write of NULL,
/// never a skipped column.
fn compile_patch(patch: &MetadataPatch) -> PatchSql {
    let mut touched: Vec<(&'static str, PatchValue)> = Vec::new();
    if let Some(title) = &patch.title {
        touched.push(("title", PatchValue::Set(title.clone())));
    }
    if let Some(tags) = &patch.tags {
        touched.push((
            "tags",
            PatchValue::Set(serde_json::to_string(tags).expect("a tag list always serializes")),
        ));
    }
    if let Some(archived) = patch.archived {
        touched.push(("archived", PatchValue::Set((archived as i64).to_string())));
    }
    if let Some(archived_at) = patch.archived_at {
        touched.push((
            "archived_at",
            PatchValue::tri_state(archived_at.map(|at| encode_timestamp(&at))),
        ));
    }
    if let Some(compressed) = patch.compressed {
        touched.push((
            "compressed",
            PatchValue::Set((compressed as i64).to_string()),
        ));
    }
    if let Some(compressed_at) = patch.compressed_at {
        touched.push((
            "compressed_at",
            PatchValue::tri_state(compressed_at.map(|at| encode_timestamp(&at))),
        ));
    }
    if let Some(pre_compressed_size) = patch.pre_compressed_size {
        touched.push((
            "pre_compressed_size",
            PatchValue::tri_state(pre_compressed_size.map(|size| size.to_string())),
        ));
    }
    if let Some(updated_at) = patch.updated_at {
        touched.push(("updated_at", PatchValue::Set(encode_timestamp(&updated_at))));
    }
    if let Some(last_used_at) = patch.last_used_at {
        touched.push((
            "last_used_at",
            PatchValue::tri_state(last_used_at.map(|at| encode_timestamp(&at))),
        ));
    }
    if let Some(last_prompt) = &patch.last_prompt {
        touched.push(("last_prompt", PatchValue::tri_state(last_prompt.clone())));
    }
    if let Some(cwd) = &patch.cwd {
        touched.push(("cwd", PatchValue::tri_state(cwd.clone())));
    }
    if let Some(pinned) = patch.pinned {
        touched.push(("pinned", PatchValue::Set((pinned as i64).to_string())));
    }
    if let Some(pinned_at) = patch.pinned_at {
        touched.push((
            "pinned_at",
            PatchValue::tri_state(pinned_at.map(|at| encode_timestamp(&at))),
        ));
    }
    if let Some(pin_color) = &patch.pin_color {
        touched.push(("pin_color", PatchValue::tri_state(pin_color.clone())));
    }

    // The 17 columns every insert spells, in placeholder order — the
    // compile-time whitelist (format! never interpolates a user value into
    // this SQL; the column names and the SET list come from these constants
    // alone, and every user-controlled byte goes through a `?` binding).
    const INSERT_COLUMNS: [&str; 17] = [
        "id",
        "workspace_id",
        "created_at",
        "updated_at",
        "cwd",
        "title",
        "tags",
        "archived",
        "last_prompt",
        "archived_at",
        "compressed",
        "compressed_at",
        "pre_compressed_size",
        "last_used_at",
        "pinned",
        "pinned_at",
        "pin_color",
    ];
    let now = chrono::Utc::now()
        .format(crate::TIMESTAMP_FORMAT)
        .to_string();
    let mut params: Vec<Option<String>> = Vec::with_capacity(INSERT_COLUMNS.len());
    let mut sql = String::from("INSERT INTO sessions (");
    sql.push_str(&INSERT_COLUMNS.join(", "));
    sql.push_str(") VALUES (");
    for (index, column) in INSERT_COLUMNS.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("?{}", index + 1));
        match *column {
            "id" | "workspace_id" => params.push(Some(String::new())), // the caller fills these
            "created_at" => params.push(Some(now.clone())),
            // `updated_at` honors a caller-supplied stamp (the migration
            // replays historical timestamps); only an absent one falls back
            // to the wall clock.
            "updated_at" => params.push(Some(
                touched
                    .iter()
                    .find(|(touched_column, _)| *touched_column == "updated_at")
                    .map(|(_, value)| value.clone())
                    .map_or_else(
                        || PatchValue::Set(now.clone()),
                        |value| match value {
                            PatchValue::Set(stamp) => PatchValue::Set(stamp.clone()),
                            PatchValue::Null => PatchValue::Set(now.clone()),
                        },
                    )
                    .into_param()
                    .expect("updated_at always binds a value"),
            )),
            _ => {
                let value = touched
                    .iter()
                    .find(|(touched_column, _)| touched_column == column)
                    .map_or_else(
                        || PatchValue::column_default(column),
                        |(_, value)| value.clone(),
                    );
                params.push(value.into_param());
            }
        }
    }
    sql.push(')');

    if !touched.is_empty() {
        sql.push_str(" ON CONFLICT(id) DO UPDATE SET ");
        let sets: Vec<String> = touched
            .iter()
            .map(|(column, _)| format!("{column} = excluded.{column}"))
            .collect();
        sql.push_str(&sets.join(", "));
    }
    PatchSql { sql, params }
}

/// Compile a patch with the fixed id/workspace bindings filled in — the
/// [`SessionIndex::apply_session_patch`] face (the same statement
/// [`SessionIndexPort::apply`] builds, exposed for the embedded
/// write-throughs that hold the index but not the port).
pub(crate) fn compile_patch_for(
    session_id: &str,
    workspace_id: &str,
    patch: &MetadataPatch,
) -> PatchSql {
    let mut compiled = compile_patch(patch);
    compiled.params[0] = Some(session_id.to_string());
    compiled.params[1] = Some(workspace_id.to_string());
    compiled
}

/// The `SessionIndex`-backed [`MetadataStorePort`].
///
/// The async faces run the synchronous storage calls inline — the
/// statements are all sub-millisecond and `SessionIndex` serializes its
/// one connection behind a mutex, so the executor is blocked only for a
/// single quick call (and `block_in_place` would panic on the
/// single-threaded test runtime, which every `#[tokio::test]` defaults
/// to). Callers stay free to `.await` inside handlers as before.
#[derive(Debug)]
pub struct SessionIndexPort(pub Arc<SessionIndex>);

#[async_trait::async_trait]
impl MetadataStorePort for SessionIndexPort {
    async fn exists(&self, session_id: &SessionId) -> bool {
        self.0.get_session(session_id.as_str()).is_some()
    }

    async fn get(&self, session_id: &SessionId) -> Option<SessionMetadataRecord> {
        self.0
            .get_session(session_id.as_str())
            .map(record_from_row)
    }

    async fn apply(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        touched: MetadataPatch,
    ) -> Result<(), MetadataStoreError> {
        self.apply_sync(session_id, workspace_id, &touched)
    }

    async fn delete(&self, session_id: &SessionId) -> Result<(), MetadataStoreError> {
        self.delete_sync(session_id)
    }

    async fn apply_seed(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        record: SessionMetadataRecord,
    ) -> Result<(), MetadataStoreError> {
        self.seed_sync(session_id, workspace_id, record)
    }

    async fn list_ids(&self, workspace_id: &str) -> Vec<String> {
        let mut ids = self.0.list_all_rows().unwrap_or_default();
        ids.retain(|r| r.workspace_id == workspace_id);
        let mut ids: Vec<String> = ids.into_iter().map(|r| r.id).collect();
        ids.sort();
        ids
    }
}

impl SessionIndexPort {
    /// The synchronous apply — one compiled INSERT … ON CONFLICT statement.
    fn apply_sync(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        touched: &MetadataPatch,
    ) -> Result<(), MetadataStoreError> {
        let mut compiled = compile_patch(touched);
        // The fixed prefix's id/workspace bindings, filled in place (the
        // placeholder numbering already counts them).
        compiled.params[0] = Some(session_id.as_str().to_string());
        compiled.params[1] = Some(workspace_id.to_string());
        let refs: Vec<&dyn rusqlite::ToSql> = compiled
            .params
            .iter()
            .map(|param| param as &dyn rusqlite::ToSql)
            .collect();
        self.0
            .apply_patch_sql(&compiled.sql, refs.as_slice())
            .map_err(|error| MetadataStoreError::Storage(error.to_string()))
    }

    /// The synchronous delete — row and tombstone in one transaction.
    fn delete_sync(&self, session_id: &SessionId) -> Result<(), MetadataStoreError> {
        self.0
            .delete_session_with_tombstone(session_id.as_str())
            .map_err(|error| MetadataStoreError::Storage(error.to_string()))
    }

    /// The synchronous seed — the whole row (caller-supplied `created_at`
    /// included) through the same single-row upsert SQL every write
    /// funnels through, inside one transaction. The inverse of
    /// [`record_from_row`]: the record maps back onto the row shape.
    fn seed_sync(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        record: SessionMetadataRecord,
    ) -> Result<(), MetadataStoreError> {
        self.0
            .seed_session(&crate::SessionRow {
                id: session_id.as_str().to_string(),
                workspace_id: workspace_id.to_string(),
                cwd: record.cwd,
                title: Some(record.title),
                created_at: record.created_at,
                updated_at: record.updated_at,
                archived: record.archived,
                tags: record.tags,
                custom: None,
                last_prompt: record.last_prompt,
                state_mtime: None,
                archived_at: record.archived_at,
                compressed: record.compressed,
                compressed_at: record.compressed_at,
                pinned: record.pinned,
                pinned_at: record.pinned_at,
                pin_color: record.pin_color,
                last_used_at: record.last_used_at,
                pre_compressed_size: record
                    .pre_compressed_size
                    .and_then(|size| i64::try_from(size).ok()),
            })
            .map_err(|error| MetadataStoreError::Storage(error.to_string()))
    }
}

/// Project one storage row onto the port's read-back record.
///
/// A row whose `created_at`/`updated_at` no longer parse is already read as
/// absent by the storage layer's mapper (a corrupt row is a cold miss, not a
/// port failure); the projection here therefore only fails loudly on
/// genuinely impossible shapes.
fn record_from_row(row: crate::SessionRow) -> SessionMetadataRecord {
    SessionMetadataRecord {
        title: row.title.unwrap_or_default(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        last_used_at: row.last_used_at,
        archived: row.archived,
        archived_at: row.archived_at,
        compressed: row.compressed,
        compressed_at: row.compressed_at,
        pre_compressed_size: row
            .pre_compressed_size
            .map(|size| u64::try_from(size).unwrap_or(0)),
        tags: row.tags,
        last_prompt: row.last_prompt,
        cwd: row.cwd,
        pinned: row.pinned,
        pinned_at: row.pinned_at,
        pin_color: row.pin_color,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::{TimeZone, Utc};

    use super::*;

    fn open_port() -> (SessionIndexPort, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let outcome = SessionIndex::open_with_check(&dir.path().join("session-index.db")).unwrap();
        let OpenOutcome::Ok(index) = outcome else {
            panic!("a fresh database opens Ok, not {outcome:?}");
        };
        (SessionIndexPort(index), dir)
    }

    fn session_id(id: &str) -> SessionId {
        SessionId::new(id)
    }

    fn title_only(title: &str) -> MetadataPatch {
        MetadataPatch {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    /// Given a byte-truncated database file, When it is opened with the
    /// self-check, Then the outcome is `Corrupt` with SQLite's own diagnosis —
    /// never a panic, never a silent rebuild.
    #[test]
    fn should_return_corrupt_when_integrity_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        {
            let index = SessionIndex::open(&db_path).unwrap();
            index
                .upsert_session(&crate::SessionRow {
                    id: "sess-1".to_string(),
                    workspace_id: "wd_project_abc123def456".to_string(),
                    cwd: None,
                    title: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    archived: false,
                    tags: vec![],
                    custom: None,
                    last_prompt: None,
                    state_mtime: None,
                    archived_at: None,
                    compressed: false,
                    compressed_at: None,
                    pinned: false,
                    pinned_at: None,
                    pin_color: None,
                    last_used_at: None,
                    pre_compressed_size: None,
                })
                .unwrap();
        }
        let whole = std::fs::read(&db_path).unwrap();
        std::fs::write(&db_path, &whole[..whole.len() / 2]).unwrap();

        match SessionIndex::open_with_check(&db_path).unwrap() {
            OpenOutcome::Corrupt(diagnosis) => {
                assert!(
                    !diagnosis.is_empty(),
                    "the verdict carries SQLite's diagnosis"
                );
            }
            other => panic!("a truncated file is Corrupt, not {other:?}"),
        }
    }

    /// Given a fresh (absent) database file, When it is opened with the
    /// self-check, Then the outcome is `Ok` — a fresh database carries no
    /// `meta` table and is never judged against the schema floor.
    #[test]
    fn should_open_ok_when_the_database_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = SessionIndex::open_with_check(&dir.path().join("session-index.db")).unwrap();
        assert!(matches!(outcome, OpenOutcome::Ok(_)));
    }

    /// Given a database whose `meta.schema_format` is above this build's
    /// floor, When it is opened with the self-check, Then the outcome is
    /// `SchemaFloor` carrying the stamped version — an older binary refuses
    /// rather than strips.
    #[test]
    fn should_return_schema_floor_when_the_database_is_from_a_newer_era() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        {
            let _ = SessionIndex::open(&db_path).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_format', '99')",
                [],
            )
            .unwrap();
        }

        match SessionIndex::open_with_check(&db_path).unwrap() {
            OpenOutcome::SchemaFloor { db_version } => assert_eq!(db_version, 99),
            other => panic!("a newer-era database is SchemaFloor, not {other:?}"),
        }
    }

    /// Given two patches touching disjoint fields, When both apply, Then both
    /// are visible in the read-back — the SQLite mirror of the in-memory
    /// port-semantics test (neither write reverts the other).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_only_touched_fields_when_patched() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        let stamp = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();

        port.apply(&id, "ws-1", title_only("Renamed"))
            .await
            .unwrap();
        port.apply(
            &id,
            "ws-1",
            MetadataPatch {
                archived: Some(true),
                archived_at: Some(Some(stamp)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let record = port.get(&id).await.expect("the row exists");
        assert_eq!(record.title, "Renamed");
        assert!(record.archived);
        assert_eq!(record.archived_at, Some(stamp));
    }

    /// Given a patch against a row that does not exist, When it applies,
    /// Then the row is inserted — session creation is the first `apply`, so
    /// the create path needs no separate method.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_insert_row_when_apply_targets_missing_row() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        assert!(!port.exists(&id).await);

        port.apply(&id, "ws-1", title_only("Fresh")).await.unwrap();

        assert!(port.exists(&id).await, "apply creates the missing row");
        assert_eq!(port.list_ids("ws-1").await, vec!["sess-1".to_string()]);
        let record = port.get(&id).await.expect("the row exists");
        assert_eq!(record.title, "Fresh");
    }

    /// Given an inserted row, When a later patch applies, Then `created_at`
    /// never moves — it is stamped by the inserting write alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_created_at_when_a_later_apply_targets_the_row() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        port.apply(&id, "ws-1", title_only("First")).await.unwrap();
        let created_at = port.get(&id).await.unwrap().created_at;

        port.apply(&id, "ws-1", title_only("Second")).await.unwrap();

        assert_eq!(port.get(&id).await.unwrap().created_at, created_at);
    }

    /// Given a seed carrying a HISTORICAL `created_at` (the one-time
    /// migration's replay of the legacy document's birthday), When the row
    /// is seeded, Then the read-back carries exactly that stamp — never a
    /// fresh "now" — and a later `apply` does not move it (fix round 1,
    /// I-1: the SQLite insert branch of `apply` hard-codes `created_at =
    /// now`, which the seed entry exists to bypass).
    #[tokio::test(flavor = "multi_thread")]
    async fn should_seed_the_callers_created_at_verbatim() {
        let (port, _dir) = open_port();
        let id = session_id("sess-legacy");
        let birthday = Utc.with_ymd_and_hms(2020, 5, 4, 3, 2, 1).unwrap();

        port.apply_seed(
            &id,
            "ws-legacy",
            SessionMetadataRecord {
                title: "legacy title".to_string(),
                created_at: birthday,
                updated_at: birthday,
                last_used_at: None,
                archived: false,
                archived_at: None,
                compressed: false,
                compressed_at: None,
                pre_compressed_size: None,
                tags: vec!["legacy".to_string()],
                last_prompt: None,
                cwd: Some("/w/legacy".to_string()),
                pinned: false,
                pinned_at: None,
                pin_color: None,
            },
        )
        .await
        .unwrap();

        let record = port.get(&id).await.expect("the seeded row exists");
        assert_eq!(
            record.created_at, birthday,
            "the SQLite seed carries the historical created_at verbatim"
        );
        assert_eq!(record.title, "legacy title");
        assert_eq!(record.cwd.as_deref(), Some("/w/legacy"));

        // And a later patch never moves the seeded birthday.
        port.apply(&id, "ws-legacy", title_only("Renamed after seed"))
            .await
            .unwrap();
        let record = port.get(&id).await.expect("the row exists");
        assert_eq!(record.created_at, birthday);
        assert_eq!(record.title, "Renamed after seed");
    }

    /// Given a patch carrying an explicit `updated_at` (a historical stamp,
    /// e.g. the migration replaying `state.json` timestamps), When it applies,
    /// Then the read-back carries exactly that stamp — not "now" — and a
    /// later patch without one falls back to the wall clock. The insert
    /// branch must honor the caller's value the same way the in-memory
    /// store's overlay does.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_persist_caller_supplied_updated_at_when_apply_carries_one() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        let historical = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();

        port.apply(
            &id,
            "ws-1",
            MetadataPatch {
                updated_at: Some(historical),
                title: Some("Migrated".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            port.get(&id).await.expect("the row exists").updated_at,
            historical,
            "a caller-supplied updated_at is persisted, not reset to now"
        );

        // And a later patch that does not name `updated_at` leaves it exactly
        // as it was — `None` is the tri-state "skip", not "reset to now".
        port.apply(&id, "ws-1", title_only("Later")).await.unwrap();
        let record = port.get(&id).await.expect("the row exists");
        assert_eq!(
            record.updated_at, historical,
            "an unnamed updated_at is skipped, never reset"
        );
        assert_eq!(record.title, "Later", "the touched title did land");
    }

    /// Given a row whose nullable columns all hold a value, When a patch
    /// carries `Some(None)` — the clear-compressed shape (compressed_at +
    /// pre_compressed_size) and the clear-pin shape (pinned_at + pin_color) —
    /// Then exactly those columns read back as NULL while the untouched ones
    /// keep their values.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_clear_columns_to_null_when_patch_carries_some_none() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        let stamp = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();

        // Given every column holds a value.
        port.apply(
            &id,
            "ws-1",
            MetadataPatch {
                archived_at: Some(Some(stamp)),
                compressed: Some(true),
                compressed_at: Some(Some(stamp)),
                pre_compressed_size: Some(Some(4096)),
                last_used_at: Some(Some(stamp)),
                last_prompt: Some(Some("hello".to_string())),
                cwd: Some(Some("/Users/dev/project".to_string())),
                pinned: Some(true),
                pinned_at: Some(Some(stamp)),
                pin_color: Some(Some("red".to_string())),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // When the clear-compressed shape fires (the retention restore).
        port.apply(
            &id,
            "ws-1",
            MetadataPatch {
                compressed_at: Some(None),
                pre_compressed_size: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let record = port.get(&id).await.expect("the row exists");
        assert_eq!(record.compressed_at, None, "the cleared column is NULL");
        assert_eq!(
            record.pre_compressed_size, None,
            "the cleared column is NULL"
        );
        // The untouched columns keep their values.
        assert!(record.compressed);
        assert_eq!(record.archived_at, Some(stamp));
        assert_eq!(record.last_used_at, Some(stamp));
        assert_eq!(record.last_prompt.as_deref(), Some("hello"));
        assert!(record.pinned);
        assert_eq!(record.pinned_at, Some(stamp));
        assert_eq!(record.pin_color.as_deref(), Some("red"));

        // And when the clear-pin shape fires (the unpin).
        port.apply(
            &id,
            "ws-1",
            MetadataPatch {
                pinned: Some(false),
                pinned_at: Some(None),
                pin_color: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let record = port.get(&id).await.expect("the row exists");
        assert!(!record.pinned);
        assert_eq!(record.pinned_at, None, "the cleared column is NULL");
        assert_eq!(record.pin_color, None, "the cleared column is NULL");
        // The earlier clear stays cleared, and the untouched stay untouched.
        assert_eq!(record.compressed_at, None);
        assert_eq!(record.pre_compressed_size, None);
        assert_eq!(record.cwd.as_deref(), Some("/Users/dev/project"));
    }

    /// Given an existing row, When it is deleted, Then the row is gone, the
    /// workspace listing no longer carries it, and the tombstone ledger
    /// knows the id — the row deletion and the ledger entry land in **one**
    /// transaction.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_delete_row_and_tombstone_atomically() {
        let (port, _dir) = open_port();
        let id = session_id("sess-1");
        port.apply(&id, "ws-1", title_only("Doomed")).await.unwrap();

        port.delete(&id).await.unwrap();

        assert!(!port.exists(&id).await, "a deleted row no longer exists");
        assert!(port.get(&id).await.is_none());
        assert!(port.list_ids("ws-1").await.is_empty());
        let tombstones: HashSet<String> = port.0.tombstone_ids().unwrap();
        assert!(
            tombstones.contains("sess-1"),
            "the delete leaves a tombstone for the reaper"
        );
    }

    /// Given rows across two workspaces, When the ids are listed, Then only
    /// the addressed workspace's ids answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn should_list_ids_scoped_to_workspace() {
        let (port, _dir) = open_port();
        for (id, workspace) in [("sess-1", "ws-1"), ("sess-2", "ws-2"), ("sess-3", "ws-1")] {
            port.apply(&session_id(id), workspace, title_only(id))
                .await
                .unwrap();
        }

        assert_eq!(
            port.list_ids("ws-1").await,
            vec!["sess-1".to_string(), "sess-3".to_string()]
        );
        assert_eq!(port.list_ids("ws-2").await, vec!["sess-2".to_string()]);
        assert!(port.list_ids("ws-none").await.is_empty());
    }
}
