//! The rolling two-generation backup discipline for `session-index.db`
//! (ADR-006).
//!
//! The index is the metadata source of truth, so two points in the engine's
//! life take a snapshot of it: right before a library migration (a destructive
//! step must be able to roll back) and on graceful shutdown. The discipline is
//! **rolling and two-deep** — the snapshot just written becomes `<db>.bak`,
//! the previous one slides to `<db>.bak2`, and anything older is discarded, so
//! the home carries at most two generations and at most 2× the database's
//! size. `<db>` here is the database path itself, extension included:
//! `session-index.db` → `session-index.db.bak` / `session-index.db.bak2`.
//!
//! ## Why the online backup API, not a file copy
//!
//! The index runs in WAL mode, so the database's committed state lives partly
//! in `<db>-wal`: a plain `std::fs::copy` of the main file silently drops
//! every transaction the last checkpoint did not fold in, and copying while a
//! writer runs can tear a page mid-write. [`rusqlite::backup::Backup`] runs
//! SQLite's own online backup API against a live source connection, which
//! produces a transactionally consistent snapshot (it restarts if the source
//! is written mid-copy) without stopping the engine.
//!
//! ## Failure shape
//!
//! The snapshot is staged at `<db>.bak.tmp` and only rotated into place once
//! the copy has completed, so a failed backup **never damages a generation
//! that already exists**: the old `<db>.bak` stays where it is and the stale
//! staging file is removed. A corrupt source database therefore returns `Err`
//! and leaves both generations untouched — judging and recovering a corrupt
//! library is the recovery path's job, not this function's.
//!
//! ## Sidecars never travel with a generation
//!
//! SQLite keeps committed-but-uncheckpointed pages in `<db>-wal` (and
//! rollback-mode pages in `<db>-journal`). A `rename` moves the main file
//! only, so a sidecar left beside a generation would stay under the name the
//! rotated file now holds and **replay its pages into the new generation** —
//! silently mixing two generations' rows in a database whose
//! `PRAGMA integrity_check` still reads `ok`. Every slot the rotation writes
//! therefore has its `-wal` / `-shm` / `-journal` removed explicitly, before
//! the main file lands there.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::Connection;
use rusqlite::backup::Backup;

use crate::BUSY_TIMEOUT_MS;

/// Suffix of the newest backup generation (`<db>.bak`).
const NEWEST_SUFFIX: &str = ".bak";

/// Suffix of the previous backup generation (`<db>.bak2`).
const PREVIOUS_SUFFIX: &str = ".bak2";

/// Suffix of the in-flight snapshot. Never a generation: a left-over from a
/// crashed run is discarded before the next backup starts.
const STAGING_SUFFIX: &str = ".bak.tmp";

/// Pages copied per backup step (SQLite's own online-backup example batch
/// size — small enough to yield to competing writers between steps).
const PAGES_PER_STEP: i32 = 100;

/// How long each backup step sleeps before retrying a `Busy` / `Locked`
/// verdict, giving the live engine a window to finish its writes.
const STEP_PAUSE: Duration = Duration::from_millis(5);

/// Snapshot the library at `db_path` into a rolling two-generation backup.
///
/// The new snapshot becomes `<db>.bak`; the generation it displaces slides to
/// `<db>.bak2`; anything older is dropped. The copy itself runs through
/// SQLite's online backup API against a fresh source connection, so it is
/// WAL-consistent and safe while the engine is running.
///
/// The snapshot is written to `<db>.bak.tmp` first and rotated into place
/// only after it completed: a failure (an unreadable or corrupt source, a
/// locked destination, I/O trouble) returns `Err` with both existing
/// generations untouched.
///
/// # Errors
///
/// `NotFound` when there is no database at `db_path` (backing up a file that
/// does not exist would silently produce a non-existent "backup"), and
/// `InvalidData` when the file is zero bytes (SQLite opens an empty file as a
/// fresh database, so the snapshot would be a valid-but-empty library that
/// overwrites a good generation). Every other variant is the storage or
/// filesystem error that caused the failure.
pub fn backup_library(db_path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::metadata(db_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no library database at {}", db_path.display()),
            ));
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no library database at {}", db_path.display()),
        ));
    }
    // An empty file is not a database: SQLite happily opens it as a fresh one
    // and the backup would produce an "empty but valid" snapshot (whose
    // `integrity_check` reads `ok`) that silently replaces a good generation.
    if metadata.len() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the library database at {} is empty", db_path.display()),
        ));
    }

    let staging = generation_path(db_path, STAGING_SUFFIX);
    // A staged file left by a crashed run may carry stale sidecars beside it;
    // all of them are dropped so the destination starts from an empty file.
    remove_database_files(&staging)?;

    if let Err(error) = write_snapshot(db_path, &staging) {
        // Best-effort cleanup: the error that matters is the backup's.
        let _ = remove_database_files(&staging);
        return Err(error);
    }
    // The destination connection is closed by now, but never let a sidecar it
    // left behind travel with the snapshot into a generation slot.
    remove_sidecars(&staging)?;

    // Rotate only now that a complete snapshot exists: the previous newest
    // generation slides down, the staged snapshot takes its place.
    //
    // Sidecars are removed explicitly at both target slots rather than relying
    // on the platform's overwrite semantics: `rename` moves the main file
    // only, so a `-wal` / `-journal` left beside the old `.bak` would stay
    // behind under the very name the newly rotated file now holds and replay
    // its pages into the new generation.
    let newest = generation_path(db_path, NEWEST_SUFFIX);
    let previous = generation_path(db_path, PREVIOUS_SUFFIX);
    if newest.exists() {
        remove_sidecars(&newest)?;
        remove_database_files(&previous)?;
        std::fs::rename(&newest, &previous)?;
    }
    remove_sidecars(&newest)?;
    std::fs::rename(&staging, &newest)?;
    Ok(())
}

/// The most recent backup generation for the library at `db_path`, or `None`
/// when neither `<db>.bak` nor `<db>.bak2` exists.
///
/// Mtime decides which of the two is newer (a restored or hand-copied file
/// can leave `<db>.bak2` the fresher one); a tie resolves to `<db>.bak`, the
/// generation the discipline always writes last.
#[must_use]
pub fn latest_backup(db_path: &Path) -> Option<PathBuf> {
    let newest = generation_path(db_path, NEWEST_SUFFIX);
    let previous = generation_path(db_path, PREVIOUS_SUFFIX);
    match (mtime(&newest), mtime(&previous)) {
        (Some(newest_at), Some(previous_at)) => Some(if newest_at >= previous_at {
            newest
        } else {
            previous
        }),
        (Some(_), None) => Some(newest),
        (None, Some(_)) => Some(previous),
        (None, None) => None,
    }
}

/// Copy `db_path` into `target` through SQLite's online backup API — the
/// WAL-consistent snapshot both the migration and the shutdown seam take.
fn write_snapshot(db_path: &Path, target: &Path) -> std::io::Result<()> {
    let source = Connection::open(db_path)
        .map_err(|error| storage_error("opening the library", db_path, error))?;
    source
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(|error| {
            storage_error("setting the busy timeout on the library", db_path, error)
        })?;

    let mut destination = Connection::open(target)
        .map_err(|error| storage_error("creating the backup file", target, error))?;
    {
        let backup = Backup::new(&source, &mut destination)
            .map_err(|error| storage_error("starting the backup", db_path, error))?;
        backup
            .run_to_completion(PAGES_PER_STEP, STEP_PAUSE, None)
            .map_err(|error| storage_error("running the backup", db_path, error))?;
    }

    // The destination connection is dropped here, so SQLite folds any WAL it
    // wrote back into the file and removes the sidecars before the caller
    // renames the snapshot into place.
    drop(destination);
    Ok(())
}

/// `<db_path><suffix>` — the backup siblings keep the database's own file
/// name, so every generation sits next to the library it came from.
fn generation_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Remove a database file together with its SQLite sidecars. A file that is
/// not there is a no-op.
fn remove_database_files(path: &Path) -> std::io::Result<()> {
    for candidate in database_files(path) {
        remove_file_if_present(&candidate)?;
    }
    Ok(())
}

/// Remove only the sidecars beside `path`, keeping the main file — the guard
/// that stops a stale `-wal` / `-journal` from replaying into whatever main
/// file later occupies that name.
fn remove_sidecars(path: &Path) -> std::io::Result<()> {
    for candidate in database_files(path).iter().skip(1) {
        remove_file_if_present(candidate)?;
    }
    Ok(())
}

/// Remove one file, tolerating its absence.
fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The main file plus its `-wal` / `-shm` / `-journal` sidecars. The rollback
/// journal belongs here alongside the WAL pair: a database left in rollback
/// journal mode (or a generation written before the WAL switch) keeps its
/// uncommitted pages in `-journal`, and that replays into a renamed main file
/// exactly as a stale `-wal` does.
fn database_files(path: &Path) -> [PathBuf; 4] {
    let with_suffix = |suffix: &str| {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        PathBuf::from(candidate)
    };
    [
        path.to_path_buf(),
        with_suffix("-wal"),
        with_suffix("-shm"),
        with_suffix("-journal"),
    ]
}

/// The file's modification time, `None` when it does not exist or the
/// platform cannot report it (an unreadable stamp never hides a generation).
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Wrap a storage error with the path it applies to — the backup reports the
/// `std::io::Error` shape its shutdown / migration callers already handle.
fn storage_error(action: &str, path: &Path, error: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(format!("{action} at {} failed: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::DateTime;

    use crate::{ListOpts, SessionIndex, SessionRow};

    /// One row builder with defaults, so each test spells only its deltas.
    fn session_row(id: &str, generation: i64) -> SessionRow {
        let updated_at = DateTime::from_timestamp(1_700_000_000 + generation, 0).unwrap();
        SessionRow {
            id: id.to_string(),
            workspace_id: "wd_project_abc123def456".to_string(),
            cwd: Some("/Users/dev/project".to_string()),
            title: Some(format!("generation {generation}")),
            created_at: updated_at,
            updated_at,
            archived: false,
            tags: Vec::new(),
            custom: None,
            last_prompt: Some("what is up".to_string()),
            state_mtime: Some(1_700_000_000_000),
            archived_at: None,
            compressed: false,
            compressed_at: None,
            pinned: false,
            pinned_at: None,
            pin_color: None,
            last_used_at: None,
            pre_compressed_size: None,
        }
    }

    /// The ids a backup file carries, ordered — opening the snapshot through
    /// the ordinary index open is part of the assertion: a backup that only
    /// copies bytes but cannot be opened is not a backup.
    fn session_ids(backup: &Path) -> Vec<String> {
        let index = SessionIndex::open(backup).expect("the backup opens as an index");
        let mut ids = index
            .list_sessions(ListOpts::default())
            .expect("the backup lists its sessions")
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// The two generation main files present in the library's directory,
    /// sorted — matched exactly, so a sidecar (`.bak-wal`, `.bak-shm`,
    /// `.bak2-journal`) or a staged `.bak.tmp` is never mistaken for a
    /// generation.
    fn generation_file_names(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".bak") || name.ends_with(".bak2"))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Leave a `-wal` beside `backup` carrying an extra row that is **not** in
    /// the backup's main file — the shape a crashed writer or an abandoned
    /// open leaves behind.
    ///
    /// A hand-written WAL is useless (SQLite checksums its header and
    /// discards anything it did not write), so the row is committed for real
    /// and the resulting WAL is harvested **while the writing connection is
    /// still open** — closing it would checkpoint and delete the sidecar.
    /// The main file is then restored to its pre-commit bytes and the
    /// harvested `-wal` is put back: reading the backup replays the row,
    /// exactly what a crash between commit and checkpoint leaves on disk.
    fn plant_stale_wal(backup: &Path, stale_row: &SessionRow) {
        let wal_path = generation_path(backup, "-wal");
        let clean_main = std::fs::read(backup).unwrap();

        let wal_bytes = {
            let index = crate::SessionIndex::open(backup).unwrap();
            index.upsert_session(stale_row).unwrap();
            std::fs::read(&wal_path).expect("a committed WAL-mode write leaves a -wal file")
        };

        std::fs::write(backup, clean_main).unwrap();
        std::fs::write(&wal_path, wal_bytes).unwrap();

        // The planted WAL must genuinely replay, or the test proves nothing.
        // Verified on a throwaway copy so the planting itself is not consumed.
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("replay.db");
        std::fs::copy(backup, &copy).unwrap();
        std::fs::copy(&wal_path, generation_path(&copy, "-wal")).unwrap();
        assert!(
            session_ids(&copy).contains(&stale_row.id),
            "the planted WAL replays its row on the next open"
        );
    }

    /// Given a live library backed up three times with data changing between
    /// runs, When the rotations complete, Then `.bak` holds the newest
    /// generation, `.bak2` the one before it, the oldest generation is gone
    /// (exactly two generations on disk), and both open as indexes carrying
    /// the rows they were snapshotted with.
    #[test]
    fn should_rotate_backups_and_keep_two_generations_when_backed_up_thrice() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        let index = SessionIndex::open(&db_path).unwrap();

        index.upsert_session(&session_row("sess-gen1", 1)).unwrap();
        backup_library(&db_path).unwrap();

        index.upsert_session(&session_row("sess-gen2", 2)).unwrap();
        backup_library(&db_path).unwrap();

        index.upsert_session(&session_row("sess-gen3", 3)).unwrap();
        backup_library(&db_path).unwrap();

        // Two generations, never three: the first one was dropped, not
        // shifted into a `.bak3`.
        assert_eq!(
            generation_file_names(dir.path()),
            vec![
                "session-index.db.bak".to_string(),
                "session-index.db.bak2".to_string(),
            ]
        );

        let newest = generation_path(&db_path, NEWEST_SUFFIX);
        let previous = generation_path(&db_path, PREVIOUS_SUFFIX);
        assert_eq!(latest_backup(&db_path), Some(newest.clone()));

        // `.bak` = the third generation, `.bak2` = the second.
        assert_eq!(
            session_ids(&newest),
            vec![
                "sess-gen1".to_string(),
                "sess-gen2".to_string(),
                "sess-gen3".to_string(),
            ]
        );
        assert_eq!(
            session_ids(&previous),
            vec!["sess-gen1".to_string(), "sess-gen2".to_string()]
        );
    }

    /// Given a good `.bak` left with a stale `-wal` (a crashed writer's
    /// uncheckpointed row) and the same beside `.bak2`, When the next backup
    /// rotates, Then neither generation replays the other's leftover rows:
    /// `.bak` carries exactly the newest generation and `.bak2` exactly the
    /// previous one. Without sidecar cleanup the stale WAL rides along on the
    /// renamed main file and silently mixes generations — while
    /// `PRAGMA integrity_check` still reports `ok`, so the recovery path
    /// could never notice.
    #[test]
    fn should_not_mix_generations_when_stale_wal_sidecars_remain() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        let index = SessionIndex::open(&db_path).unwrap();

        // Generation 1: the previous generation that rotation will move down.
        index.upsert_session(&session_row("sess-gen1", 1)).unwrap();
        backup_library(&db_path).unwrap();
        let newest = generation_path(&db_path, NEWEST_SUFFIX);
        plant_stale_wal(&newest, &session_row("sess-stale-on-bak", 90));

        // Generation 2: also displace a previous `.bak2` carrying its own
        // stale sidecar, so both targets of the rotation are poisoned.
        index.upsert_session(&session_row("sess-gen2", 2)).unwrap();
        backup_library(&db_path).unwrap();
        let previous = generation_path(&db_path, PREVIOUS_SUFFIX);
        plant_stale_wal(&previous, &session_row("sess-stale-on-bak2", 91));
        plant_stale_wal(&newest, &session_row("sess-stale-on-bak-2", 92));

        // Generation 3: the rotation under test.
        index.upsert_session(&session_row("sess-gen3", 3)).unwrap();
        backup_library(&db_path).unwrap();

        assert_eq!(
            session_ids(&newest),
            vec![
                "sess-gen1".to_string(),
                "sess-gen2".to_string(),
                "sess-gen3".to_string(),
            ],
            "the new `.bak` carries only its own generation"
        );
        assert_eq!(
            session_ids(&previous),
            vec!["sess-gen1".to_string(), "sess-gen2".to_string()],
            "the demoted `.bak2` carries only its own generation"
        );

        // No sidecar survives beside either generation.
        for generation in [&newest, &previous] {
            for sidecar in ["-wal", "-shm", "-journal"] {
                assert!(
                    !generation_path(generation, sidecar).exists(),
                    "{} left a {sidecar} behind",
                    generation.display()
                );
            }
        }
    }

    /// Given a zero-byte file where the library should be, When a backup is
    /// requested, Then it fails and no generation is touched: an empty source
    /// would otherwise snapshot into an "empty but valid" database that
    /// overwrites a good generation (and `integrity_check` would call it ok).
    #[test]
    fn should_return_an_error_when_the_library_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        let index = SessionIndex::open(&db_path).unwrap();
        index.upsert_session(&session_row("sess-gen1", 1)).unwrap();
        backup_library(&db_path).unwrap();
        let good = generation_path(&db_path, NEWEST_SUFFIX);
        let good_bytes = std::fs::read(&good).unwrap();

        std::fs::write(&db_path, b"").unwrap();

        assert!(
            backup_library(&db_path).is_err(),
            "an empty source is reported, never snapshotted"
        );
        assert_eq!(
            std::fs::read(&good).unwrap(),
            good_bytes,
            "the good generation survives byte for byte"
        );
        assert!(!generation_path(&db_path, STAGING_SUFFIX).exists());
    }

    /// Given a library that was never backed up, When the latest generation
    /// is asked for, Then there is none.
    #[test]
    fn should_return_no_backup_when_none_was_ever_written() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");

        assert_eq!(latest_backup(&db_path), None);
    }

    /// Given a `.bak2` that is newer than `.bak` (a restored or hand-copied
    /// generation), When the latest generation is asked for, Then mtime
    /// decides, not the file name.
    #[test]
    fn should_return_the_newer_generation_when_bak2_is_fresher_than_bak() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        let newest = generation_path(&db_path, NEWEST_SUFFIX);
        let previous = generation_path(&db_path, PREVIOUS_SUFFIX);
        std::fs::write(&newest, b"older").unwrap();
        // A whole second of separation defeats filesystem mtime granularity.
        std::thread::sleep(Duration::from_millis(1_100));
        std::fs::write(&previous, b"newer").unwrap();

        assert_eq!(latest_backup(&db_path), Some(previous));
    }

    /// Given a live library whose newest rows are still only in the `-wal`
    /// file (no checkpoint has run), When the backup is taken with the index
    /// still open, Then the snapshot carries those rows — the online backup
    /// API reads the committed WAL state, where a plain file copy of the main
    /// database would silently drop them.
    #[test]
    fn should_include_uncheckpointed_wal_rows_when_backing_up_a_live_library() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        let index = SessionIndex::open(&db_path).unwrap();

        index.upsert_session(&session_row("sess-wal", 1)).unwrap();
        // The write is committed but not yet folded into the main file.
        let wal_bytes = std::fs::metadata(generation_path(&db_path, "-wal"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert!(
            wal_bytes > 0,
            "the test needs the row to still live in the WAL, not the main file"
        );

        backup_library(&db_path).unwrap();

        assert_eq!(
            session_ids(&generation_path(&db_path, NEWEST_SUFFIX)),
            vec!["sess-wal".to_string()]
        );
    }

    /// Given a good backup generation and a source database that is no longer
    /// a database, When the next backup runs, Then it fails and the good
    /// generation survives — a corrupt library never destroys the backup the
    /// recovery path depends on.
    #[test]
    fn should_keep_the_existing_generation_when_the_source_library_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");
        {
            let index = SessionIndex::open(&db_path).unwrap();
            index.upsert_session(&session_row("sess-gen1", 1)).unwrap();
        }
        backup_library(&db_path).unwrap();
        let good = generation_path(&db_path, NEWEST_SUFFIX);

        std::fs::write(&db_path, b"this is not a sqlite database").unwrap();

        assert!(
            backup_library(&db_path).is_err(),
            "a corrupt source is reported, not silently snapshotted"
        );
        assert_eq!(session_ids(&good), vec!["sess-gen1".to_string()]);
        assert_eq!(latest_backup(&db_path), Some(good));
        assert!(
            !generation_path(&db_path, STAGING_SUFFIX).exists(),
            "the failed attempt leaves no staging file behind"
        );
    }

    /// Given a path with no database behind it, When a backup is requested,
    /// Then it fails rather than producing an empty snapshot.
    #[test]
    fn should_return_not_found_when_the_library_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("session-index.db");

        let error = backup_library(&db_path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!generation_path(&db_path, NEWEST_SUFFIX).exists());
    }
}
