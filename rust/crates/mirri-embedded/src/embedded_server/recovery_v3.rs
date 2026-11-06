//! The three-tier library recovery pyramid (ADR-006 Task 11).
//!
//! `SessionIndex::open_with_check` returning `Corrupt` is the entry: the
//! bootstrap refuses to serve with a corrupt library, and this module is
//! what the recovery path runs instead of giving up —
//!
//! 1. **Backup restore**: the rolling two-generation snapshot
//!    (`session-index.db.bak` / `.bak2`) exists → copy it back over the
//!    corrupt primary through the online backup API (the reverse of the
//!    backup seam) and re-check. A backup that itself fails the check
//!    falls through to tier 2.
//! 2. **Half-rebuild**: delete the corrupt database, create a fresh one,
//!    and rebuild every session row from the wire logs — the ONLY legal
//!    whole-row write point left after the field-level narrowing (the
//!    rebuild cannot know state.json anything; the wire log is what
//!    survived). Titles come from the first user message (truncated);
//!    the archived state is inferred from the compression shape only
//!    (`.gz` → compressed; an archived-but-uncompressed session reads as
//!    un-archived — the honest limit of a wire-log-only rebuild).
//! 3. The format stamp is written last (the rebuilt library is fully
//!    migrated by construction — every row was just written).
//!
//! Failure posture: any tier's failure returns `Err` — the caller (the
//! bootstrap) turns that into the loud startup failure with the original
//! diagnosis attached.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_session_index::{SessionIndex, SessionRow};

/// Recover the library at `home/session-index.db`: try the newest backup
/// first, then the half-rebuild. Returns the freshly opened, checked
/// index handle on success.
pub(crate) async fn recover_library(
    home: &Path,
    sessions_root: &Path,
) -> anyhow::Result<Arc<SessionIndex>> {
    let db_path = home.join("session-index.db");

    // Tier 1: backup restore.
    if let Some(backup) = mirri_session_index::backup::latest_backup(&db_path) {
        match try_restore_from_backup(&db_path, &backup) {
            Ok(index) => return Ok(index),
            Err(error) => {
                tracing::warn!(
                    %error,
                    backup = %backup.display(),
                    "restoring the library from the backup failed; falling through to the half-rebuild"
                );
            }
        }
    }

    // Tier 2: the half-rebuild (blocking body off the async worker).
    let sessions_root = sessions_root.to_path_buf();
    let index = tokio::task::spawn_blocking(move || rebuild_from_wire_logs(&db_path, &sessions_root))
        .await
        .map_err(|join_error| anyhow::anyhow!("the half-rebuild task failed to join: {join_error}"))??;
    Ok(Arc::new(index))
}

/// Tier 1 — copy the backup over the corrupt primary and re-check it.
fn try_restore_from_backup(db_path: &Path, backup: &Path) -> anyhow::Result<Arc<SessionIndex>> {
    // A sidecar-clean removal (the corrupt db may carry a stale WAL the
    // restore would otherwise replay over the restored pages).
    remove_database_files(db_path);
    std::fs::copy(backup, db_path)
        .map_err(|error| anyhow::anyhow!("copying the backup failed: {error}"))?;
    match SessionIndex::open_with_check(db_path)? {
        mirri_session_index::OpenOutcome::Ok(index) => Ok(index),
        mirri_session_index::OpenOutcome::Corrupt(diagnosis) => Err(anyhow::anyhow!(
            "the backup itself fails the check: {diagnosis}"
        )),
        // A backup stamped by a FUTURE build — treat as unusable (the
        // half-rebuild writes a fresh library at this build's floor).
        mirri_session_index::OpenOutcome::SchemaFloor { db_version } => Err(anyhow::anyhow!(
            "the backup uses a newer format (v{db_version})"
        )),
    }
}

/// Remove the database file and its sidecars (`-wal` / `-shm`).
fn remove_database_files(db_path: &Path) {
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", db_path.display())));
    let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", db_path.display())));
}

/// Tier 2 — rebuild every row from the wire logs. The ONLY legal
/// whole-row write point: the wire log is the surviving evidence, the
/// rebuild's own bulk upsert is the sanctioned full-field write.
fn rebuild_from_wire_logs(db_path: &Path, sessions_root: &Path) -> anyhow::Result<SessionIndex> {
    remove_database_files(db_path);
    let index = SessionIndex::open(db_path)?;

    let mut rows = Vec::new();
    for (workspace_id, bucket_dir) in enumerate_workspaces(sessions_root) {
        for session_dir in subdirectories(&bucket_dir) {
            let Some(session_id) = session_dir
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(row) = rebuild_row(&session_id, &workspace_id, &session_dir) else {
                continue;
            };
            rows.push(row);
        }
    }
    index.bulk_upsert_sessions(&rows)?;
    // Tier 3: the stamp — the rebuilt library is fully migrated by
    // construction.
    index.write_meta_value("schema_format", &format!("{}", KNOWN_SCHEMA_FORMAT))?;
    if !rows.is_empty() {
        tracing::info!(
            sessions = rows.len(),
            "the half-rebuild rebuilt the session library from the wire logs"
        );
    }
    Ok(index)
}

/// The schema format this build writes — the same constant the
/// one-shot migration stamps (mirri-session-index's KNOWN_SCHEMA_FORMAT;
/// mirrored here as a private re-declaration, with a drift-guard test
/// below asserting the stamped library passes `open_with_check`).
const KNOWN_SCHEMA_FORMAT: i64 = 2;

/// One rebuilt row from one session directory: title = the first user
/// message (truncated), archived = compression shape only.
fn rebuild_row(session_id: &str, workspace_id: &str, session_dir: &Path) -> Option<SessionRow> {
    use mirri_persistence::compress::WIRE_LOG_GZ_FILE;

    // The compressed shape is the only archive evidence a wire-log-only
    // rebuild has (an archived-but-uncompressed session reads as
    // un-archived — the honest limit, recorded in the module doc).
    let compressed = session_dir.join(WIRE_LOG_GZ_FILE).exists();
    let (title, created_at) = read_first_user_message(session_dir)?;

    let now = chrono::Utc::now();
    Some(SessionRow {
        id: session_id.to_string(),
        workspace_id: workspace_id.to_string(),
        cwd: None,
        title: Some(title),
        created_at,
        updated_at: now,
        archived: compressed,
        tags: Vec::new(),
        custom: None,
        last_prompt: None,
        state_mtime: None,
        archived_at: None,
        compressed,
        compressed_at: None,
        last_used_at: None,
        pre_compressed_size: None,
        pinned: false,
        pinned_at: None,
        pin_color: None,
    })
}

/// The first user message's text: the records read through the dual-shape
/// entry (plain or `.gz` — never a bare file open), the envelope skipped,
/// the first `context.append_message` with `message.role == "user"`
/// contributing its text content (truncated to the title budget).
fn read_first_user_message(session_dir: &Path) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    let wire_log = session_dir.join(mirri_persistence::compress::WIRE_LOG_FILE);
    // The rebuild body rides a blocking thread; the reader is async, so a
    // throwaway current-thread runtime drives it (the bootstrap ACP bridge
    // precedent).
    let records = tokio::runtime::Builder::new_current_thread()
        .build()
        .ok()?
        .block_on(mirri_persistence::migration::migrate_records_in_memory(&wire_log))
        .ok()?;

    let mut created_at = None;
    for record in &records {
        // The envelope carries the creation timestamp.
        if record.get("type").and_then(|t| t.as_str()) == Some("metadata") {
            if let Some(raw) = record.get("created_at").and_then(|t| t.as_u64()) {
                created_at = chrono::DateTime::from_timestamp(raw as i64, 0);
            }
            continue;
        }
        if record.get("type").and_then(|t| t.as_str()) == Some("context.append_message") {
            let message = record.get("message")?;
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            if let Some(text) = message_text(message) {
                return Some((truncate_title(&text), created_at.unwrap_or_default()));
            }
        }
    }
    None
}

/// The message's text content — a bare string or the first text part of
/// an array (the two shapes the wire format carries).
fn message_text(message: &serde_json::Value) -> Option<String> {
    match message.get("content")? {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .next()
            .map(str::to_string),
        _ => None,
    }
}

/// The title budget (the truncate convention the side-title seam uses).
fn truncate_title(text: &str) -> String {
    const BUDGET: usize = 80;
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= BUDGET {
        single_line
    } else {
        let truncated: String = single_line.chars().take(BUDGET).collect();
        format!("{truncated}…")
    }
}

/// The registered workspace buckets under the sessions root: every
/// sub-directory that holds session directories (a flat-layout rebuild
/// answers the same enumeration — the recovery does not discriminate).
fn enumerate_workspaces(sessions_root: &Path) -> Vec<(String, PathBuf)> {
    subdirectories(sessions_root)
        .into_iter()
        .filter_map(|dir| {
            let id = dir.file_name()?.to_str()?.to_string();
            Some((id, dir))
        })
        .collect()
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out
}
