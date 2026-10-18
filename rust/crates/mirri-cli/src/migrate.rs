//! `mirri migrate` — bring legacy (TS-engine) sessions into the rust
//! engine's v3 storage root as *usable* copies.
//!
//! The TS kernel persists a session under `<mirri-home>/sessions/<bucket>/
//! session_<uuid>/` with `state.json` at the top and the main wire log at
//! `agents/main/wire.jsonl`; the rust engine reads
//! `<mirri-home>/v3_sessions/<bucket>/<session_id>/` with `wire.jsonl` at
//! the session dir's top level. Moving a TS directory wholesale (the
//! retired `migrate-sessions` approach) therefore produced sessions that
//! *list* (state.json parses — the schema is alias-compatible) but never
//! *resume* (the wire log sits at a path the resume path never reads).
//!
//! This command copies instead of moves — the legacy root is left
//! untouched so the TS engines keep working — and adapts the layout on
//! the way:
//!
//! 1. every session directory is copied verbatim into the sibling
//!    `v3_sessions` root, re-bucketed by the rust
//!    [`encode_work_dir_key`](mirri_core::encode_work_dir_key) of the
//!    state.json `workDir` (the source bucket name when no workDir is
//!    recorded), so routing and disk always agree on the bucket;
//! 2. a TS layout's `agents/main/wire.jsonl` is additionally promoted to
//!    the copy's top-level `wire.jsonl` (the original stays in place);
//! 3. each copy is verified the way the listing/resume paths read it:
//!    `state.json` parses through the validated read and the top-level
//!    wire log's first record is a `metadata` record;
//! 4. each migrated session's workspace is registered in the home's
//!    `workspaces.json` (D8 merged write, tombstones respected) so the
//!    engine can route the session after the next server start.
//!
//! The wire protocol version upgrade (1.x → 2.0) stays lazy: it happens
//! when a session actually resumes, exactly as before.
//!
//! Idempotent: a session already present in the target root is skipped
//! and reported; `--dry-run` reports without writing anything.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mirri_core::session::persistence::locate_session_dirs;
use mirri_core::{SessionId, SessionPersistence, encode_work_dir_key};
use mirri_os::{LocalFs, LocalSyncFs};
use mirri_session_index::{
    RegistryWriter, WORKSPACE_REGISTRY_FILENAME, WorkspaceRow, read_workspace_registry,
};

/// The legacy TS-era sessions subdirectory under the Mirri home (or under a
/// workspace's `.mirri/` directory).
const SESSIONS_SUBDIR: &str = "sessions";

/// The rust engine's storage-root subdirectory — always the target.
const V3_SESSIONS_SUBDIR: &str = "v3_sessions";

/// The counters a migration run reports.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationTotals {
    /// Session directories copied into the target root.
    copied: usize,
    /// Sessions already present in the target root (the idempotency total).
    skipped: usize,
    /// Copies whose verified read failed (copied but not resumable yet).
    unresumable: usize,
    /// I/O failures that aborted a copy.
    failed: usize,
    /// Workspaces newly registered in `workspaces.json`.
    registered: usize,
    /// Workspaces whose registration was refused by a tombstone (the user
    /// deleted the workspace earlier — `workspaces.json` semantics win).
    tombstoned: usize,
}

/// Run `mirri migrate`: copy every legacy session into the rust engine's
/// v3 storage root (see the module doc for the full semantics).
///
/// Without `--workspace` only the global home root
/// (`<mirri-home>/sessions/`) is migrated; with `--workspace <dir>` that
/// workspace's `.mirri/sessions/` is migrated as well. Workspace
/// registration always goes to the home's `workspaces.json`.
pub async fn run_migrate(workspace: Option<&str>, dry_run: bool) -> Result<()> {
    let home = mirri_cli::commands::mirri_data_dir()?;
    let mut pairs = vec![(home.join(SESSIONS_SUBDIR), home.join(V3_SESSIONS_SUBDIR))];
    if let Some(workspace_root) = workspace {
        let mirri_dir = PathBuf::from(workspace_root).join(".mirri");
        pairs.push((
            mirri_dir.join(SESSIONS_SUBDIR),
            mirri_dir.join(V3_SESSIONS_SUBDIR),
        ));
    }
    let registry_path = home.join(WORKSPACE_REGISTRY_FILENAME);
    let totals = run_migrate_inner(&pairs, &registry_path, dry_run).await?;

    println!(
        "{} session(s) copied, {} skipped (already in the v3 root), {} copied but not \
         resumable, {} failed",
        totals.copied, totals.skipped, totals.unresumable, totals.failed
    );
    println!(
        "{} workspace(s) registered, {} skipped (previously deleted)",
        totals.registered, totals.tombstoned
    );
    if totals.copied > 0 && !dry_run {
        println!(
            "Restart the rust server (or reopen the web app) so the session index picks \
             the migrated sessions up; the legacy root is untouched and stays readable \
             by the TS engines."
        );
    }
    Ok(())
}

/// The migration orchestration over explicit source/target pairs (and the
/// workspace-registry path), so the whole flow is testable without touching
/// the process environment.
async fn run_migrate_inner(
    pairs: &[(PathBuf, PathBuf)],
    registry_path: &Path,
    dry_run: bool,
) -> Result<MigrationTotals> {
    let mut totals = MigrationTotals::default();
    for (source, target) in pairs {
        migrate_root(source, target, registry_path, dry_run, &mut totals).await?;
    }
    Ok(totals)
}

/// Migrate one source root into its v3 sibling.
///
/// A missing source root is reported and skipped (not an error). Sessions
/// already present in the target root are skipped; the workspace rows of
/// the sessions actually copied are merged into the registry at the end of
/// the root's pass (one D8-locked write per root, not per session).
async fn migrate_root(
    source: &Path,
    target: &Path,
    registry_path: &Path,
    dry_run: bool,
    totals: &mut MigrationTotals,
) -> Result<()> {
    if !source.exists() {
        println!("No sessions directory found at {}", source.display());
        return Ok(());
    }
    let source_sessions = locate_session_dirs(source, &LocalSyncFs::new())
        .with_context(|| format!("enumerate sessions under '{}'", source.display()))?;
    let target_sessions = locate_session_dirs(target, &LocalSyncFs::new())
        .with_context(|| format!("enumerate sessions under '{}'", target.display()))?;
    if source_sessions.is_empty() && target_sessions.is_empty() {
        println!("No sessions found under '{}'", source.display());
        return Ok(());
    }

    // Sessions already in the target root: skipped (the idempotency total).
    let target_by_id: HashMap<&str, &Path> = target_sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session.dir.as_path()))
        .collect();

    // The workspace rows to register for the sessions this pass copies:
    // one row per workDir, with the most recent session `updatedAt` as the
    // `last_opened_at` (historical — a migrated workspace never hijacks the
    // serve default-workspace choice, which picks max `last_opened_at`).
    let mut rows_by_workdir: HashMap<String, WorkspaceRow> = HashMap::new();

    for session in &source_sessions {
        if target_by_id.contains_key(session.session_id.as_str()) {
            totals.skipped += 1;
            continue;
        }

        // The state's workDir (TS `workDir`/`cwd`, rust `work_dir`) decides
        // the target bucket; the source bucket name is the fallback for a
        // state that records none.
        let state = read_state_fields(&session.dir);
        let bucket = state
            .work_dir
            .as_deref()
            .map(Path::new)
            .map(encode_work_dir_key)
            .or_else(|| session.bucket.clone());
        let destination = match &bucket {
            Some(bucket) => target.join(bucket).join(&session.session_id),
            None => target.join(&session.session_id),
        };
        if destination.exists() {
            // A bucket-name collision with an existing target entry (e.g. a
            // flat session named like the bucket) — never merge, never
            // overwrite; manual resolution unblocks a re-run.
            totals.skipped += 1;
            println!(
                "Skipped {} (target '{}' already exists; rename one side and re-run)",
                session.dir.display(),
                destination.display()
            );
            continue;
        }

        if dry_run {
            println!(
                "[dry-run] Would copy {} -> {}",
                session.dir.display(),
                destination.display()
            );
            totals.copied += 1;
            if let Some(work_dir) = &state.work_dir {
                note_registration(&mut rows_by_workdir, work_dir, state.updated_at);
            }
            continue;
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create the target bucket '{}'", parent.display()))?;
        }
        let promoted = match copy_dir_recursive(&session.dir, &destination)
            .and_then(|()| promote_main_wire_log(&destination))
        {
            Ok(promoted) => promoted,
            Err(error) => {
                totals.failed += 1;
                eprintln!(
                    "Failed to migrate {} -> {}: {}",
                    session.dir.display(),
                    destination.display(),
                    error
                );
                continue;
            }
        };
        let suffix = if promoted {
            " (TS wire log promoted)"
        } else {
            ""
        };
        match verify_resumable(target, bucket.as_deref(), &session.session_id).await {
            Ok(()) => {
                println!(
                    "Copied {} -> {}{}",
                    session.dir.display(),
                    destination.display(),
                    suffix
                );
            }
            Err(reason) => {
                totals.unresumable += 1;
                println!(
                    "Copied {} -> {}{} but NOT resumable: {}",
                    session.dir.display(),
                    destination.display(),
                    suffix,
                    reason
                );
            }
        }
        totals.copied += 1;

        if let Some(work_dir) = &state.work_dir {
            note_registration(&mut rows_by_workdir, work_dir, state.updated_at);
        }
    }

    register_workspaces(registry_path, &rows_by_workdir, dry_run, totals)
}

/// The migration-relevant fields of a session's `state.json`, read
/// tolerantly (the listing's dual-engine tolerance: camelCase or
/// snake_case, epoch-milliseconds or RFC 3339 timestamps).
#[derive(Debug, Default)]
struct StateFields {
    work_dir: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

/// Read [`StateFields`] from a session directory's `state.json`. A missing
/// or unparseable file yields the default (no workDir, no timestamp) — the
/// copy still proceeds, just without re-bucketing and registration.
fn read_state_fields(session_dir: &Path) -> StateFields {
    let Ok(text) = std::fs::read_to_string(session_dir.join("state.json")) else {
        return StateFields::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return StateFields::default();
    };
    let work_dir = ["workDir", "cwd", "work_dir"]
        .iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_string);
    let updated_at = ["updatedAt", "updated_at"]
        .iter()
        .find_map(|key| value.get(*key).and_then(parse_timestamp));
    StateFields {
        work_dir,
        updated_at,
    }
}

/// Parse an engine timestamp from either spelling: epoch-milliseconds
/// integer (TS) or RFC 3339 string (rust).
fn parse_timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .and_then(DateTime::<Utc>::from_timestamp_millis),
        serde_json::Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|dt| dt.with_timezone(&Utc)),
        _ => None,
    }
}

/// Record (or refresh) the registration row for a migrated session's
/// `work_dir`, keeping the most recent `updated_at` seen for it.
fn note_registration(
    rows_by_workdir: &mut HashMap<String, WorkspaceRow>,
    work_dir: &str,
    updated_at: Option<DateTime<Utc>>,
) {
    let id = encode_work_dir_key(Path::new(work_dir));
    // A missing `updatedAt` falls back to the epoch — a registered row must
    // never hijack the serve default-workspace choice (max last_opened_at)
    // on the strength of missing data.
    let last_opened_at = updated_at.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    match rows_by_workdir.get_mut(&id) {
        Some(existing) => {
            if last_opened_at > existing.last_opened_at {
                existing.last_opened_at = last_opened_at;
            }
        }
        None => {
            let name = Path::new(work_dir)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            rows_by_workdir.insert(
                id.clone(),
                WorkspaceRow {
                    id,
                    root: Some(work_dir.to_string()),
                    name,
                    created_at: Utc::now(),
                    last_opened_at,
                    sort_order: 0,
                },
            );
        }
    }
}

/// Merge the collected workspace rows into `workspaces.json` (one D8-locked
/// merged write; tombstones win — a workspace the user deleted is never
/// resurrected by a migration, its sessions copy but stay unroutable and
/// the run reports the refusal).
fn register_workspaces(
    registry_path: &Path,
    rows_by_workdir: &HashMap<String, WorkspaceRow>,
    dry_run: bool,
    totals: &mut MigrationTotals,
) -> Result<()> {
    if rows_by_workdir.is_empty() {
        return Ok(());
    }
    let rows: Vec<WorkspaceRow> = rows_by_workdir.values().cloned().collect();
    if dry_run {
        for row in &rows {
            println!(
                "[dry-run] Would register workspace '{}' ({})",
                row.root.as_deref().unwrap_or("?"),
                row.id
            );
        }
        return Ok(());
    }

    RegistryWriter::new(registry_path.to_path_buf())
        .write_merged(&rows)
        .with_context(|| {
            format!(
                "merge migrated workspaces into '{}'",
                registry_path.display()
            )
        })?;
    // A row missing after the merged write was dropped by a tombstone —
    // the user deleted that workspace; report it instead of silently
    // leaving its sessions unroutable.
    let registered_rows = read_workspace_registry(registry_path);
    let present: std::collections::HashSet<&str> =
        registered_rows.iter().map(|row| row.id.as_str()).collect();
    for row in &rows {
        if present.contains(row.id.as_str()) {
            totals.registered += 1;
        } else {
            totals.tombstoned += 1;
            println!(
                "Workspace '{}' ({}) skipped: previously deleted (workspaces.json \
                 tombstone); its sessions were copied but stay unroutable until the \
                 workspace is re-registered",
                row.root.as_deref().unwrap_or("?"),
                row.id
            );
        }
    }
    Ok(())
}

/// Promote the TS layout's main wire log to the top level: copy
/// `agents/main/wire.jsonl` to `wire.jsonl` when the top-level file does
/// not exist (the rust resume path reads only the top-level path). The
/// original stays in place — the copy keeps the full source tree. Returns
/// whether a promotion happened.
fn promote_main_wire_log(destination: &Path) -> Result<bool> {
    let main_log = destination.join("agents").join("main").join("wire.jsonl");
    let top_log = destination.join("wire.jsonl");
    if main_log.is_file() && !top_log.exists() {
        std::fs::copy(&main_log, &top_log).with_context(|| {
            format!(
                "promote '{}' to '{}'",
                main_log.display(),
                top_log.display()
            )
        })?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Verify a copied session the way the engine reads it on resume: the
/// `state.json` parses through the listing's validated read, and the
/// top-level `wire.jsonl` exists with a `metadata` first record. Returns
/// the human-readable reason when it does not.
async fn verify_resumable(
    target: &Path,
    bucket: Option<&str>,
    session_id: &str,
) -> std::result::Result<(), String> {
    let mut persistence = SessionPersistence::new(target.to_path_buf(), Arc::new(LocalFs::new()));
    if let Some(bucket) = bucket {
        persistence = persistence.with_workspace_key(bucket.to_string());
    }
    let sid = SessionId::new(session_id);
    if persistence.read_persisted_summary(&sid).await.is_none() {
        return Err("state.json is missing or unparseable".to_string());
    }
    let wire_log = persistence.wire_log_path(&sid);
    if !wire_log.is_file() {
        return Err(format!("no top-level wire.jsonl at {}", wire_log.display()));
    }
    match first_record_is_metadata(&wire_log) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "the first wire record of {} is not a metadata record",
            wire_log.display()
        )),
        Err(reason) => Err(reason),
    }
}

/// Whether the first line of a wire log is a parseable `metadata` record
/// (either engine's spelling: `type` or `op`). A read failure surfaces as
/// the error string.
fn first_record_is_metadata(path: &Path) -> std::result::Result<bool, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("wire log unreadable: {error}"))?;
    let mut line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut line)
        .map_err(|error| format!("wire log unreadable: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(line.trim_end())
        .map_err(|_| "the wire log's first line is not valid JSON".to_string())?;
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("op").and_then(serde_json::Value::as_str));
    Ok(kind == Some("metadata"))
}

/// Recursively copy a directory tree (plain files and subdirectories;
/// symlinks are followed by `std::fs::copy`, which preserves content).
fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let entry_path = entry.path();
        let entry_destination = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry_path, &entry_destination)?;
        } else {
            std::fs::copy(&entry_path, &entry_destination)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_session_index::read_workspace_registry;

    /// Seed a TS-layout session (`state.json` camelCase + the main wire log
    /// at `agents/main/wire.jsonl`) under `<root>/<bucket>/<session_id>/`
    /// and return the session directory.
    fn make_ts_session(root: &Path, bucket: &str, session_id: &str) -> PathBuf {
        let session_dir = root.join(bucket).join(session_id);
        std::fs::create_dir_all(session_dir.join("agents").join("main"))
            .expect("create ts session dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"createdAt":1789373829678,"updatedAt":1789374426548,"title":"ts session","workDir":"/w/proj"}"#,
        )
        .expect("write state.json should succeed");
        std::fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            "{\"type\":\"metadata\",\"protocol_version\":\"1.5\",\"created_at\":1789373829678}\n{\"type\":\"user\"}\n",
        )
        .expect("write wire.jsonl should succeed");
        session_dir
    }

    #[tokio::test]
    async fn should_copy_promote_and_register_when_migrating_a_ts_session() {
        // Given a temp home holding a TS-layout session in the legacy root
        // and no v3 root yet
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        let ts_session = make_ts_session(&source, "wd_old_000000000001", "session_ts");
        let registry_path = home.path().join(WORKSPACE_REGISTRY_FILENAME);

        // When the migration runs
        let totals = run_migrate_inner(&[(source.clone(), target.clone())], &registry_path, false)
            .await
            .expect("migration should succeed");

        // Then the session was copied into the rust bucket derived from the
        // state's workDir, with the TS main wire log promoted to the
        // top-level path the resume path reads
        let bucket = encode_work_dir_key(Path::new("/w/proj"));
        let migrated = target.join(&bucket).join("session_ts");
        assert_eq!(totals.copied, 1);
        assert_eq!(totals.unresumable, 0);
        assert_eq!(totals.failed, 0);
        assert!(migrated.join("state.json").is_file());
        assert_eq!(
            std::fs::read_to_string(migrated.join("wire.jsonl")).expect("read promoted log"),
            std::fs::read_to_string(ts_session.join("agents").join("main").join("wire.jsonl"))
                .expect("read source log")
        );
        // And the original nested log stays in place (full-fidelity copy)
        assert!(
            migrated
                .join("agents")
                .join("main")
                .join("wire.jsonl")
                .is_file()
        );
        // And the source root is untouched (copy semantics — the TS
        // engines keep reading it)
        assert!(ts_session.join("state.json").is_file());
        assert!(
            ts_session
                .join("agents")
                .join("main")
                .join("wire.jsonl")
                .is_file()
        );
        // And the copy verifies as resumable through the engine's own read
        verify_resumable(&target, Some(&bucket), "session_ts")
            .await
            .expect("the migrated session should verify as resumable");

        // And the workspace was registered with the same bucket id and a
        // historical last_opened_at (the state's updatedAt)
        let rows = read_workspace_registry(&registry_path);
        let row = rows
            .iter()
            .find(|row| row.id == bucket)
            .expect("the migrated workspace is registered");
        assert_eq!(row.root.as_deref(), Some("/w/proj"));
        assert_eq!(
            row.last_opened_at,
            DateTime::<Utc>::from_timestamp_millis(1789374426548).expect("parse ts")
        );
        assert_eq!(totals.registered, 1);
    }

    #[tokio::test]
    async fn should_skip_sessions_already_in_the_target_root_when_re_running() {
        // Given a migration that already ran once
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        make_ts_session(&source, "wd_old_000000000001", "session_ts");
        let registry_path = home.path().join(WORKSPACE_REGISTRY_FILENAME);
        let pairs = [(source, target)];
        let first = run_migrate_inner(&pairs, &registry_path, false)
            .await
            .expect("first migration should succeed");

        // When the migration runs again
        let second = run_migrate_inner(&pairs, &registry_path, false)
            .await
            .expect("second migration should succeed");

        // Then the re-run is a no-op: everything is skipped, nothing copied
        assert_eq!(first.copied, 1);
        assert_eq!(second.copied, 0);
        assert_eq!(second.skipped, 1);
        assert_eq!(second.registered, 0);
    }

    #[tokio::test]
    async fn should_copy_a_flat_rust_session_without_touching_its_wire_log() {
        // Given a flat-layout rust session (top-level wire.jsonl, no agents
        // tree) in the legacy root
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        let session_dir = source.join("session_flat");
        std::fs::create_dir_all(&session_dir).expect("create flat dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"version":3,"created_at":"2025-01-15T10:00:00Z","updated_at":"2025-01-15T10:05:00Z","work_dir":"/w/flat"}"#,
        )
        .expect("write state.json should succeed");
        std::fs::write(
            session_dir.join("wire.jsonl"),
            "{\"type\":\"metadata\",\"protocol_version\":\"2.0\"}\n",
        )
        .expect("write wire.jsonl should succeed");

        // When the migration runs
        let totals = run_migrate_inner(
            &[(source, target.clone())],
            &home.path().join(WORKSPACE_REGISTRY_FILENAME),
            false,
        )
        .await
        .expect("migration should succeed");

        // Then the session copies into the derived bucket verbatim — the
        // top-level wire log already satisfies the rust layout, no
        // promotion happens, and the copy verifies
        let bucket = encode_work_dir_key(Path::new("/w/flat"));
        let migrated = target.join(&bucket).join("session_flat");
        assert_eq!(totals.copied, 1);
        assert_eq!(totals.unresumable, 0);
        assert_eq!(
            std::fs::read_to_string(migrated.join("wire.jsonl")).expect("read wire log"),
            "{\"type\":\"metadata\",\"protocol_version\":\"2.0\"}\n"
        );
        assert!(!migrated.join("agents").exists());
    }

    #[tokio::test]
    async fn should_report_a_session_without_a_wire_log_as_not_resumable() {
        // Given a TS-layout session whose wire log is missing entirely (a
        // pruned logs dir), only state.json remains
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        let session_dir = source.join("wd_old_000000000001").join("session_bare");
        std::fs::create_dir_all(&session_dir).expect("create session dir should succeed");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"createdAt":1789373829678,"workDir":"/w/bare"}"#,
        )
        .expect("write state.json should succeed");

        // When the migration runs
        let totals = run_migrate_inner(
            &[(source, target.clone())],
            &home.path().join(WORKSPACE_REGISTRY_FILENAME),
            false,
        )
        .await
        .expect("migration should succeed");

        // Then the session is copied (state survives) but reported as not
        // resumable — the verification names the missing wire log
        let bucket = encode_work_dir_key(Path::new("/w/bare"));
        assert_eq!(totals.copied, 1);
        assert_eq!(totals.unresumable, 1);
        assert!(
            target
                .join(&bucket)
                .join("session_bare")
                .join("state.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn should_not_resurrect_a_tombstoned_workspace_when_registering() {
        // Given a registry whose tombstone list marks the migrated
        // session's workspace deleted
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        make_ts_session(&source, "wd_old_000000000001", "session_ts");
        let registry_path = home.path().join(WORKSPACE_REGISTRY_FILENAME);
        let bucket = encode_work_dir_key(Path::new("/w/proj"));
        std::fs::write(
            &registry_path,
            format!(r#"{{"version":1,"workspaces":{{}},"deleted_workspace_ids":["{bucket}"]}}"#),
        )
        .expect("write registry should succeed");

        // When the migration runs
        let totals = run_migrate_inner(&[(source, target)], &registry_path, false)
            .await
            .expect("migration should succeed");

        // Then the sessions copy, the registration is refused by the
        // tombstone, and the row stays absent
        assert_eq!(totals.copied, 1);
        assert_eq!(totals.registered, 0);
        assert_eq!(totals.tombstoned, 1);
        assert!(
            read_workspace_registry(&registry_path)
                .iter()
                .all(|row| row.id != bucket)
        );
    }

    #[tokio::test]
    async fn should_write_nothing_when_dry_run_is_set() {
        // Given a temp home with a TS session and no v3 root
        let home = tempfile::tempdir().expect("home tempdir should succeed");
        let source = home.path().join(SESSIONS_SUBDIR);
        let target = home.path().join(V3_SESSIONS_SUBDIR);
        make_ts_session(&source, "wd_old_000000000001", "session_ts");
        let registry_path = home.path().join(WORKSPACE_REGISTRY_FILENAME);

        // When the migration runs with dry_run
        let totals = run_migrate_inner(&[(source, target.clone())], &registry_path, true)
            .await
            .expect("dry-run migration should succeed");

        // Then nothing was written: no v3 root, no registry file
        assert_eq!(totals.copied, 1);
        assert!(!target.exists());
        assert!(!registry_path.exists());
    }
}
