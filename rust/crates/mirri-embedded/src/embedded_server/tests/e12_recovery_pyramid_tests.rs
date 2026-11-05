//! The recovery pyramid's e2e tests (ADR-006 Task 11).
//!
//! These drive `recover_library` against real corrupt libraries on disk —
//! tier 1 (backup restore) and tier 2 (half-rebuild from wire logs).

use std::path::Path;

use crate::embedded_server::recovery_v3::recover_library;
use mirri_session_index::SessionIndex;

/// Seed one session directory with a wire log whose first user message
/// is `title`, plus a matching state.json (the pre-corruption shape).
fn seed_session(
    sessions_root: &Path,
    workspace_id: &str,
    session_id: &str,
    first_user_message: &str,
) {
    let dir = sessions_root.join(workspace_id).join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut log = String::from("{\"type\":\"metadata\",\"protocol_version\":1}\n");
    log.push_str(&format!(
        "{{\"type\":\"context.append_message\",\"message\":{{\"role\":\"user\",\"content\":\"{first_user_message}\"}}}}\n"
    ));
    std::fs::write(dir.join("wire.jsonl"), log).unwrap();
    std::fs::write(
        dir.join("state.json"),
        serde_json::json!({
            "version": 1,
            "title": "seeded",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
        })
        .to_string(),
    )
    .unwrap();
}

/// Corrupt a database file beyond repair (garbage bytes at the header).
fn corrupt(db_path: &Path) {
    std::fs::write(db_path, b"this is not a database, not even close").unwrap();
}

/// Given a healthy library with a backup, When the primary corrupts and
/// recovery runs, Then the backup's rows are back and the library passes
/// the startup check.
#[tokio::test(flavor = "multi_thread")]
async fn should_restore_from_backup_when_primary_corrupt() {
    let home = tempfile::tempdir().unwrap();
    let sessions_root = home.path().join("v3_sessions");
    seed_session(&sessions_root, "wd_a", "session_backup", "hello from the backup tier");
    let db_path = home.path().join("session-index.db");

    // A healthy library carrying the row, then the backup of it.
    let index = SessionIndex::open(&db_path).unwrap();
    let now = chrono::Utc::now();
    index
        .bulk_upsert_sessions(&[mirri_session_index::SessionRow {
            id: "session_backup".to_string(),
            workspace_id: "wd_a".to_string(),
            cwd: None,
            title: Some("the surviving title".to_string()),
            created_at: now,
            updated_at: now,
            archived: false,
            tags: Vec::new(),
            custom: None,
            last_prompt: None,
            state_mtime: None,
            archived_at: None,
            compressed: false,
            compressed_at: None,
            last_used_at: None,
            pre_compressed_size: None,
            pinned: false,
            pinned_at: None,
            pin_color: None,
        }])
        .unwrap();
    drop(index);
    mirri_session_index::backup::backup_library(&db_path).unwrap();

    // When the primary corrupts and the recovery runs.
    corrupt(&db_path);
    let recovered = recover_library(home.path(), &sessions_root).await.unwrap();

    // Then the backup's row is back (title survives — tier 1 restored the
    // actual pages, not a rebuild).
    let row = recovered.get_session("session_backup").unwrap();
    assert_eq!(row.title.as_deref(), Some("the surviving title"));
}

/// Given a corrupt primary AND a corrupt backup, When recovery runs,
/// Then the half-rebuild recreates the rows from the wire logs — the
/// title is the first user message (truncated), the format stamp is
/// written, and the rebuilt library passes `open_with_check`.
#[tokio::test(flavor = "multi_thread")]
async fn should_rebuild_from_wire_logs_when_backup_also_corrupt() {
    let home = tempfile::tempdir().unwrap();
    let sessions_root = home.path().join("v3_sessions");
    seed_session(
        &sessions_root,
        "wd_b",
        "session_rebuild",
        "first user message that should become the title",
    );
    let db_path = home.path().join("session-index.db");

    // A corrupt primary and a corrupt backup — tier 2 is the only way out.
    // (The backup seam's newest generation is `.bak` — the real filename;
    // corrupting any other name would leave the actual backup intact and
    // the "backup also corrupt" premise would not hold.)
    corrupt(&db_path);
    std::fs::write(home.path().join("session-index.db.bak"), b"also not a database").unwrap();

    let recovered = recover_library(home.path(), &sessions_root).await.unwrap();

    // Then the row exists with the first-user-message title.
    let row = recovered.get_session("session_rebuild").unwrap();
    assert_eq!(
        row.title.as_deref(),
        Some("first user message that should become the title")
    );
    assert_eq!(row.workspace_id, "wd_b");
    assert!(!row.archived, "an uncompressed session reads as un-archived");

    // And the stamp is written — the rebuilt library is a first-class
    // citizen (a later boot's migration sees FreshOk).
    assert_eq!(
        recovered.read_meta_value("schema_format").as_deref(),
        Some("2")
    );
    // And it passes the startup check.
    assert!(matches!(
        SessionIndex::open_with_check(&db_path).unwrap(),
        mirri_session_index::OpenOutcome::Ok(_)
    ));
}

/// Given a session with NO wire log at all, When the half-rebuild runs,
/// Then the session contributes no row (nothing survived) and the rest
/// of the library still rebuilds.
#[tokio::test(flavor = "multi_thread")]
async fn should_skip_wire_log_less_directories_during_the_rebuild() {
    let home = tempfile::tempdir().unwrap();
    let sessions_root = home.path().join("v3_sessions");
    seed_session(&sessions_root, "wd_c", "session_alive", "i have a wire log");
    // A directory with a state.json but no wire log — unrecoverable.
    let orphan = sessions_root.join("wd_c").join("session_logless");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(
        orphan.join("state.json"),
        serde_json::json!({"version": 1, "title": "no wire log"}).to_string(),
    )
    .unwrap();
    let db_path = home.path().join("session-index.db");
    corrupt(&db_path);

    let recovered = recover_library(home.path(), &sessions_root).await.unwrap();

    assert!(recovered.get_session("session_alive").is_some());
    assert!(
        recovered.get_session("session_logless").is_none(),
        "a directory with no wire log contributes no row"
    );
}

/// Given a corrupt primary whose wire logs hold one session, When the
/// full bootstrap assembly runs (the production boot path), Then the
/// engine boots through the recovery pyramid instead of panicking and
/// the rebuilt row is queryable through the assembled store.
#[tokio::test(flavor = "multi_thread")]
async fn should_boot_through_the_pyramid_when_the_library_is_corrupt() {
    let home = tempfile::tempdir().unwrap();
    let sessions_root = home.path().join("v3_sessions");
    seed_session(
        &sessions_root,
        "wd_boot",
        "session_boot",
        "the boot-path rebuild title",
    );
    let db_path = home.path().join("session-index.db");
    corrupt(&db_path);

    let workspace = tempfile::tempdir().unwrap();
    let parts = crate::bootstrap::assemble(
        mirri_core::WorkspaceId::new("boot-recovery"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        crate::bootstrap::WorkspacePolicy::UntrustedDefault,
        Vec::new(),
    );

    // Then the engine assembled (no panic) and the row is in the store —
    // verified through the registry's shared index connection (the same
    // one every runtime's store writes flow through).
    let store = parts
        .session_manager
        .persistence()
        .metadata_store()
        .expect("the store is wired")
        .clone();
    let row = store
        .get(&mirri_core::SessionId::new("session_boot"))
        .await
        .expect("the rebuilt row is queryable");
    assert_eq!(row.title, "the boot-path rebuild title");
}
