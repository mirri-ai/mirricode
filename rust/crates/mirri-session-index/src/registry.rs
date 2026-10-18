//! The `workspaces.json` registry mirror — the cross-engine source of truth
//! for workspace identity, read and written by the TS engine
//! (`FileWorkspacePersistence`, see
//! `packages/agent-core-v2/src/app/workspace/fileWorkspacePersistence.ts`).
//!
//! The rust engine keeps its own copy of the workspace catalog inside
//! `session-index.db` (see [`crate::SessionIndex`]), but `workspaces.json`
//! remains the file a TS engine sharing the same Mirri home reads. So the
//! rust side **seeds** its table from the registry on open and
//! **writes through** on every `upsert_workspace` — the file always reflects
//! the union of both engines' registrations (plus the `deleted_workspace_ids`
//! tombstone list, which round-trips untouched so a TS-side soft deletion
//! survives a rust write).
//!
//! On-disk shape (v1, the TS `PersistedWorkspaceFile`):
//!
//! ```json
//! {
//!   "version": 1,
//!   "workspaces": {
//!     "<workspace id>": {
//!       "root": "/Users/dev/project",
//!       "name": "project",
//!       "created_at": "2024-01-01T00:00:00.000Z",
//!       "last_opened_at": "2024-06-01T00:00:00.000Z"
//!     }
//!   },
//!   "deleted_workspace_ids": []
//! }
//! ```
//!
//! Reads are tolerant exactly like the TS `load()`: a missing file is an
//! empty catalog, a corrupt one degrades to empty with a warning (never a
//! failure), and an unparseable timestamp falls back to "now" (the TS
//! `parseTime` behaviour). Writes go through
//! [`mirri_persistence::write_file_atomically`] — the same crash-safe
//! temp-file + rename the engine's other JSON documents use.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::WorkspaceRow;

/// The registry filename inside the Mirri home (TS `WORKSPACE_CATALOG_KEY`).
pub const WORKSPACE_REGISTRY_FILENAME: &str = "workspaces.json";

/// The catalog version the TS engine writes (`WORKSPACE_CATALOG_VERSION`).
const CATALOG_VERSION: i64 = 1;

/// Read the workspace registry — the TS `FileWorkspacePersistence.load`
/// tolerance: a missing file is an empty catalog, a corrupt file is an empty
/// catalog plus a warning, and entries missing required fields are skipped.
///
/// Rows whose id sits in the file's `deleted_workspace_ids` are filtered out
/// (design §2.3 rule 2 — the tombstone is delete *semantics*, not just a
/// write-side keepsake: a deleted workspace never reappears on a plain read,
/// only an explicit re-registration clears its tombstone).
pub fn read_workspace_registry(path: &Path) -> Vec<WorkspaceRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new(); // missing file → empty (first run)
    };
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!(
            registry = %path.display(),
            "workspaces.json could not be parsed; treating the catalog as empty"
        );
        return Vec::new();
    };
    let Some(entries) = document.get("workspaces").and_then(Value::as_object) else {
        return Vec::new();
    };
    let tombstones: std::collections::HashSet<&str> = document
        .get("deleted_workspace_ids")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let now = Utc::now();
    let mut rows = Vec::new();
    for (id, raw) in entries {
        // TS `sanitizeEntry`: a non-object entry or one missing the required
        // string fields is skipped, not fatal. `root` is one of them.
        //
        // `name` is deliberately NOT required: our own writer emitted
        // `"name": null` for unnamed workspaces (the serve `--workspace`
        // boot row) before basename backfill existed, and a null-name row
        // read here must survive — the re-read → rewrite cycle of a later
        // tombstone write would otherwise silently drop it from
        // workspaces.json. A missing/null name maps to `None`; the next
        // write backfills the root's basename (the TS `createOrTouch`
        // fallback), so a TS reader — whose `sanitizeEntry` requires a
        // string — never sees the null again.
        let Some(raw) = raw.as_object() else { continue };
        let Some(root) = raw.get("root").and_then(Value::as_str) else {
            continue;
        };
        let name = raw.get("name").and_then(Value::as_str);

        // Read-side tombstone filter (§2.3): a tombstoned id is absent even
        // when a lock-less TS writer left its row behind.
        if tombstones.contains(id.as_str()) {
            continue;
        }
        let created_at = parse_time(raw.get("created_at").and_then(Value::as_str), now);
        let last_opened_at = parse_time(raw.get("last_opened_at").and_then(Value::as_str), now);
        rows.push(WorkspaceRow {
            id: id.clone(),
            root: Some(root.to_string()),
            name: name.map(str::to_string),
            created_at,
            last_opened_at,
            sort_order: 0,
        });
    }
    rows
}

/// Write the workspace registry atomically, preserving the
/// `deleted_workspace_ids` tombstone list of the file already on disk (a
/// rust write must never resurrect a workspace the TS engine soft-deleted).
///
/// The timestamps are written the way the TS engine writes them —
/// `Date.toISOString()`, always milliseconds — so a TS reader sees its own
/// spelling.
pub fn write_workspace_registry(path: &Path, rows: &[WorkspaceRow]) -> anyhow::Result<()> {
    // Preserve the tombstones of the current file (tolerant read: a missing
    // or corrupt file simply has none to preserve).
    let deleted_ids = read_tombstoned_ids(path);
    write_registry_document(path, rows, &deleted_ids)
}

/// Write the registry document with an **explicit** tombstone set — the one
/// writer every higher-level strategy (preserve-current, D8 merge, tombstone
/// add) funnels through. Atomic replace via
/// [`mirri_persistence::write_file_atomically`].
fn write_registry_document(
    path: &Path,
    rows: &[WorkspaceRow],
    deleted_ids: &[String],
) -> anyhow::Result<()> {
    let mut workspaces = serde_json::Map::new();
    for row in rows {
        workspaces.insert(
            row.id.clone(),
            json!({
                "root": row.root,
                "name": registry_display_name(row),
                "created_at": to_iso_millis(row.created_at),
                "last_opened_at": to_iso_millis(row.last_opened_at),
            }),
        );
    }
    let document = json!({
        "version": CATALOG_VERSION,
        "workspaces": Value::Object(workspaces),
        "deleted_workspace_ids": deleted_ids,
    });
    mirri_persistence::write_file_atomically(
        path,
        serde_json::to_string_pretty(&document)?.as_bytes(),
    )?;
    Ok(())
}

/// The `name` value a written row carries — never `null`. The TS
/// `sanitizeEntry` requires `name` to be a string (`typeof v.name !==
/// 'string'` drops the whole row on the TS engine's next read), and omitting
/// the key fails the same check, so an unnamed rust row (the serve
/// `--workspace` boot row) degrades to the root's basename — the same
/// fallback the TS `createOrTouch` applies (`name ?? basename(root)`).
fn registry_display_name(row: &WorkspaceRow) -> String {
    if let Some(name) = row.name.as_deref() {
        return name.to_string();
    }
    row.root
        .as_deref()
        .and_then(|root| Path::new(root).file_name())
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_default()
}

/// Write a tombstone for a deleted workspace (design §2.3 rule 3 — the
/// unregister path's write face): through the D8 lock, re-read the disk's
/// current rows, drop the workspace's row, append its id to the tombstone
/// list, and write back. A tombstone survives every later merged write (the
/// D8 rule) until an explicit re-registration clears it
/// ([`RegistryWriter::write_merged_reinstated`]).
pub fn add_tombstone(path: &Path, workspace_id: &str) -> anyhow::Result<()> {
    let writer = RegistryWriter::new(path.to_path_buf());
    let _lock = writer.acquire_lock()?;
    let mut rows: Vec<WorkspaceRow> = read_workspace_registry(path)
        .into_iter()
        .filter(|row| row.id != workspace_id)
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let mut tombstones = read_tombstoned_ids(path);
    if !tombstones.iter().any(|id| id == workspace_id) {
        tombstones.push(workspace_id.to_string());
    }
    tombstones.sort();
    write_registry_document(path, &rows, &tombstones)
}

/// The D8 registry write strategy (design `08-multi-workspace.md` §2.3) —
/// lock file + read-merge-write, so a rust write never silently drops a row
/// a TS engine (or another process) wrote after our last read.
///
/// Write protocol:
///
/// 1. **Lock**: take `<workspaces.json>.lock` with the `create_new`
///    convention (no advisory locks, no new dependencies — if the file
///    exists someone else holds the lock). A lock file whose mtime is older
///    than [`LOCK_STALE_TIMEOUT`] is a crashed holder's residue and is taken
///    over (removed + re-created); a fresh lock is waited out (50 ms steps)
///    up to [`LOCK_WAIT_TIMEOUT`], then the write fails hard rather than
///    clobbering a live writer.
/// 2. **Read-merge-write**: after the lock is held, the disk's *current*
///    rows are re-read (the lock is the point of the protocol — the read
///    cannot race another D8 writer). Merge rule: local rows ∪ disk rows,
///    **tombstone wins** — a local row whose id is in the disk file's
///    `deleted_workspace_ids` is not resurrected. The merged set is written
///    through [`write_workspace_registry`] (atomic replace, tombstones
///    preserved).
/// 3. **Release**: the lock file is removed (its existence is the lock).
pub struct RegistryWriter {
    /// The `workspaces.json` path the writer merges into.
    path: PathBuf,
    /// A lock file older than this is a crashed holder's residue (takeover).
    stale_timeout: Duration,
    /// How long a fresh (live) lock is waited out before the write fails.
    wait_timeout: Duration,
}

/// A lock file older than this is assumed to be a crashed holder's residue
/// (mtime-staleness takeover, design §2.3).
const LOCK_STALE_TIMEOUT: Duration = Duration::from_secs(10);

/// A live lock is waited out for at most this long before the write fails.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// The lock retry cadence while waiting out a live holder.
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

impl RegistryWriter {
    /// A writer for the registry at `path` with the default lock timeouts.
    pub fn new(path: PathBuf) -> Self {
        Self::with_timeouts(path, LOCK_STALE_TIMEOUT, LOCK_WAIT_TIMEOUT)
    }

    /// The test seam: writer with explicit staleness/wait timeouts (the
    /// tests cannot wait out the production 10 s / 2 s windows).
    fn with_timeouts(path: PathBuf, stale_timeout: Duration, wait_timeout: Duration) -> Self {
        Self {
            path,
            stale_timeout,
            wait_timeout,
        }
    }

    /// The `<workspaces.json>.lock` sibling — the lock file's existence is
    /// the lock.
    fn lock_path(&self) -> PathBuf {
        lock_file_path(&self.path)
    }

    /// Locked read-merge-write: hold the lock, merge `local_rows` with the
    /// disk's current rows (tombstones win), atomically replace the file.
    pub fn write_merged(&self, local_rows: &[WorkspaceRow]) -> anyhow::Result<()> {
        self.write_merged_reinstated(local_rows, &[])
    }

    /// [`Self::write_merged`] with **reinstated** ids: a fresh registration
    /// clears the tombstone of the id it re-registers (the TS
    /// `createOrTouch` `deletedIds.delete(ws.id)` parity — design D4's
    /// "cleared on re-register" rule). Reinstated ids are exempt from the
    /// tombstone filter AND removed from the written tombstone list, so a
    /// remove→re-register cycle restores the workspace across processes.
    pub fn write_merged_reinstated(
        &self,
        local_rows: &[WorkspaceRow],
        reinstated: &[String],
    ) -> anyhow::Result<()> {
        let _lock = self.acquire_lock()?;
        let disk_rows = read_workspace_registry(&self.path);
        let mut tombstones = read_tombstoned_ids(&self.path);
        tombstones.retain(|id| !reinstated.contains(id));
        let merged = merge_rows(local_rows, &disk_rows, &tombstones);
        write_registry_document(&self.path, &merged, &tombstones)
    }

    /// Take the lock file: `create_new` wins, a stale residue is taken over,
    /// a live lock is waited out. Returns a guard whose drop removes the
    /// file (release).
    fn acquire_lock(&self) -> anyhow::Result<LockGuard> {
        RegistryLock::with_timeouts(self.lock_path(), self.stale_timeout, self.wait_timeout)
            .acquire()
    }
}

/// The `<path>.lock` sibling of any file — the lock file's existence is
/// the lock (the D8 `workspaces.json.lock` convention shared by the
/// registry writer and any other cross-process critical section over the
/// Mirri home, e.g. the session-index reconcile).
pub fn lock_file_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.to_string_lossy()))
}

/// The D8 lock primitive standalone (ADR-004): `create_new` wins, a stale
/// residue (mtime older than the staleness window) is taken over, a live
/// lock is waited out up to the wait timeout. Dropping the guard removes
/// the file (release); a remove failure is logged, never fatal (the
/// staleness window makes a leaked lock self-healing).
///
/// The same defaults the registry writer uses
/// ([`LOCK_STALE_TIMEOUT`]/[`LOCK_WAIT_TIMEOUT`]).
pub struct RegistryLock {
    path: PathBuf,
    stale_timeout: Duration,
    wait_timeout: Duration,
}

impl RegistryLock {
    /// A lock for `path` with the default registry timeouts.
    pub fn new(path: PathBuf) -> Self {
        Self::with_timeouts(path, LOCK_STALE_TIMEOUT, LOCK_WAIT_TIMEOUT)
    }

    /// The test seam: explicit staleness/wait timeouts (tests cannot wait
    /// out the production 10 s / 2 s windows).
    pub fn with_timeouts(path: PathBuf, stale_timeout: Duration, wait_timeout: Duration) -> Self {
        Self {
            path,
            stale_timeout,
            wait_timeout,
        }
    }

    /// Take the lock — `Ok(guard)` while held (drop releases), `Err` when a
    /// live holder outlasts the wait timeout.
    pub fn acquire(self) -> anyhow::Result<LockGuard> {
        let deadline = SystemTime::now() + self.wait_timeout;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)
            {
                Ok(_) => return Ok(LockGuard { path: self.path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if self.lock_is_stale(&self.path) {
                        // A crashed holder's residue (its mtime is older than
                        // the staleness window): remove and retry immediately.
                        std::fs::remove_file(&self.path)?;
                        continue;
                    }
                    if SystemTime::now() >= deadline {
                        anyhow::bail!(
                            "lock {} is held by a live writer (waited {} ms)",
                            self.path.display(),
                            self.wait_timeout.as_millis()
                        );
                    }
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error).context(format!(
                        "creating the lock file {}",
                        self.path.display()
                    )));
                }
            }
        }
    }

    /// Whether an existing lock file is stale (older than the staleness
    /// window — a crashed holder never removes its lock).
    fn lock_is_stale(&self, lock_path: &Path) -> bool {
        let Ok(modified) = std::fs::metadata(lock_path).and_then(|meta| meta.modified()) else {
            // The lock vanished between the open and the stat (the previous
            // holder released it) — treat as stale so the retry takes over.
            return true;
        };
        match SystemTime::now().duration_since(modified) {
            Ok(age) => age >= self.stale_timeout,
            // The clock moved backwards — do not take over a lock that is
            // younger than the staleness window says it is.
            Err(_) => false,
        }
    }
}

/// Release the registry lock — the lock is the file, so dropping the guard
/// removes it. A remove failure is logged, never fatal (the staleness window
/// makes a leaked lock self-healing).
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                lock = %self.path.display(),
                %error,
                "removing the workspace registry lock failed; the staleness \
                 window will take it over"
            );
        }
    }
}

/// The `deleted_workspace_ids` of the file at `path` — the tombstone set a
/// merged write must not resurrect (tolerant read: missing/corrupt → empty).
fn read_tombstoned_ids(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|document| {
            document
                .get("deleted_workspace_ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// The D8 merge rule: local rows ∪ disk rows (local wins the fields of a
/// shared id — the caller's view is the freshest), minus every id the disk
/// tombstone set marks deleted (tombstone wins — a deleted workspace is
/// never resurrected by a later write).
fn merge_rows(
    local: &[WorkspaceRow],
    disk: &[WorkspaceRow],
    tombstones: &[String],
) -> Vec<WorkspaceRow> {
    let mut by_id: std::collections::HashMap<&str, &WorkspaceRow> =
        std::collections::HashMap::new();
    for row in local {
        by_id.insert(row.id.as_str(), row);
    }
    for row in disk {
        by_id.entry(row.id.as_str()).or_insert(row);
    }
    let tombstoned: std::collections::HashSet<&str> =
        tombstones.iter().map(String::as_str).collect();
    let mut merged: Vec<WorkspaceRow> = by_id
        .into_values()
        .filter(|row| !tombstoned.contains(row.id.as_str()))
        .cloned()
        .collect();
    merged.sort_by(|a, b| a.id.cmp(&b.id));
    merged
}

/// Parse a registry timestamp — the TS `parseTime` behaviour: an RFC 3339
/// (or ISO) string when parseable, else the fallback (`now`).
fn parse_time(value: Option<&str>, fallback: DateTime<Utc>) -> DateTime<Utc> {
    value
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
        .unwrap_or(fallback)
}

/// Format a timestamp the way `Date.toISOString()` does — always exactly
/// three fractional digits and a `Z` suffix.
fn to_iso_millis(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given a registry file in the TS on-disk shape, When it is read, Then
    /// every well-formed entry round-trips into rows with the file's ids,
    /// roots, names, and timestamps.
    #[test]
    fn should_read_rows_from_a_ts_shaped_registry_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "workspaces": {
                    "wd_project_abc123def456": {
                        "root": "/Users/dev/project",
                        "name": "project",
                        "created_at": "2024-01-01T00:00:00.000Z",
                        "last_opened_at": "2024-06-01T12:30:00.000Z"
                    }
                },
                "deleted_workspace_ids": ["wd_gone_000000000000"]
            }"#,
        )
        .unwrap();

        let rows = read_workspace_registry(&path);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "wd_project_abc123def456");
        assert_eq!(rows[0].root.as_deref(), Some("/Users/dev/project"));
        assert_eq!(rows[0].name.as_deref(), Some("project"));
        assert_eq!(rows[0].created_at.to_rfc3339(), "2024-01-01T00:00:00+00:00");
    }

    /// Given no registry file, When it is read, Then the catalog is empty
    /// (a first run, never a failure).
    #[test]
    fn should_return_an_empty_catalog_when_the_registry_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_workspace_registry(&tmp.path().join("workspaces.json")).is_empty());
    }

    // =========================================================================
    // Unnamed rows (webverify bug D — registry corruption for unnamed
    // workspaces)
    // =========================================================================

    // Given a registry row carrying `"name": null` (the shape the engine's
    // own writer emitted for unnamed workspaces, e.g. the serve `--workspace`
    // boot row), when the registry is read, then the row SURVIVES with a
    // `None` name — requiring a string name here is what made a later
    // tombstone write silently drop the row from workspaces.json.
    #[test]
    fn should_preserve_a_null_name_row_when_reading_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{"wd_ws_a_000000000001":{"root":"/workspaces/ws-a","name":null,"created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":[]}"#,
        )
        .unwrap();

        let rows = read_workspace_registry(&path);

        assert_eq!(rows.len(), 1, "the null-name row is not dropped on read");
        assert_eq!(rows[0].id, "wd_ws_a_000000000001");
        assert_eq!(rows[0].root.as_deref(), Some("/workspaces/ws-a"));
        assert_eq!(rows[0].name, None, "the null name reads back as None");
    }

    // Given an unnamed row A on disk (a `"name": null` boot row), when a
    // tombstone is written for an UNRELATED workspace B, then row A stays in
    // the file — the tombstone write's re-read → rewrite cycle must not
    // corrupt the catalog by dropping rows our stricter read used to skip.
    #[test]
    fn should_keep_an_unnamed_row_when_tombstoning_an_unrelated_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{
                "wd_ws_a_000000000001":{"root":"/workspaces/ws-a","name":null,"created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"},
                "wd_ws_b_000000000002":{"root":"/workspaces/ws-b","name":"ws-b","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}
            },"deleted_workspace_ids":[]}"#,
        )
        .unwrap();

        add_tombstone(&path, "wd_ws_b_000000000002").unwrap();

        let rows = read_workspace_registry(&path);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["wd_ws_a_000000000001"],
            "the unnamed row A survives the tombstone write for workspace B"
        );
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            document["workspaces"].get("wd_ws_a_000000000001").is_some(),
            "row A is still physically present in workspaces.json"
        );
    }

    // Given an unnamed row (name: None), when the registry is written and
    // read back, then the row round-trips with the root's basename as its
    // name — never a JSON null (the TS `sanitizeEntry` requires a string and
    // would drop a null-name row on the TS engine's next read; omitting the
    // key fails the same check).
    #[test]
    fn should_round_trip_an_unnamed_row_with_the_root_basename_as_name() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        let now = Utc::now();
        let unnamed = WorkspaceRow {
            id: "wd_ws_a_000000000001".to_string(),
            root: Some("/workspaces/ws-a".to_string()),
            name: None,
            created_at: now,
            last_opened_at: now,
            sort_order: 0,
        };

        write_workspace_registry(&path, &[unnamed]).unwrap();

        // The written document never carries a null name
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["workspaces"]["wd_ws_a_000000000001"]["name"],
            json!("ws-a"),
            "an unnamed row is written with the root's basename (the TS createOrTouch fallback)"
        );
        // And the read round-trips the backfilled name
        let rows = read_workspace_registry(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name.as_deref(), Some("ws-a"));
    }

    /// Given a corrupt registry file, When it is read, Then the catalog
    /// degrades to empty (the TS `load` tolerance — never a failure).
    #[test]
    fn should_return_an_empty_catalog_when_the_registry_file_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(&path, "{ not json").unwrap();

        assert!(read_workspace_registry(&path).is_empty());
    }

    /// Given a registry file carrying tombstones, When the catalog is
    /// rewritten, Then the `deleted_workspace_ids` list survives the write
    /// (a rust write must not resurrect a TS-side soft deletion).
    #[test]
    fn should_preserve_deleted_ids_when_rewriting_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{},"deleted_workspace_ids":["wd_gone_000000000000"]}"#,
        )
        .unwrap();
        let now = Utc::now();
        let rows = vec![WorkspaceRow {
            id: "wd_project_abc123def456".to_string(),
            root: Some("/Users/dev/project".to_string()),
            name: Some("project".to_string()),
            created_at: now,
            last_opened_at: now,
            sort_order: 0,
        }];

        write_workspace_registry(&path, &rows).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["deleted_workspace_ids"],
            json!(["wd_gone_000000000000"])
        );
        assert_eq!(
            written["workspaces"]["wd_project_abc123def456"]["root"],
            json!("/Users/dev/project")
        );
        // The timestamps carry the TS `toISOString()` spelling (millisecond
        // precision, Z suffix).
        assert_eq!(
            written["workspaces"]["wd_project_abc123def456"]["created_at"]
                .as_str()
                .unwrap()
                .len(),
            24
        );
    }

    // =========================================================================
    // RegistryWriter — the D8 lock + read-merge-write strategy (§2.3)
    // =========================================================================

    /// One registry row with the fixture shape (used by the writer tests).
    fn writer_row(id: &str) -> WorkspaceRow {
        let now = Utc::now();
        WorkspaceRow {
            id: id.to_string(),
            root: Some(format!("/tmp/{id}")),
            name: Some(id.to_string()),
            created_at: now,
            last_opened_at: now,
            sort_order: 0,
        }
    }

    /// The workspace ids present in the registry file at `path`.
    fn written_ids(path: &Path) -> Vec<String> {
        let mut ids: Vec<String> = read_workspace_registry(path)
            .into_iter()
            .map(|row| row.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    // Given a registry row set written through the D8 writer, when another
    // writer inserted a new row on disk in between, then a second merged
    // write keeps BOTH sets — the concurrent insert is never dropped by the
    // read-merge-write (the D8 red line).
    #[test]
    fn should_keep_an_external_row_when_merging_local_rows() {
        // Given a registry file holding a row an external writer added after
        // our last read
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{"wd_external_000000000001":{"root":"/ext","name":"external","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":[]}"#,
        )
        .unwrap();

        // When a local row set is merged in
        RegistryWriter::new(path.clone())
            .write_merged(&[writer_row("wd_local_000000000002")])
            .unwrap();

        // Then both the external and the local row survive
        assert_eq!(
            written_ids(&path),
            vec![
                "wd_external_000000000001".to_string(),
                "wd_local_000000000002".to_string(),
            ]
        );
    }

    // Given a workspace id tombstoned on disk, when a local row set tries to
    // resurrect it, then the tombstone wins — the merged write must not
    // bring a deleted workspace back (D8 rule).
    #[test]
    fn should_let_the_tombstone_win_when_a_local_row_resurrects_the_id() {
        // Given a registry file whose tombstone list marks one id deleted
        // while a local write wants to re-register the very same id
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{"wd_gone_000000000000":{"root":"/gone","name":"gone","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":["wd_gone_000000000000"]}"#,
        )
        .unwrap();

        // When the resurrecting row is merged in
        RegistryWriter::new(path.clone())
            .write_merged(&[writer_row("wd_gone_000000000000")])
            .unwrap();

        // Then the tombstone wins: the id stays absent from the catalog and
        // its tombstone survives for the next writer
        assert!(written_ids(&path).is_empty());
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["deleted_workspace_ids"],
            json!(["wd_gone_000000000000"])
        );
    }

    // Given a registry file whose tombstone list marks an id deleted while a
    // lock-less writer left its row behind, when the registry is read, then
    // the row is absent — the read-side tombstone filter (§2.3 rule 2: a
    // tombstone is delete semantics, not just a write-side keepsake).
    #[test]
    fn should_filter_a_tombstoned_row_when_reading_the_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{"wd_gone_000000000000":{"root":"/gone","name":"gone","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"},"wd_live_000000000001":{"root":"/live","name":"live","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":["wd_gone_000000000000"]}"#,
        )
        .unwrap();

        let rows = read_workspace_registry(&path);

        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["wd_live_000000000001"],
            "the tombstoned row is absent even though the file still carries it"
        );
    }

    // Given a workspace tombstoned through the W6 unregister face, when a
    // later merged write tries to resurrect the row, then the tombstone wins
    // (the D8 rule holds for tombstones written by THIS engine too, not just
    // TS-side ones).
    #[test]
    fn should_keep_a_tombstone_written_by_add_tombstone_dead_when_merged_against() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        write_workspace_registry(&path, &[writer_row("wd_removed_000000000009")]).unwrap();

        add_tombstone(&path, "wd_removed_000000000009").unwrap();
        // The row is gone from the file's workspaces map and dead on read.
        assert!(written_ids(&path).is_empty());

        // A concurrent writer resurrecting the id loses to the tombstone.
        RegistryWriter::new(path.clone())
            .write_merged(&[writer_row("wd_removed_000000000009")])
            .unwrap();
        assert!(
            written_ids(&path).is_empty(),
            "the tombstone written by add_tombstone must win over a resurrecting merge"
        );
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["deleted_workspace_ids"],
            json!(["wd_removed_000000000009"])
        );
    }

    // Given a tombstoned workspace, when a REINSTATED merged write
    // re-registers the id, then the row returns and the tombstone is cleared
    // — the D4 "re-register clears the tombstone" rule (remove → re-register
    // restores the workspace across processes, exactly the TS
    // `createOrTouch` behaviour).
    #[test]
    fn should_clear_the_tombstone_when_a_reinstated_write_re_registers_the_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        write_workspace_registry(&path, &[writer_row("wd_removed_000000000009")]).unwrap();
        add_tombstone(&path, "wd_removed_000000000009").unwrap();

        RegistryWriter::new(path.clone())
            .write_merged_reinstated(
                &[writer_row("wd_removed_000000000009")],
                &["wd_removed_000000000009".to_string()],
            )
            .unwrap();

        assert_eq!(
            written_ids(&path),
            vec!["wd_removed_000000000009".to_string()],
            "the re-registered row is back in the catalog"
        );
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document["deleted_workspace_ids"],
            json!([]),
            "the tombstone is cleared by the re-registration"
        );
    }

    // Given a lock file older than the staleness window, when a merged write
    // runs, then the stale lock is taken over and the write proceeds (the
    // crashed holder's residue never blocks the registry).
    #[test]
    fn should_take_over_a_stale_lock_when_writing_merged_rows() {
        // Given a lock file whose mtime sits beyond the staleness window
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        let lock = PathBuf::from(format!("{}.lock", path.to_string_lossy()));
        std::fs::write(&lock, b"stale holder").unwrap();
        let past = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(past)
            .unwrap();

        // When a merged write runs with a 1-second staleness window
        let writer = RegistryWriter::with_timeouts(
            path.clone(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        writer
            .write_merged(&[writer_row("wd_local_000000000002")])
            .unwrap();

        // Then the stale lock was taken over (removed and re-created) and
        // the write landed
        assert_eq!(
            written_ids(&path),
            vec!["wd_local_000000000002".to_string()]
        );
    }

    // Given a FRESH lock file (a live writer's), when a merged write runs,
    // then it waits out the window and fails without touching the registry —
    // a live holder is never clobbered.
    #[test]
    fn should_fail_without_touching_the_registry_when_a_fresh_lock_is_held() {
        // Given a lock file younger than the staleness window
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(WORKSPACE_REGISTRY_FILENAME);
        std::fs::write(
            &path,
            r#"{"version":1,"workspaces":{"wd_existing_000000000003":{"root":"/ext","name":"ext","created_at":"2024-01-01T00:00:00.000Z","last_opened_at":"2024-01-01T00:00:00.000Z"}},"deleted_workspace_ids":[]}"#,
        )
        .unwrap();
        let lock = PathBuf::from(format!("{}.lock", path.to_string_lossy()));
        std::fs::write(&lock, b"live holder").unwrap();

        // When a merged write runs with a 10-second staleness window and a
        // 100 ms wait budget
        let writer = RegistryWriter::with_timeouts(
            path.clone(),
            Duration::from_secs(10),
            Duration::from_millis(100),
        );
        let result = writer.write_merged(&[writer_row("wd_local_000000000002")]);

        // Then the write fails (the live lock never freed) and the registry
        // file is untouched
        assert!(result.is_err(), "a live lock must fail the write");
        assert_eq!(
            written_ids(&path),
            vec!["wd_existing_000000000003".to_string()]
        );
        assert!(
            std::fs::metadata(&lock).is_ok(),
            "the live lock file must survive the failed write"
        );
    }
}
