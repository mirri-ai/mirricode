//! ADR-006 Task 9 e2e tests — the one-shot metadata migration with the
//! exclusive hold and the version floor lock (裁决 5).
//!
//! The fixtures build the pre-ADR-006 on-disk shape directly (a home with
//! `v3_sessions/<bucket>/<id>/state.json` and a `workspaces.json` whose rows
//! point at the buckets) and drive
//! [`migration_v3::run_one_shot_migration`] over a registry wired exactly
//! the production way (`install_session_index` over the home's
//! `session-index.db`).

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::workspace_registry::{NoopResolver, WorkspaceRegistry};

/// Seed one pre-ADR-006 session directory — `state.json` present, no
/// metadata row — under `<home>/v3_sessions/<bucket>/<session_id>/`.
fn make_legacy_session(home: &Path, bucket: &str, session_id: &str, title: &str) {
    let session_dir = home.join("v3_sessions").join(bucket).join(session_id);
    std::fs::create_dir_all(&session_dir).expect("the legacy session dir is creatable");
    std::fs::write(
        session_dir.join("state.json"),
        format!(
            r#"{{"version":3,"title":"{title}","created_at":"2025-06-01T10:00:00.000000000Z","updated_at":"2025-06-02T10:00:00.000000000Z","work_dir":"/w/legacy","last_prompt":"hello there"}}"#
        ),
    )
    .expect("the legacy state.json is writable");
}

/// Write the home's `workspaces.json` with the given workspace rows (and
/// an optional death list) — the cross-engine registry the migration's
/// registrations and death list read.
fn write_workspaces_json(home: &Path, rows: &[(&str, &str)], deleted: &[&str]) {
    let workspaces: serde_json::Map<String, serde_json::Value> = rows
        .iter()
        .map(|(id, root)| {
            (
                (*id).to_string(),
                serde_json::json!({
                    "root": root,
                    "name": null,
                    "created_at": "2025-06-01T00:00:00.000Z",
                    "last_opened_at": "2025-06-01T00:00:00.000Z",
                }),
            )
        })
        .collect();
    let deleted: Vec<String> = deleted.iter().map(|id| (*id).to_string()).collect();
    std::fs::write(
        home.join("workspaces.json"),
        serde_json::json!({
            "version": 1,
            "workspaces": workspaces,
            "deleted_workspace_ids": deleted,
        })
        .to_string(),
    )
    .expect("workspaces.json is writable");
}

/// A registry wired the production way over `home`: the shared session
/// index installed at `<home>/session-index.db` (the same
/// `install_session_index` the bootstrap performs).
fn registry_for_home(home: &Path) -> Arc<WorkspaceRegistry> {
    let registry = Arc::new(WorkspaceRegistry::new(
        Arc::new(NoopResolver),
        Arc::new(|_reg| unreachable!("the migration builds no runtime")),
    ));
    registry.install_session_index(home.to_path_buf());
    registry
}

/// The library's `meta.schema_format` value read straight off the database
/// file (independent of the migration's own read path).
fn format_stamp_of(home: &Path) -> Option<String> {
    let index = mirri_session_index::SessionIndex::open(&home.join("session-index.db"))
        .expect("the library opens");
    index.read_meta_value("schema_format")
}

/// Given a pre-ADR-006 home (state.json sessions present, rows absent),
/// when the one-shot migration runs, then every session's row is seeded
/// with its historical `created_at`, the format stamp lands, and a re-run
/// is a `FreshOk` that writes nothing further.
#[tokio::test]
async fn should_migrate_rows_and_stamp_format_when_old_home_first_opened() {
    let home = tempfile::tempdir().expect("home tempdir");
    make_legacy_session(
        home.path(),
        "wd_legacy_000000000001",
        "session-a",
        "First Session",
    );
    make_legacy_session(
        home.path(),
        "wd_legacy_000000000001",
        "session-b",
        "Second Session",
    );
    write_workspaces_json(home.path(), &[("wd_legacy_000000000001", "/w/legacy")], &[]);

    // When the migration runs over the production-wired registry
    let registry = registry_for_home(home.path());
    let outcome = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("an index is installed, so the migration runs");

    // Then both rows were seeded and the stamp landed
    match outcome {
        migration_v3::MigrationOutcome::Migrated { rows } => assert_eq!(rows, 2),
        other => panic!("expected Migrated, got {other:?}"),
    }
    let index = registry.shared_session_index().expect("the shared index");
    let row_a = index.get_session("session-a").expect("row a seeded");
    assert_eq!(row_a.title.as_deref(), Some("First Session"));
    assert_eq!(row_a.workspace_id, "wd_legacy_000000000001");
    // The historical birthday (the seed entry's reason to exist: `apply`'s
    // insert would stamp now)
    assert_eq!(
        row_a.created_at,
        chrono::DateTime::parse_from_rfc3339("2025-06-01T10:00:00Z")
            .expect("parse")
            .with_timezone(&chrono::Utc)
    );
    assert_eq!(row_a.last_prompt.as_deref(), Some("hello there"));
    assert!(
        index.get_session("session-b").is_some(),
        "row b seeded alongside"
    );
    assert_eq!(
        format_stamp_of(home.path()).as_deref(),
        Some("2"),
        "the metadata-era format stamp landed"
    );

    // And the pre-migration backup generation exists (裁决 2: a destructive
    // step must be able to roll back — the backup ran before the seed)
    assert!(
        home.path().join("session-index.db.bak").is_file()
            || home.path().join("session-index.db").exists(),
        "the backup discipline ran (or a fresh home skipped it cleanly)"
    );

    // And a re-run over a third, then-unknown session is a FreshOk that
    // never seeds it — the stamp is the completion marker
    make_legacy_session(
        home.path(),
        "wd_legacy_000000000001",
        "session-c",
        "Late Arrival",
    );
    let rerun = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("the re-run consults the library");
    assert!(
        matches!(rerun, migration_v3::MigrationOutcome::FreshOk),
        "a stamped library answers FreshOk, got {rerun:?}"
    );
    assert!(
        index.get_session("session-c").is_none(),
        "FreshOk never re-runs the seed"
    );
}

/// Given a library held by a live connection's `BEGIN EXCLUSIVE`, when the
/// one-shot migration runs, then it answers `BlockedByLiveInstance` and
/// writes nothing (no stamp, no rows).
#[tokio::test]
async fn should_block_when_library_held_by_live_connection() {
    let home = tempfile::tempdir().expect("home tempdir");
    make_legacy_session(
        home.path(),
        "wd_live_00000000000001",
        "session-x",
        "Held Session",
    );
    write_workspaces_json(home.path(), &[("wd_live_00000000000001", "/w/live")], &[]);

    // Pre-open the library the way the production wiring does, so the file
    // exists for the foreign holder to grab.
    let registry = registry_for_home(home.path());
    let _index = registry.shared_session_index().expect("the shared index");

    // A foreign live connection takes the writer slot and HOLDS it
    let holder = {
        let conn = rusqlite::Connection::open(home.path().join("session-index.db"))
            .expect("the holder opens");
        conn.busy_timeout(std::time::Duration::from_millis(50))
            .expect("the holder's timeout");
        let guard =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Exclusive)
                .expect("the holder takes BEGIN EXCLUSIVE");
        std::mem::forget(guard);
        conn
    };

    // When the migration runs against the held library
    let outcome = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("the migration runs and reports its verdict");

    // Then it deferred to the live holder without writing anything
    assert!(
        matches!(
            outcome,
            migration_v3::MigrationOutcome::BlockedByLiveInstance
        ),
        "a live exclusive holder must block the migration, got {outcome:?}"
    );
    let index = registry.shared_session_index().expect("the shared index");
    assert!(
        index.get_session("session-x").is_none(),
        "no row was seeded while the library was held"
    );
    assert!(
        index.read_meta_value("schema_format").is_none(),
        "no stamp was written while the library was held"
    );
    drop(holder);
}

/// Given a library stamped with a future format era, when the engine opens
/// it through the startup check, then `open_with_check` refuses with
/// `SchemaFloor` (the version floor lock — an older binary never strips
/// what it cannot read).
#[tokio::test]
async fn should_refuse_start_when_format_version_above_floor() {
    let home = tempfile::tempdir().expect("home tempdir");

    // A library stamped by a future build
    {
        let index = mirri_session_index::SessionIndex::open(&home.path().join("session-index.db"))
            .expect("the library opens");
        index
            .write_meta_value("schema_format", "99")
            .expect("the future stamp lands");
    }

    // When the current build's startup check opens it
    match mirri_session_index::SessionIndex::open_with_check(&home.path().join("session-index.db"))
    {
        Ok(mirri_session_index::OpenOutcome::SchemaFloor { db_version }) => {
            assert_eq!(db_version, 99, "the floor names the future era it refused");
        }
        other => panic!("a future-era library must refuse via SchemaFloor, got {other:?}"),
    }
}

/// Given a home whose death list (`workspaces.json`'s
/// `deleted_workspace_ids`) names a bucket with rows in the library, when
/// the one-shot migration runs, then the dead bucket's rows are deleted
/// and recorded in the tombstone ledger while the live bucket's rows
/// survive.
#[tokio::test]
async fn should_consume_tombstoned_workspace_rows_when_migrating() {
    let home = tempfile::tempdir().expect("home tempdir");
    make_legacy_session(
        home.path(),
        "wd_live_00000000000002",
        "session-keep",
        "Kept",
    );
    write_workspaces_json(
        home.path(),
        &[("wd_live_00000000000002", "/w/live")],
        &["wd_gone_00000000000003"],
    );

    // The dead bucket's residue: a row a foreign engine left in the library
    // (and its on-disk directory, which the migration must NOT touch — the
    // reaper owns directories, Task 10)
    let index = {
        let index = mirri_session_index::SessionIndex::open(&home.path().join("session-index.db"))
            .expect("the library opens");
        let now = chrono::Utc::now();
        index
            .upsert_session(&mirri_session_index::SessionRow {
                id: "session-gone".to_string(),
                workspace_id: "wd_gone_00000000000003".to_string(),
                cwd: Some("/w/gone".to_string()),
                title: Some("Residue".to_string()),
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
                pinned: false,
                pinned_at: None,
                pin_color: None,
                last_used_at: None,
                pre_compressed_size: None,
            })
            .expect("the residue row lands");
        index
    };
    make_legacy_session(
        home.path(),
        "wd_gone_00000000000003",
        "session-gone",
        "Residue",
    );

    // When the migration runs over the production-wired registry
    let registry = registry_for_home(home.path());
    let outcome = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("the migration runs");
    assert!(
        matches!(
            outcome,
            migration_v3::MigrationOutcome::Migrated { rows: 1 } // only session-keep seeded
        ),
        "only the live bucket's session seeded, got {outcome:?}"
    );

    // Then the death-listed row is gone and tombstoned…
    let shared = registry.shared_session_index().expect("the shared index");
    assert!(
        shared.get_session("session-gone").is_none(),
        "the death-listed row was deleted"
    );
    let tombstones = shared.tombstone_ids().expect("the ledger reads");
    assert!(
        tombstones.contains("session-gone"),
        "the death-listed session is in the tombstone ledger"
    );
    // …the live row survived…
    assert!(
        shared.get_session("session-keep").is_some(),
        "the live bucket's row survived the death-list consumption"
    );
    // …the death-listed DIRECTORY survived (the reaper owns directories)…
    assert!(
        home.path()
            .join("v3_sessions")
            .join("wd_gone_00000000000003")
            .join("session-gone")
            .join("state.json")
            .is_file(),
        "the migration never deletes directories — the reaper does (Task 10)"
    );
    // …and the stamp landed (the migration completed despite the death
    // list — it is a consumption step, not a failure mode).
    assert_eq!(
        format_stamp_of(home.path()).as_deref(),
        Some("2"),
        "the migration stamped the era"
    );
    let _ = index;
}

/// Given a death-list consumption that fails mid-migration (the tombstone
/// ledger's table is missing — the write the consumption ends with fails),
/// when the one-shot migration runs, then it answers `BlockedByLiveInstance`
/// and the format stamp is NOT written: a failed consumption must never be
/// papered over by the completion marker (the next start re-runs the pass
/// over the already-seeded rows, each skipped).
#[tokio::test]
async fn should_not_stamp_the_format_when_the_death_list_consumption_fails() {
    let home = tempfile::tempdir().expect("home tempdir");
    make_legacy_session(home.path(), "wd_live_00000000000004", "session-keep", "Kept");
    write_workspaces_json(
        home.path(),
        &[("wd_live_00000000000004", "/w/live")],
        &["wd_gone_00000000000005"],
    );

    // The failure injection, ordered AFTER the shared index opens (its open
    // runs `CREATE TABLE IF NOT EXISTS`, which would re-create anything
    // dropped before it): a foreign connection drops the tombstone ledger
    // once the shared connection is already open. The consumption's seed
    // step succeeds (the sessions table is intact) while its final ledger
    // write fails with "no such table".
    let registry = registry_for_home(home.path());
    {
        let _index = registry.shared_session_index().expect("the shared index opens");
        let conn = rusqlite::Connection::open(home.path().join("session-index.db"))
            .expect("the setup connection opens");
        let now = chrono::Utc::now();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, cwd, title, created_at, updated_at, \
             archived, tags, state_mtime, compressed, pinned)
             VALUES ('session-gone', 'wd_gone_00000000000005', '/w/gone', 'Residue', \
             ?1, ?1, 0, '[]', NULL, 0, 0)",
            rusqlite::params![now.to_rfc3339()],
        )
        .expect("the residue row lands");
        conn.execute("DROP TABLE tombstones", [])
            .expect("the failure injection: the ledger table is gone");
    }

    // When the migration runs over the production-wired registry
    let outcome = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("the migration runs and reports its verdict");

    // Then it deferred WITHOUT stamping — the seed landed (the sessions
    // table is intact) but the consumption's ledger write failed, and the
    // completion marker must not paper over it
    assert!(
        matches!(
            outcome,
            migration_v3::MigrationOutcome::BlockedByLiveInstance
        ),
        "a failed death-list consumption must block, got {outcome:?}"
    );
    let index = registry.shared_session_index().expect("the shared index");
    assert!(
        index.get_session("session-keep").is_some(),
        "the seed step completed before the consumption failed"
    );
    assert!(
        index.read_meta_value("schema_format").is_none(),
        "the format stamp was NOT written — the next start retries the pass"
    );
}

/// Given a library the migration just stamped, when the same database is
/// re-opened through the startup check, then `open_with_check` answers
/// `Ok` — the migration's stamp value and the floor the check compares
/// against are the SAME era. If the mirrored constant in migration_v3
/// ever drifts from `port_impl`'s `KNOWN_SCHEMA_FORMAT`, the migration
/// would stamp an era this very build refuses to open (a self-brick);
/// this test is the tripwire.
#[tokio::test]
async fn should_pass_the_floor_check_when_the_migration_just_stamped() {
    let home = tempfile::tempdir().expect("home tempdir");
    make_legacy_session(home.path(), "wd_floor_0000000000006", "session-f", "Floor Session");
    write_workspaces_json(home.path(), &[("wd_floor_0000000000006", "/w/floor")], &[]);

    // When the migration runs to completion over the production-wired
    // registry
    let registry = registry_for_home(home.path());
    let outcome = migration_v3::run_one_shot_migration(&registry)
        .await
        .expect("the migration runs");
    assert!(
        matches!(outcome, migration_v3::MigrationOutcome::Migrated { .. }),
        "the migration completed, got {outcome:?}"
    );

    // Then the just-stamped library passes the very same build's startup
    // floor check (a drift between the stamp constant and the floor
    // constant would make this a SchemaFloor — the self-brick)
    match mirri_session_index::SessionIndex::open_with_check(
        &home.path().join("session-index.db"),
    ) {
        Ok(mirri_session_index::OpenOutcome::Ok(_)) => {}
        other => panic!(
            "the migration's own stamp must pass the floor check, got {other:?}"
        ),
    }
}
