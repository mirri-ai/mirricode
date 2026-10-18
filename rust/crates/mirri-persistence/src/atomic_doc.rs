//! Crash-safe atomic document storage for `state.json`-style files.
//!
//! Uses the write-to-temp + atomic rename pattern so that a crash at any
//! point leaves the destination file either fully in its previous state or
//! fully in its new state — never partially written.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs;

/// Crash-safe atomic document storage using write-to-temp + rename pattern.
///
/// Write flow:
/// 1. Serialize to pretty JSON.
/// 2. Write to `<path>.tmp`.
/// 3. Sync the temp file to disk for durability.
/// 4. Atomic rename to the final path (same filesystem).
/// 5. Best-effort fsync of the parent directory so the rename itself is
///    durable (a failure here is logged, not fatal).
///
/// Read flow:
/// 1. Read the file.
/// 2. Deserialize.
/// 3. Return `None` if the file does not exist or is empty.
pub struct AtomicDocument;

impl AtomicDocument {
    /// Atomically write `value` as pretty JSON to `path`.
    ///
    /// The write is durable: the temp file is synced to disk before the
    /// rename, so once this returns successfully the data is on stable
    /// storage (subject to the OS rename atomicity guarantees on the same
    /// filesystem).
    pub async fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).await?;
        }

        let json = serde_json::to_string_pretty(value)?;
        crate::fs_utils::write_file_atomically(path, json.as_bytes())?;

        Ok(())
    }

    /// Read and deserialize a document from `path`.
    ///
    /// Returns `Ok(None)` when the file does not exist or is empty, so
    /// callers can treat "first run" uniformly without special error handling.
    pub async fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match fs::read(path).await {
            Ok(bytes) => {
                if bytes.is_empty() {
                    return Ok(None);
                }
                let value = serde_json::from_slice(&bytes)?;
                Ok(Some(value))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The blocking twin of [`AtomicDocument::read`] for callers already
    /// on the blocking pool — identical semantics, no await point.
    pub fn read_blocking<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    return Ok(None);
                }
                let value = serde_json::from_slice(&bytes)?;
                Ok(Some(value))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
