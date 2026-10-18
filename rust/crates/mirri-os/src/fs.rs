//! Filesystem abstractions for the mirri execution environment.
//!
//! This module provides a [`FileSystem`] trait that abstracts common file
//! operations behind an async interface, plus [`LocalFs`] — a concrete
//! implementation backed by `tokio::fs` for the host filesystem.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Context;
use tokio::io::SeekFrom;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};

/// A boxed, `Send` future as returned by the object-safe [`DynFileSystem`]
/// trait's methods.
///
/// The [`FileSystem`] trait itself uses RPITIT (`async fn` in trait), which
/// is not dyn-compatible; [`DynFileSystem`] erases the same surface behind
/// boxed futures so providers can be injected as `Arc<dyn DynFileSystem>`.
pub type DynFsFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Metadata describing a single filesystem entry.
///
/// Mirrors the TypeScript `KaosStat` interface: all time fields are Unix
/// epoch seconds, and `mode` is the POSIX permission bits on Unix.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileStat {
    /// File size in bytes.
    pub size: u64,
    /// `true` if the entry is a directory.
    #[serde(rename = "isDir")]
    pub is_dir: bool,
    /// `true` if the entry is a regular file.
    #[serde(rename = "isFile")]
    pub is_file: bool,
    /// `true` if the entry is a symbolic link (the link itself, not the target).
    #[serde(rename = "isSymlink")]
    pub is_symlink: bool,
    /// Last modification time as Unix epoch seconds.
    pub mtime: u64,
    /// Last access time as Unix epoch seconds.
    pub atime: u64,
    /// POSIX permission bits (Unix) or a best-effort value (non-Unix).
    pub mode: u32,
}

/// A single entry returned by directory iteration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    /// Entry basename (e.g. `foo.txt`).
    pub name: String,
    /// Entry path relative to the iterated directory (e.g. `dir/foo.txt`).
    pub path: String,
    /// `true` if the entry is a directory.
    #[serde(rename = "isDir")]
    pub is_dir: bool,
    /// `true` if the entry is a regular file.
    #[serde(rename = "isFile")]
    pub is_file: bool,
}

/// Result of scanning a text file for structural metadata.
///
/// Produced by [`LocalFs::scan_text_file`]; mirrors the return type of the
/// TypeScript `scanTextFile` helper.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileScanResult {
    /// Total number of lines in the file.
    #[serde(rename = "totalLines")]
    pub total_lines: u64,
    /// `true` when the file ends with a trailing newline.
    #[serde(rename = "endsWithNewline")]
    pub ends_with_newline: bool,
    /// `true` when the file contains at least one NUL byte.
    #[serde(rename = "hasNul")]
    pub has_nul: bool,
    /// Detected line-ending style: `"lf"`, `"crlf"`, or `"mixed"`.
    #[serde(rename = "lineEndingFlags")]
    pub line_ending_flags: String,
}

/// Async filesystem abstraction used by the agent engine.
///
/// All methods are async via RPITIT (return-position impl trait in trait),
/// so no `async-trait` macro is required. Implementors must be `Send + Sync`.
///
/// The `async_fn_in_trait` clippy lint is allowed because this trait is
/// intended for internal use within the runtime where the implementors
/// (`LocalFs`, and future test doubles) produce `Send` futures via `tokio`.
#[allow(async_fn_in_trait)]
pub trait FileSystem: Send + Sync {
    /// Read the entire file as a UTF-8 string.
    async fn read_text(&self, path: &Path) -> anyhow::Result<String>;

    /// Read the entire file as raw bytes.
    async fn read_bytes(&self, path: &Path) -> anyhow::Result<Vec<u8>>;

    /// Read up to `max_bytes` bytes starting at byte `offset` in the file.
    ///
    /// Returns fewer bytes than `max_bytes` only when end-of-file is
    /// reached; an `offset` beyond end-of-file returns an empty vector. Lets
    /// callers page through large files with bounded memory instead of
    /// loading them whole via [`FileSystem::read_bytes`].
    async fn read_bytes_range(
        &self,
        path: &Path,
        offset: u64,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>>;

    /// Write `content` to `path`, creating parent directories as needed.
    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()>;

    /// Write `content` to `path`, creating parent directories as needed.
    async fn write_bytes(&self, path: &Path, content: &[u8]) -> anyhow::Result<()>;

    /// Create a directory. When `recursive` is true, intermediate directories
    /// are created (like `mkdir -p`).
    async fn mkdir(&self, path: &Path, recursive: bool) -> anyhow::Result<()>;

    /// Stat `path` without following symbolic links.
    async fn stat(&self, path: &Path) -> anyhow::Result<FileStat>;

    /// List the immediate children of `path`.
    async fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<DirEntry>>;

    /// Remove a single file.
    async fn remove_file(&self, path: &Path) -> anyhow::Result<()>;

    /// Remove a directory. When `recursive` is true the directory and all its
    /// contents are removed (like `rm -r`).
    async fn remove_dir(&self, path: &Path, recursive: bool) -> anyhow::Result<()>;

    /// Rename or move `from` to `to`.
    async fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()>;

    /// Copy `from` to `to`, overwriting `to` if it exists.
    async fn copy(&self, from: &Path, to: &Path) -> anyhow::Result<()>;

    /// Append `contents` to the end of `path`, creating the file if it does
    /// not exist, and flush before returning.
    ///
    /// Parent directories are *not* created — callers that need them (e.g.
    /// the Write tool's append mode) call [`FileSystem::mkdir`] first.
    async fn append(&self, path: &Path, contents: &str) -> anyhow::Result<()>;

    /// Read the target of the symbolic link at `path`.
    async fn read_link(&self, path: &Path) -> anyhow::Result<PathBuf>;

    /// Whether `path` exists (following symlinks, like `Path::exists`).
    async fn exists(&self, path: &Path) -> anyhow::Result<bool>;
}

/// Object-safe (dyn-compatible) mirror of [`FileSystem`].
///
/// The RPITIT trait cannot be used as `dyn FileSystem` (async fn in trait is
/// not dyn-compatible), so provider injection (T13a) goes through this
/// erased twin: every method returns a boxed `Send` future instead. The
/// local implementation ([`LocalFs`]) implements both; test doubles
/// implement only this trait.
///
/// Every method has a default body returning an error, so fakes only
/// override the operations they exercise. [`LocalFs`] overrides all of them.
pub trait DynFileSystem: Send + Sync {
    /// Read the entire file as a UTF-8 string.
    fn read_text<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<String>> {
        Box::pin(async { Err(unimplemented_fs("read_text")) })
    }

    /// Read the entire file as raw bytes.
    fn read_bytes<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async { Err(unimplemented_fs("read_bytes")) })
    }

    /// Read up to `max_bytes` bytes starting at byte `offset` in the file.
    /// See [`FileSystem::read_bytes_range`].
    fn read_bytes_range<'a>(
        &'a self,
        _path: &'a Path,
        _offset: u64,
        _max_bytes: usize,
    ) -> DynFsFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async { Err(unimplemented_fs("read_bytes_range")) })
    }

    /// Write `content` to `path`, creating parent directories as needed.
    fn write_text<'a>(
        &'a self,
        _path: &'a Path,
        _content: &'a str,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("write_text")) })
    }

    /// Write `content` to `path`, creating parent directories as needed.
    fn write_bytes<'a>(
        &'a self,
        _path: &'a Path,
        _content: &'a [u8],
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("write_bytes")) })
    }

    /// Create a directory. When `recursive` is true, intermediate directories
    /// are created (like `mkdir -p`).
    fn mkdir<'a>(
        &'a self,
        _path: &'a Path,
        _recursive: bool,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("mkdir")) })
    }

    /// Stat `path` without following symbolic links.
    fn stat<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<FileStat>> {
        Box::pin(async { Err(unimplemented_fs("stat")) })
    }

    /// List the immediate children of `path`.
    fn iterdir<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<Vec<DirEntry>>> {
        Box::pin(async { Err(unimplemented_fs("iterdir")) })
    }

    /// Remove a single file.
    fn remove_file<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("remove_file")) })
    }

    /// Remove a directory. When `recursive` is true the directory and all its
    /// contents are removed (like `rm -r`).
    fn remove_dir<'a>(
        &'a self,
        _path: &'a Path,
        _recursive: bool,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("remove_dir")) })
    }

    /// Rename or move `from` to `to`.
    fn rename<'a>(&'a self, _from: &'a Path, _to: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("rename")) })
    }

    /// Copy `from` to `to`, overwriting `to` if it exists.
    fn copy<'a>(&'a self, _from: &'a Path, _to: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("copy")) })
    }

    /// Append `contents` to the end of `path`, creating the file if it does
    /// not exist, and flush before returning.
    fn append<'a>(
        &'a self,
        _path: &'a Path,
        _contents: &'a str,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Err(unimplemented_fs("append")) })
    }

    /// Read the target of the symbolic link at `path`.
    fn read_link<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<PathBuf>> {
        Box::pin(async { Err(unimplemented_fs("read_link")) })
    }

    /// Whether `path` exists (following symlinks, like `Path::exists`).
    fn exists<'a>(&'a self, _path: &'a Path) -> DynFsFuture<'a, anyhow::Result<bool>> {
        Box::pin(async { Ok(false) })
    }
}

/// The error returned by [`DynFileSystem`]'s default method bodies, marking
/// an operation a double chose not to implement.
fn unimplemented_fs(op: &str) -> anyhow::Error {
    anyhow::anyhow!("operation '{op}' is not implemented by this DynFileSystem")
}

/// [`DynFileSystem`] implementation delegating to [`LocalFs`]'s [`FileSystem`]
/// methods (the RPITIT futures are `Send`, so they box directly).
impl DynFileSystem for LocalFs {
    fn read_text<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<String>> {
        Box::pin(<Self as FileSystem>::read_text(self, path))
    }

    fn read_bytes<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(<Self as FileSystem>::read_bytes(self, path))
    }

    fn read_bytes_range<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
        max_bytes: usize,
    ) -> DynFsFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(<Self as FileSystem>::read_bytes_range(
            self, path, offset, max_bytes,
        ))
    }

    fn write_text<'a>(
        &'a self,
        path: &'a Path,
        content: &'a str,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::write_text(self, path, content))
    }

    fn write_bytes<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::write_bytes(self, path, content))
    }

    fn mkdir<'a>(&'a self, path: &'a Path, recursive: bool) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::mkdir(self, path, recursive))
    }

    fn stat<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<FileStat>> {
        Box::pin(<Self as FileSystem>::stat(self, path))
    }

    fn iterdir<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<Vec<DirEntry>>> {
        Box::pin(<Self as FileSystem>::iterdir(self, path))
    }

    fn remove_file<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::remove_file(self, path))
    }

    fn remove_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::remove_dir(self, path, recursive))
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::rename(self, from, to))
    }

    fn copy<'a>(&'a self, from: &'a Path, to: &'a Path) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::copy(self, from, to))
    }

    fn append<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
    ) -> DynFsFuture<'a, anyhow::Result<()>> {
        Box::pin(<Self as FileSystem>::append(self, path, contents))
    }

    fn read_link<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<PathBuf>> {
        Box::pin(<Self as FileSystem>::read_link(self, path))
    }

    fn exists<'a>(&'a self, path: &'a Path) -> DynFsFuture<'a, anyhow::Result<bool>> {
        Box::pin(<Self as FileSystem>::exists(self, path))
    }
}

/// [`FileSystem`] implementation backed by the host local filesystem via
/// `tokio::fs`.
#[derive(Debug, Default)]
pub struct LocalFs;

impl LocalFs {
    /// Create a new `LocalFs`.
    pub fn new() -> Self {
        Self
    }

    /// Ensure the parent directory of `path` exists, creating it recursively.
    async fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create parent dirs for {}", path.display()))?;
        }
        Ok(())
    }

    /// Scan `path` for line-count, NUL-byte, and line-ending metadata.
    ///
    /// The entire file is read into memory; suitable for text files of moderate
    /// size.
    pub async fn scan_text_file(&self, path: &Path) -> anyhow::Result<FileScanResult> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read file for scanning: {}", path.display()))?;

        let has_nul = bytes.contains(&0u8);

        // Count newlines and detect line-ending style.
        let mut newline_count: u64 = 0;
        let mut crlf_count: u64 = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            if byte == b'\n' {
                newline_count += 1;
                if i > 0 && bytes[i - 1] == b'\r' {
                    crlf_count += 1;
                }
            }
        }

        let ends_with_newline = !bytes.is_empty() && bytes.last() == Some(&b'\n');

        // Total lines: trailing-newline files have one line per newline;
        // files without a trailing newline get one extra partial line.
        let total_lines = if bytes.is_empty() {
            0
        } else if ends_with_newline {
            newline_count
        } else {
            newline_count + 1
        };

        let line_ending_flags = if newline_count == 0 {
            // No newlines at all — report "lf" as the neutral default.
            "lf".to_string()
        } else if crlf_count == newline_count {
            "crlf".to_string()
        } else if crlf_count == 0 {
            "lf".to_string()
        } else {
            "mixed".to_string()
        };

        Ok(FileScanResult {
            total_lines,
            ends_with_newline,
            has_nul,
            line_ending_flags,
        })
    }

    /// Read a range of lines from `path`, starting at `start_line` (0-based)
    /// and returning at most `max_lines` lines.
    pub async fn read_line_range(
        &self,
        path: &Path,
        start_line: u64,
        max_lines: u64,
    ) -> anyhow::Result<Vec<String>> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("failed to open file for line range: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();

        let mut result = Vec::new();
        let mut current_line: u64 = 0;

        while let Some(line) = lines
            .next_line()
            .await
            .with_context(|| format!("failed to read line from: {}", path.display()))?
        {
            if current_line >= start_line {
                result.push(line);
                if result.len() as u64 >= max_lines {
                    break;
                }
            }
            current_line += 1;
        }

        Ok(result)
    }

    /// Read the last `tail_count` lines from `path` using a reverse
    /// block-by-block scan. Lines are returned in original file order.
    pub async fn read_tail_lines(
        &self,
        path: &Path,
        tail_count: u64,
    ) -> anyhow::Result<Vec<String>> {
        if tail_count == 0 {
            return Ok(Vec::new());
        }

        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("failed to open file for tail read: {}", path.display()))?;

        let file_size = file
            .metadata()
            .await
            .with_context(|| format!("failed to stat file for tail read: {}", path.display()))?
            .len();

        if file_size == 0 {
            return Ok(Vec::new());
        }

        const BLOCK_SIZE: u64 = 8 * 1024;

        // Lines collected in reverse order, then reversed at the end.
        let mut collected: Vec<String> = Vec::new();
        let mut pos = file_size;
        // Leftover bytes from the previous (earlier-in-file) block that belong
        // to the line currently being assembled.
        let mut leftover: Vec<u8> = Vec::new();

        while pos > 0 && (collected.len() as u64) < tail_count {
            let read_size = BLOCK_SIZE.min(pos);
            pos -= read_size;

            file.seek(SeekFrom::Start(pos))
                .await
                .with_context(|| format!("failed to seek in: {}", path.display()))?;

            let mut buf = vec![0u8; read_size as usize];
            file.read_exact(&mut buf)
                .await
                .with_context(|| format!("failed to read block from: {}", path.display()))?;

            // Prepend leftover from the next (later-in-file) block so we can
            // split this combined buffer on newlines.
            let mut chunk = buf;
            chunk.extend_from_slice(&leftover);
            leftover.clear();

            // Walk backwards through the chunk, splitting on '\n'.
            // Bytes after the last '\n' (i.e. at the end) form a partial line
            // that belongs to the next block — keep them as leftover.
            let mut segment_end = chunk.len();
            for i in (0..chunk.len()).rev() {
                if chunk[i] == b'\n' {
                    // Bytes [i+1, segment_end) form one complete line.
                    let line_bytes = &chunk[i + 1..segment_end];

                    if !line_bytes.is_empty() || i + 1 < segment_end {
                        // Strip a trailing '\r' if this is a CRLF file.
                        let line_bytes = if line_bytes.last() == Some(&b'\r') {
                            &line_bytes[..line_bytes.len() - 1]
                        } else {
                            line_bytes
                        };
                        let line = String::from_utf8_lossy(line_bytes).into_owned();
                        collected.push(line);
                    }

                    segment_end = i;

                    if (collected.len() as u64) >= tail_count {
                        break;
                    }
                }
            }

            // Whatever precedes the first newline (from the start of chunk up
            // to segment_end) belongs to the line continued in an earlier block.
            if segment_end > 0 {
                leftover = chunk[..segment_end].to_vec();
            }
        }

        // If we reached the beginning of the file with leftover bytes, that
        // leftover is the first line of the file (no preceding newline).
        if (collected.len() as u64) < tail_count && !leftover.is_empty() {
            let line_bytes = if leftover.last() == Some(&b'\r') {
                &leftover[..leftover.len() - 1]
            } else {
                &leftover[..]
            };
            let line = String::from_utf8_lossy(line_bytes).into_owned();
            collected.push(line);
        }

        collected.reverse();
        Ok(collected)
    }
}

impl FileSystem for LocalFs {
    async fn read_text(&self, path: &Path) -> anyhow::Result<String> {
        tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read text from {}", path.display()))
    }

    async fn read_bytes(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read bytes from {}", path.display()))
    }

    async fn read_bytes_range(
        &self,
        path: &Path,
        offset: u64,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("failed to open {}", path.display()))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .with_context(|| format!("failed to seek in {}", path.display()))?;
        let mut buf = vec![0u8; max_bytes];
        let mut read = 0usize;
        loop {
            if read == max_bytes {
                break;
            }
            match file.read(&mut buf[read..]).await {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("failed to read bytes from {}", path.display())));
                }
            }
        }
        buf.truncate(read);
        Ok(buf)
    }

    async fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        Self::ensure_parent_dir(path).await?;
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("failed to write text to {}", path.display()))?;
        Ok(())
    }

    async fn write_bytes(&self, path: &Path, content: &[u8]) -> anyhow::Result<()> {
        Self::ensure_parent_dir(path).await?;
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("failed to write bytes to {}", path.display()))?;
        Ok(())
    }

    async fn mkdir(&self, path: &Path, recursive: bool) -> anyhow::Result<()> {
        if recursive {
            tokio::fs::create_dir_all(path).await.with_context(|| {
                format!("failed to create dirs (recursive): {}", path.display())
            })?;
        } else {
            tokio::fs::create_dir(path)
                .await
                .with_context(|| format!("failed to create dir: {}", path.display()))?;
        }
        Ok(())
    }

    async fn stat(&self, path: &Path) -> anyhow::Result<FileStat> {
        let metadata = tokio::fs::symlink_metadata(path)
            .await
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

    async fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path)
            .await
            .with_context(|| format!("failed to read dir: {}", path.display()))?;

        while let Some(entry) = dir
            .next_entry()
            .await
            .with_context(|| format!("failed to read entry in: {}", path.display()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let entry_path = entry.path();
            let path_str = entry_path.to_string_lossy().into_owned();

            let file_type = entry.file_type().await.with_context(|| {
                format!("failed to get file type for: {}", entry_path.display())
            })?;

            entries.push(DirEntry {
                name,
                path: path_str,
                is_dir: file_type.is_dir(),
                is_file: file_type.is_file(),
            });
        }

        Ok(entries)
    }

    async fn remove_file(&self, path: &Path) -> anyhow::Result<()> {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("failed to remove file: {}", path.display()))?;
        Ok(())
    }

    async fn remove_dir(&self, path: &Path, recursive: bool) -> anyhow::Result<()> {
        if recursive {
            tokio::fs::remove_dir_all(path)
                .await
                .with_context(|| format!("failed to remove dir (recursive): {}", path.display()))?;
        } else {
            tokio::fs::remove_dir(path)
                .await
                .with_context(|| format!("failed to remove dir: {}", path.display()))?;
        }
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        tokio::fs::rename(from, to)
            .await
            .with_context(|| format!("failed to rename {} to {}", from.display(), to.display()))?;
        Ok(())
    }

    async fn copy(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        Self::ensure_parent_dir(to).await?;
        tokio::fs::copy(from, to)
            .await
            .with_context(|| format!("failed to copy {} to {}", from.display(), to.display()))?;
        Ok(())
    }

    async fn append(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
        // Open in append mode, creating the file if it does not exist. Parent
        // directories are deliberately NOT created here — callers that need
        // them call `mkdir` first (the Write tool's append mode).
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .await
            .with_context(|| format!("failed to open file for appending: {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .await
            .with_context(|| format!("failed to append to {}", path.display()))?;
        // Flush before reporting success: write_all puts the data in the
        // kernel buffer, but a drop-triggered background flush is not awaited
        // and a crash before it completes could lose data.
        file.flush()
            .await
            .with_context(|| format!("failed to flush append to {}", path.display()))?;
        Ok(())
    }

    async fn read_link(&self, path: &Path) -> anyhow::Result<PathBuf> {
        tokio::fs::read_link(path)
            .await
            .with_context(|| format!("failed to read link {}", path.display()))
    }

    async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
        // `Path::exists` semantics: any stat error (including a broken
        // symlink) counts as "not existing". `tokio::fs::metadata` follows
        // symlinks, matching the pre-trait `path.exists()` call sites.
        Ok(tokio::fs::metadata(path).await.is_ok())
    }
}
