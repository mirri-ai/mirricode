//! Synchronous filesystem abstraction for the state plane (zero-IO T13b).
//!
//! The async [`FileSystem`](crate::fs::FileSystem) trait serves the tool
//! face; the state plane (profile/config/skill/AGENTS.md loading and the
//! persistence hosts) is synchronous code, so this module provides
//! [`SyncFs`] — the same operation surface behind plain (non-`async`) methods.
//!
//! Synchronous methods are naturally dyn-compatible, so — unlike the async
//! traits, which need erased `Dyn*` mirrors — [`SyncFs`] is directly
//! object-safe and is injected as `&dyn SyncFs` / `Arc<dyn SyncFs>`.
//! [`LocalSyncFs`] is the host-filesystem implementation used as the engine
//! default (J3).

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::fs::{DirEntry, FileStat};

/// Synchronous filesystem abstraction for state-plane hosts.
///
/// Every method has a default body returning an error (or `false` for
/// [`SyncFs::exists`]), so test doubles only override the operations they
/// exercise; [`LocalSyncFs`] overrides all of them.
///
/// Semantics notes:
/// - [`SyncFs::write_text`] is a plain write: parent directories are **not**
///   created and the write is not atomic — call sites that need either call
///   [`SyncFs::mkdir`] first or use an atomic-write helper, matching the
///   pre-trait behavior of the state-plane hosts.
/// - [`SyncFs::stat`] stats without following symlinks (lstat semantics, like
///   the async [`FileSystem::stat`](crate::fs::FileSystem::stat)), so
///   `is_symlink` is meaningful.
/// - [`SyncFs::iterdir`] reports `is_dir`/`is_file` **following** symlinks
///   (like `Path::is_dir`/`Path::is_file`), matching the directory-scan
///   behavior of the migrated hosts.
pub trait SyncFs: Send + Sync {
    /// Read the entire file as a UTF-8 string.
    fn read_text(&self, _path: &Path) -> anyhow::Result<String> {
        Err(unimplemented_sync_fs("read_text"))
    }

    /// Read the entire file as raw bytes.
    fn read_bytes(&self, _path: &Path) -> anyhow::Result<Vec<u8>> {
        Err(unimplemented_sync_fs("read_bytes"))
    }

    /// Write `content` to `path` (plain write; see the trait docs).
    fn write_text(&self, _path: &Path, _content: &str) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("write_text"))
    }

    /// Write raw `bytes` to `path` (plain write; see the trait docs).
    fn write_bytes(&self, _path: &Path, _bytes: &[u8]) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("write_bytes"))
    }

    /// Stat `path` without following symbolic links.
    fn stat(&self, _path: &Path) -> anyhow::Result<FileStat> {
        Err(unimplemented_sync_fs("stat"))
    }

    /// Whether `path` exists (following symlinks, like `Path::exists`).
    fn exists(&self, _path: &Path) -> bool {
        false
    }

    /// List the immediate children of `path`.
    fn iterdir(&self, _path: &Path) -> anyhow::Result<Vec<DirEntry>> {
        Err(unimplemented_sync_fs("iterdir"))
    }

    /// Create a directory. When `recursive` is true, intermediate directories
    /// are created (like `mkdir -p`).
    fn mkdir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("mkdir"))
    }

    /// Remove a single file.
    fn remove_file(&self, _path: &Path) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("remove_file"))
    }

    /// Remove a directory. When `recursive` is true the directory and all its
    /// contents are removed (like `rm -r`).
    fn remove_dir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("remove_dir"))
    }

    /// Rename or move `from` to `to`.
    fn rename(&self, _from: &Path, _to: &Path) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("rename"))
    }

    /// Read the target of the symbolic link at `path`.
    fn read_link(&self, _path: &Path) -> anyhow::Result<PathBuf> {
        Err(unimplemented_sync_fs("read_link"))
    }

    /// Append `contents` to the end of `path`, creating the file if it does
    /// not exist, and **fsync before returning**.
    ///
    /// This carries the task-output-log append semantics: an acknowledged
    /// append must survive a crash, so the implementation syncs the file
    /// (not just flushes to the kernel buffer). Parent directories are not
    /// created — call [`SyncFs::mkdir_private`] / [`SyncFs::mkdir`] first.
    fn append(&self, _path: &Path, _contents: &str) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("append"))
    }

    /// Read up to `max_bytes` bytes from `path` starting at byte `offset`.
    ///
    /// Equivalent to `File::open` + `seek(Start(offset))` + a single `read`
    /// into a `max_bytes` buffer: fewer bytes than requested are returned
    /// when the file is shorter than `offset + max_bytes`. Returns the raw
    /// bytes (not necessarily valid UTF-8).
    fn read_range(&self, _path: &Path, _offset: u64, _max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        Err(unimplemented_sync_fs("read_range"))
    }

    /// Create a directory (and its parents) with mode `0o700` on Unix.
    ///
    /// Directories holding raw command output are kept private, so this is
    /// the private-directory primitive (recursive `DirBuilder` with mode
    /// `0o700`; a plain recursive create on non-Unix platforms).
    fn mkdir_private(&self, _path: &Path) -> anyhow::Result<()> {
        Err(unimplemented_sync_fs("mkdir_private"))
    }
}

/// The error returned by [`SyncFs`]'s default method bodies, marking an
/// operation a double chose not to implement.
fn unimplemented_sync_fs(op: &str) -> anyhow::Error {
    anyhow::anyhow!("operation '{op}' is not implemented by this SyncFs")
}

/// [`SyncFs`] implementation backed by the host local filesystem via
/// `std::fs`.
#[derive(Debug, Default)]
pub struct LocalSyncFs;

impl LocalSyncFs {
    /// Create a new `LocalSyncFs`.
    pub fn new() -> Self {
        Self
    }
}

impl SyncFs for LocalSyncFs {
    fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read text from {}", path.display()))
    }

    fn read_bytes(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        std::fs::read(path).with_context(|| format!("failed to read bytes from {}", path.display()))
    }

    fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        std::fs::write(path, content)
            .with_context(|| format!("failed to write text to {}", path.display()))
    }

    fn write_bytes(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        std::fs::write(path, bytes)
            .with_context(|| format!("failed to write bytes to {}", path.display()))
    }

    fn stat(&self, path: &Path) -> anyhow::Result<FileStat> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?;

        let is_symlink = metadata.file_type().is_symlink();
        let is_dir = metadata.is_dir();
        let is_file = metadata.is_file();

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let atime = metadata
            .accessed()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        #[cfg(not(unix))]
        let mode = 0o644;

        Ok(FileStat {
            size: metadata.len(),
            is_dir,
            is_file,
            is_symlink,
            mtime,
            atime,
            mode,
        })
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        let dir = std::fs::read_dir(path)
            .with_context(|| format!("failed to read dir: {}", path.display()))?;

        for entry in dir {
            let entry =
                entry.with_context(|| format!("failed to read entry in: {}", path.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let entry_path = entry.path();
            let path_str = entry_path.to_string_lossy().into_owned();

            // Follow symlinks for the type flags (like `Path::is_dir` /
            // `Path::is_file`); an unstat-able entry (e.g. a broken symlink)
            // reports neither, matching `Path::is_*`'s error-to-false.
            let (is_dir, is_file) = match std::fs::metadata(&entry_path) {
                Ok(metadata) => (metadata.is_dir(), metadata.is_file()),
                Err(_) => (false, false),
            };

            entries.push(DirEntry {
                name,
                path: path_str,
                is_dir,
                is_file,
            });
        }

        Ok(entries)
    }

    fn mkdir(&self, path: &Path, recursive: bool) -> anyhow::Result<()> {
        if recursive {
            std::fs::create_dir_all(path).with_context(|| {
                format!("failed to create dirs (recursive): {}", path.display())
            })?;
        } else {
            std::fs::create_dir(path)
                .with_context(|| format!("failed to create dir: {}", path.display()))?;
        }
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove file: {}", path.display()))?;
        Ok(())
    }

    fn remove_dir(&self, path: &Path, recursive: bool) -> anyhow::Result<()> {
        if recursive {
            std::fs::remove_dir_all(path)
                .with_context(|| format!("failed to remove dir (recursive): {}", path.display()))?;
        } else {
            std::fs::remove_dir(path)
                .with_context(|| format!("failed to remove dir: {}", path.display()))?;
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        std::fs::rename(from, to)
            .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
        Ok(())
    }

    fn read_link(&self, path: &Path) -> anyhow::Result<PathBuf> {
        std::fs::read_link(path).with_context(|| format!("failed to read link {}", path.display()))
    }

    fn append(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open file for appending: {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to append to {}", path.display()))?;
        // fsync before reporting success: an acknowledged append survives a
        // crash (task output-log semantics).
        file.sync_all()
            .with_context(|| format!("failed to sync append to {}", path.display()))?;
        Ok(())
    }

    fn read_range(&self, path: &Path, offset: u64, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("failed to open file for range read: {}", path.display()))?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .with_context(|| format!("failed to seek in {}", path.display()))?;
        }
        // A single `read` (not `read_exact`): a short file yields fewer than
        // `max_bytes` bytes rather than an error, matching the pre-trait
        // offset-read behavior.
        let mut buf = vec![0u8; max_bytes];
        let n = file
            .read(&mut buf)
            .with_context(|| format!("failed to read range from {}", path.display()))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn mkdir_private(&self, path: &Path) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .with_context(|| format!("failed to create private dir: {}", path.display()))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(path)
                .with_context(|| format!("failed to create private dir: {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// should return false by default when a SyncFs double does not
    /// implement exists
    #[test]
    fn should_return_false_by_default_when_double_does_not_implement_exists() {
        struct NoopFs;
        impl SyncFs for NoopFs {}

        assert!(!NoopFs.exists(Path::new("/anywhere")));
        assert!(NoopFs.read_text(Path::new("/anywhere")).is_err());
    }

    /// should roundtrip text through write_text and read_text on the local fs
    #[test]
    fn should_roundtrip_text_through_write_and_read_on_local_fs() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("a/b.txt");
        let fs = LocalSyncFs;

        // Plain-write semantics: the parent must be created explicitly.
        fs.mkdir(path.parent().expect("parent"), true)
            .expect("mkdir");
        fs.write_text(&path, "hello").expect("write");
        assert_eq!(fs.read_text(&path).expect("read"), "hello");
        assert!(fs.exists(&path));

        let stat = fs.stat(&path).expect("stat");
        assert!(stat.is_file);
        assert_eq!(stat.size, 5);
        assert!(!stat.is_symlink);
    }

    /// should list entries with followed type flags when iterating a local dir
    #[test]
    fn should_list_entries_with_followed_type_flags_when_iterating_local_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let fs = LocalSyncFs;
        fs.mkdir(&tmp.path().join("sub"), true).expect("mkdir");
        fs.write_text(&tmp.path().join("f.txt"), "x")
            .expect("write");

        let mut entries = fs.iterdir(tmp.path()).expect("iterdir");
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_file);
        assert_eq!(entries[0].name, "f.txt");
        assert!(entries[1].is_dir);
        assert_eq!(entries[1].name, "sub");
    }

    /// should append with fsync semantics and read back an offset range
    #[test]
    fn should_append_with_fsync_semantics_and_read_back_an_offset_range() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("output.log");
        let fs = LocalSyncFs;

        fs.append(&path, "line one\n").expect("append");
        fs.append(&path, "line two\n").expect("append");

        assert_eq!(
            fs.read_range(&path, 0, 100).expect("read"),
            b"line one\nline two\n"
        );
        // Offset reads: the range starts exactly at the byte offset.
        assert_eq!(fs.read_range(&path, 9, 4).expect("read"), b"line");
        // A short file yields fewer bytes than requested, not an error.
        assert_eq!(fs.read_range(&path, 14, 100).expect("read"), b"two\n");
        // An offset at (or past) end of file yields nothing.
        assert!(fs.read_range(&path, 18, 10).expect("read").is_empty());
    }

    /// should create a private directory with mode 0o700 on unix
    #[test]
    fn should_create_a_private_directory_with_mode_0o700_on_unix() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("a/b/private");
        let fs = LocalSyncFs;

        fs.mkdir_private(&path).expect("mkdir_private");
        assert!(fs.stat(&path).expect("stat").is_dir);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "private dirs must be mode 0o700");
        }
    }
}
