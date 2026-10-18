//! Small filesystem helpers shared across the persistence stores.

use std::path::Path;

/// Fsync a directory so entry creations/renames inside it are durable.
///
/// Without this, a crash right after an append or rename can leave the file
/// system pointing at the old directory state even though the file data was
/// synced. Directory fsync only exists on Unix; on other platforms (e.g.
/// Windows) directory durability is handled by the OS and this is a no-op.
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// No-op fallback for platforms without directory fsync (e.g. Windows).
#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Durable atomic file replace, the single canonical write path for
/// user-level state files (configs, token stores, task records, plugin
/// registries).
///
/// Write flow:
/// 1. Create the parent directory if missing.
/// 2. Write `contents` to a unique sibling temp file (`.<name>.<pid>.<nanos>.tmp`),
///    created with mode `0o600` on Unix, so concurrent writers (processes or
///    threads) never share a temp file.
/// 3. Fsync the temp file so a crash after the rename cannot leave an empty
///    or partial destination.
/// 4. Atomically rename it over `path` (atomic on the same filesystem).
/// 5. Best-effort fsync of the parent directory so the rename itself is
///    durable (a failure here is logged, not fatal).
///
/// On any failure the temp file is removed best-effort and the destination
/// is left untouched, so a failed write never corrupts the previous state.
pub fn write_file_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let pid = std::process::id();

    let mut file = None;
    for _ in 0..2 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            .wrapping_mul(0x9E3779B97F4A7C15);
        let temp_path = path.with_file_name(format!(".{file_name}.{pid}.{nanos:016x}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(opened) => {
                file = Some((opened, temp_path));
                break;
            }
            // A random-name collision is astronomically unlikely; retry once
            // with a fresh timestamp before giving up.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some((mut file, temp_path)) = file else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a unique temp file after retry",
        ));
    };

    let result = (|| {
        use std::io::Write;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(error) = sync_directory(parent)
        {
            tracing::warn!(
                directory = %parent.display(),
                error = %error,
                "best-effort directory fsync after atomic rename failed"
            );
        }
        Ok(())
    })();

    if result.is_err() {
        // Best-effort cleanup so a failed write does not litter the
        // destination directory with stray temp files.
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_sync_existing_directory_when_called() {
        let tmp = tempfile::tempdir().unwrap();
        // On Unix this performs a real directory fsync; on other platforms
        // the no-op fallback also succeeds.
        sync_directory(tmp.path()).expect("directory fsync should succeed");
    }

    #[test]
    fn should_write_full_contents_when_file_replaced_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        write_file_atomically(&path, b"{\"a\":1}").expect("write should succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn should_replace_previous_contents_when_file_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(&path, b"old").unwrap();
        write_file_atomically(&path, b"new contents").expect("write should succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"new contents");
    }

    #[test]
    fn should_not_leave_temp_files_behind_when_write_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        write_file_atomically(&path, b"payload").expect("write should succeed");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files lingered: {leftovers:?}");
    }

    #[test]
    fn should_restrict_file_to_owner_when_written_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("state.json");
            write_file_atomically(&path, b"secret").expect("write should succeed");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn should_create_missing_parent_directories_when_path_is_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c/state.json");
        write_file_atomically(&path, b"nested").expect("write should succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"nested");
    }

    #[test]
    fn should_remove_temp_file_when_write_fails_at_rename() {
        let tmp = tempfile::tempdir().unwrap();
        // An existing non-empty directory at the destination makes the
        // final rename fail, exercising the failure path.
        let path = tmp.path().join("state.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("blocker"), b"x").unwrap();
        let result = write_file_atomically(&path, b"payload");
        assert!(result.is_err(), "write over a directory should fail");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files lingered: {leftovers:?}");
    }

    #[test]
    fn should_keep_contents_intact_when_two_writers_write_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let writer = |contents: &'static [u8]| {
            let dir = tmp.path().to_path_buf();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    write_file_atomically(&dir.join("state.json"), contents)
                        .expect("concurrent write should succeed");
                }
            })
        };
        let a = writer(b"aaaa");
        let b = writer(b"bbbb");
        a.join().unwrap();
        b.join().unwrap();
        // After the race the file must hold one of the two payloads in full —
        // never a torn mix of both.
        let final_contents = std::fs::read(&path).unwrap();
        assert!(
            final_contents == b"aaaa" || final_contents == b"bbbb",
            "torn write detected: {:?}",
            String::from_utf8_lossy(&final_contents)
        );
    }
}
