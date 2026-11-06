//! The scheduled-tasks SQLite index (ADR-020): one engine-rooted database,
//! one `scheduled_tasks` row per task, `workspace_id` as a plain column.
use std::path::Path;
use std::sync::Mutex;
use rusqlite::Connection;

/// Schema version this build writes (PRAGMA user_version). History:
/// - 1 — the ADR-020 initial shape.
const SCHEMA_VERSION: i32 = 1;

/// One row per workspace cron task (ADR-020): the task's JSON body embedded
/// (serde round-trips the full CronTask), `workspace_id` and query-relevant
/// columns lifted out for WHERE/ORDER BY. Runs stay embedded in the row's
/// JSON (single-task volume is small and dies with the task — ADR-020).
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scheduled_tasks (
    id            TEXT PRIMARY KEY NOT NULL,
    workspace_id  TEXT NOT NULL,
    next_fire_at  TEXT,
    paused        INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    body          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_ws_created
    ON scheduled_tasks (workspace_id, created_at DESC);
";

pub struct CronIndex {
    pub(crate) conn: Mutex<Connection>,
}

/// The lifted columns every row carries; `body` is the caller's JSON.
pub struct ScheduledTaskRow {
    pub id: String,
    pub workspace_id: String,
    pub next_fire_at: Option<String>, // RFC3339 fixed-width text
    pub paused: bool,
    pub created_at: String, // RFC3339 fixed-width text
    pub body: String,      // full task JSON (serde form)
}

impl CronIndex {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// The degraded/test constructor: same schema, no file. The registry's
    /// lazy-open falls back to this on a failed file open (the
    /// shared_session_index() degrade posture — never a boot panic).
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        let mut version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        // v1 is the initial shape — the batch above already created it.
        version = SCHEMA_VERSION;
        conn.pragma_update(None, "user_version", version)?;
        Ok(())
    }

    pub fn upsert_task(&self, row: &ScheduledTaskRow) -> anyhow::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO scheduled_tasks (id, workspace_id, next_fire_at, paused, created_at, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               next_fire_at = excluded.next_fire_at,
               paused = excluded.paused,
               created_at = excluded.created_at,
               body = excluded.body",
            rusqlite::params![row.id, row.workspace_id, row.next_fire_at, row.paused, row.created_at, row.body],
        )?;
        Ok(())
    }

    pub fn get_task(&self, workspace_id: &str, id: &str) -> anyhow::Result<Option<ScheduledTaskRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, next_fire_at, paused, created_at, body
             FROM scheduled_tasks WHERE workspace_id = ?1 AND id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![workspace_id, id])?;
        Ok(rows.next()?.map(map_row).transpose()?)
    }

    pub fn list_workspace(&self, workspace_id: &str) -> anyhow::Result<Vec<ScheduledTaskRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, next_fire_at, paused, created_at, body
             FROM scheduled_tasks WHERE workspace_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(rusqlite::params![workspace_id], map_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_all(&self) -> anyhow::Result<Vec<ScheduledTaskRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workspace_id, next_fire_at, paused, created_at, body
             FROM scheduled_tasks ORDER BY workspace_id, id",
        )?;
        let rows = stmt.query_map([], map_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_task(&self, workspace_id: &str, id: &str) -> anyhow::Result<bool> {
        let changed = self.conn.lock().unwrap().execute(
            "DELETE FROM scheduled_tasks WHERE workspace_id = ?1 AND id = ?2",
            rusqlite::params![workspace_id, id],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_workspace(&self, workspace_id: &str) -> anyhow::Result<u64> {
        let changed = self.conn.lock().unwrap().execute(
            "DELETE FROM scheduled_tasks WHERE workspace_id = ?1",
            rusqlite::params![workspace_id],
        )? as u64;
        Ok(changed)
    }

    /// Transactional whole-workspace replace (the write-through primitive
    /// `WorkspaceCronManager` uses): one transaction covers inserts, updates
    /// AND deletions — a mid-write crash leaves the workspace untouched.
    pub fn replace_workspace(&self, workspace_id: &str, rows: &[ScheduledTaskRow]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM scheduled_tasks WHERE workspace_id = ?1", rusqlite::params![workspace_id])?;
        for row in rows {
            tx.execute(
                "INSERT INTO scheduled_tasks (id, workspace_id, next_fire_at, paused, created_at, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![row.id, row.workspace_id, row.next_fire_at, row.paused, row.created_at, row.body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One-time import (ADR-020): reads every workspace's legacy JSON document
    /// and upserts its tasks. Guarded by a persisted flag (the `meta` table):
    /// after the first successful run the JSON files are a FROZEN archive and
    /// are never re-read — a re-import would resurrect deleted tasks and wipe
    /// runs written since the migration.
    pub fn import_legacy_json_once(
        &self,
        buckets: &[(String, std::path::PathBuf)],
    ) -> anyhow::Result<usize> {
        // The persisted flag is the ONLY guard: once set, the legacy JSON is a
        // frozen archive and is never read again (deleted tasks must stay
        // deleted across reboots).
        {
            let conn = self.conn.lock().unwrap();
            conn.execute("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY NOT NULL, value INTEGER NOT NULL)", [])?;
            let done: bool = conn.query_row(
                "SELECT value FROM meta WHERE key = 'legacy_import_done'",
                [], |r| Ok(r.get::<_, i64>(0)? == 1),
            ).unwrap_or(false);
            if done { return Ok(0); }
        }
        let mut imported = 0usize;
        for (workspace_id, bucket_dir) in buckets {
            let path = bucket_dir.join("cron").join("tasks.json");
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
            let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) else { continue };
            let runs = doc.get("runs").cloned().unwrap_or(serde_json::json!({}));
            for task in tasks {
                let Some(id) = task.get("id").and_then(|v| v.as_str()) else { continue };
                // dirty-data guard: a row without created_at never lands
                // (an empty string would pollute the created_at DESC index)
                let Some(created_at) = task.get("created_at").and_then(|v| v.as_str()) else { continue };
                let ws = task
                    .get("workspaceId")
                    .and_then(|v| v.as_str())
                    .unwrap_or(workspace_id);
                // proposals are deliberately NOT imported: the propose-approve
                // channel is retired (C2) and nothing reads them — recorded
                // deviation from ADR-020's "proposals ride the row" wording.
                let body = serde_json::json!({"task": task, "runs": runs.get(id).cloned().unwrap_or(serde_json::json!([]))});
                self.upsert_task(&ScheduledTaskRow {
                    id: id.to_string(),
                    workspace_id: ws.to_string(),
                    next_fire_at: task.get("next_fire_at").and_then(|v| v.as_str()).map(str::to_string),
                    paused: task.get("paused").and_then(|v| v.as_bool()).unwrap_or(false),
                    created_at: created_at.to_string(),
                    body: serde_json::to_string(&body)?,
                })?;
                imported += 1;
            }
        }
        // The flag is set ONLY after the whole import succeeded — a mid-import
        // error leaves it unset so the next boot retries (partial data would
        // otherwise be frozen as-is).
        self.conn.lock().unwrap()
            .execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('legacy_import_done', 1)", [])?;
        Ok(imported)
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledTaskRow> {
    Ok(ScheduledTaskRow {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        next_fire_at: r.get(2)?,
        paused: r.get::<_, i64>(3)? != 0,
        created_at: r.get(4)?,
        body: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_fresh_path_creates_the_scheduled_tasks_table() {
        let dir = tempfile::tempdir().unwrap();
        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        let (name,): (String,) = index
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM sqlite_master WHERE type='table' AND name='scheduled_tasks'", [], |r| {
                Ok((r.get(0)?,))
            })
            .unwrap();
        assert_eq!(name, "scheduled_tasks");
    }

    fn row(id: &str, ws: &str) -> ScheduledTaskRow {
        ScheduledTaskRow {
            id: id.into(),
            workspace_id: ws.into(),
            next_fire_at: Some("2026-11-04T00:12:00.000000000Z".into()),
            paused: false,
            created_at: "2026-11-03T13:38:31.000000000Z".into(),
            body: format!(r#"{{"id":"{id}","workspaceId":"{ws}"}}"#),
        }
    }

    #[test]
    fn upsert_then_list_round_trips_rows_across_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        index.upsert_task(&row("t1", "ws-a")).unwrap();
        index.upsert_task(&row("t2", "ws-b")).unwrap();
        index.upsert_task(&row("t3", "ws-a")).unwrap();
        // upsert on the same id replaces, not duplicates
        index.upsert_task(&row("t1", "ws-a")).unwrap();

        let all = index.list_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].workspace_id, "ws-a");
        assert_eq!(all[0].id, "t1");
        assert_eq!(all[2].workspace_id, "ws-b");

        let ws_a = index.list_workspace("ws-a").unwrap();
        assert_eq!(ws_a.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["t1", "t3"]);

        assert_eq!(index.get_task("ws-b", "t2").unwrap().unwrap().id, "t2");
        assert!(index.get_task("ws-b", "t1").unwrap().is_none());
    }

    #[test]
    fn delete_task_and_delete_workspace_cascade() {
        let dir = tempfile::tempdir().unwrap();
        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        index.upsert_task(&row("t1", "ws-a")).unwrap();
        index.upsert_task(&row("t2", "ws-a")).unwrap();
        index.upsert_task(&row("t3", "ws-b")).unwrap();

        assert!(index.delete_task("ws-a", "t1").unwrap());
        assert!(!index.delete_task("ws-a", "t1").unwrap()); // already gone

        assert_eq!(index.delete_workspace("ws-a").unwrap(), 1);
        assert_eq!(index.list_all().unwrap().len(), 1); // only ws-b/t3
    }

    #[test]
    fn replace_workspace_swaps_the_whole_set_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        index.upsert_task(&row("t1", "ws-a")).unwrap();
        index.upsert_task(&row("t2", "ws-a")).unwrap();

        // replace: t1 updated, t2 gone, t9 added — one transaction
        index.replace_workspace("ws-a", &[row("t1", "ws-a"), row("t9", "ws-a")]).unwrap();
        let ids: Vec<String> = index.list_workspace("ws-a").unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["t1", "t9"]);
    }

    #[test]
    fn import_legacy_json_upserts_tasks_and_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let ws_dir = dir.path().join("ws-legacy");
        let cron_dir = ws_dir.join("cron");
        std::fs::create_dir_all(&cron_dir).unwrap();
        let legacy = r#"{
            "version": 1,
            "tasks": [{
                "id": "cron-abc", "cron": "0 9 * * 1-5", "prompt": "test",
                "session_id": "", "recurring": true, "timezone": "Asia/Tokyo",
                "created_at": "2026-11-03T13:38:31.278876Z", "last_fired": null,
                "next_fire_at": "2026-11-04T00:12:25.611Z", "origin": "user",
                "max_fires": 4, "fire_count": 0, "cron_lineage": false,
                "scope": "workspace", "workspaceId": "ws-legacy",
                "permission_posture": "manual", "permission_mode": "manual",
                "paused": false, "concurrency": "skip", "title": "test 1"
            }],
            "runs": {"cron-abc": []},
            "proposals": []
        }"#;
        std::fs::write(cron_dir.join("tasks.json"), legacy).unwrap();

        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        let imported = index.import_legacy_json_once(&[("ws-legacy".into(), ws_dir.clone())]).unwrap();

        assert_eq!(imported, 1);
        let rows = index.list_workspace("ws-legacy").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "cron-abc");
        assert!(rows[0].body.contains("\"title\":\"test 1\""));
        assert!(rows[0].body.contains("\"runs\""));
        // the JSON file stays as the read-only archive
        assert!(cron_dir.join("tasks.json").exists());
        // the persisted flag makes the SECOND import a no-op — the frozen
        // archive must never override rows that evolved since (deleted
        // tasks stay deleted; runs written since survive).
        assert_eq!(index.import_legacy_json_once(&[("ws-legacy".into(), ws_dir)]).unwrap(), 0);
        assert_eq!(index.list_workspace("ws-legacy").unwrap().len(), 1);
    }

    #[test]
    fn a_task_missing_created_at_is_skipped_not_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let ws_dir = dir.path().join("ws-dirty");
        std::fs::create_dir_all(ws_dir.join("cron")).unwrap();
        std::fs::write(ws_dir.join("cron").join("tasks.json"), r#"{
            "version": 1,
            "tasks": [
                {"id": "no-created-at", "cron": "0 9 * * *", "prompt": "x",
                 "scope": "workspace", "workspaceId": "ws-dirty"},
                {"id": "healthy", "cron": "0 9 * * *", "prompt": "y",
                 "created_at": "2026-11-03T13:38:31.000000000Z",
                 "scope": "workspace", "workspaceId": "ws-dirty"}
            ],
            "runs": {}, "proposals": []
        }"#).unwrap();

        let index = CronIndex::open(&dir.path().join("cron-index.db")).unwrap();
        assert_eq!(index.import_legacy_json_once(&[("ws-dirty".into(), ws_dir)]).unwrap(), 1);
        let rows = index.list_workspace("ws-dirty").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "healthy");
        // no empty-string created_at ever lands (index-sort pollution)
        assert!(!rows[0].created_at.is_empty());
    }
}
