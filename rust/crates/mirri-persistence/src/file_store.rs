//! The files store — the v1 TS `IFileService` equivalent.
//!
//! Backs the `POST /api/v1/files` upload route and the `message.send` entry
//! resolution. Storage layout mirrors the TS `FileServiceImpl` semantics:
//! file bytes live at `{root}/{file_id}` and a JSON metadata index lives at
//! `{root}/index.json` (`{version: 1, files: [FileMeta]}`, the TS `IndexFile`
//! shape). Uploads are uncapped (local single-user deployment, the TS
//! `Number.MAX_SAFE_INTEGER` behavior) and `expires_at` is recorded but never
//! enforced on read — the same advisory semantics the TS store has.
//!
//! The store is stateless over its root directory (the index is re-read on
//! every operation), so independent instances over the same root — the REST
//! routes and the engine's entry resolution — observe each other's writes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The `f_`-prefixed file id vocabulary (the TS `FILE_ID_REGEX`).
const FILE_ID_PREFIX: &str = "f_";

/// Metadata of one stored file (the TS `FileMeta` wire shape — snake_case).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Options of a [`FileStore::save`] (the TS `SaveOptions`).
#[derive(Debug, Clone, Default)]
pub struct SaveOptions {
    /// Display name override; the upload's filename when absent.
    pub name: Option<String>,
    /// MIME type; `application/octet-stream` when absent.
    pub mime_type: Option<String>,
    /// Optional advisory expiry, in seconds from now.
    pub expires_in_sec: Option<u64>,
}

/// Whether `file_id` is a well-formed files-store reference (the TS
/// `FILE_ID_REGEX` `^f_[A-Za-z0-9][A-Za-z0-9_-]*$`).
pub fn is_file_id(file_id: &str) -> bool {
    let Some(rest) = file_id.strip_prefix(FILE_ID_PREFIX) else {
        return false;
    };
    let mut chars = rest.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric()) {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The on-disk metadata index (the TS `IndexFile`).
#[derive(Debug, Deserialize, Serialize)]
struct IndexFile {
    version: u32,
    files: Vec<FileMeta>,
}

impl Default for IndexFile {
    fn default() -> Self {
        Self {
            version: 1,
            files: Vec::new(),
        }
    }
}

/// A files store rooted at a directory.
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    /// Create a store rooted at `root` (the directory is created lazily on
    /// the first save).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store `data` under a fresh file id and record its metadata.
    ///
    /// Mirrors the TS `FileServiceImpl::save`: the id is `f_{uuid}`, the
    /// name falls back to the upload filename, the media type defaults to
    /// `application/octet-stream`, and `expires_at` is recorded only when
    /// `expires_in_sec` was provided. Best-effort cleanup on failure: a
    /// partially written blob is removed before the error propagates.
    pub fn save(
        &self,
        data: &[u8],
        filename: &str,
        options: SaveOptions,
    ) -> std::io::Result<FileMeta> {
        let mut sink = self.begin()?;
        sink.write(data)?;
        sink.finish(filename, options)
    }

    /// Start a streaming upload: the blob file is created eagerly and the
    /// returned sink accepts the payload incrementally (the upload route
    /// streams multipart bodies straight to disk instead of buffering them
    /// in memory). Layout is identical to [`FileStore::save`] — only the
    /// write cadence differs.
    pub fn begin(&self) -> std::io::Result<FileSink> {
        std::fs::create_dir_all(&self.root)?;
        let id = format!("{FILE_ID_PREFIX}{}", uuid::Uuid::new_v4());
        let file = std::fs::File::create(self.root.join(&id))?;
        Ok(FileSink {
            root: self.root.clone(),
            id,
            file,
            size: 0,
            finished: false,
        })
    }

    /// Delete a file by id: the index entry is removed and the blob file
    /// deleted (a missing blob is tolerated — the entry is pruned either
    /// way, the same resilience `get` has). Returns whether the id was
    /// known; `false` means unknown or malformed (the caller answers
    /// not-found).
    pub fn delete(&self, file_id: &str) -> std::io::Result<bool> {
        if !is_file_id(file_id) {
            return Ok(false);
        }
        let mut index = load_index_at(&self.root);
        let Some(position) = index.files.iter().position(|meta| meta.id == file_id) else {
            return Ok(false);
        };
        index.files.remove(position);
        write_index_at(&self.root, &index)?;
        // Best-effort: the entry is gone, so a missing blob is not an error.
        let _ = std::fs::remove_file(self.root.join(file_id));
        Ok(true)
    }

    /// Look a file up by id: its metadata and bytes, or `None` when the id
    /// is malformed, unknown, or its blob is missing (the TS behavior of
    /// pruning the stale index entry on a missing blob is mirrored).
    pub fn get(&self, file_id: &str) -> Option<(FileMeta, Vec<u8>)> {
        if !is_file_id(file_id) {
            return None;
        }
        let mut index = load_index_at(&self.root);
        let position = index.files.iter().position(|meta| meta.id == file_id)?;
        let meta = index.files[position].clone();
        let data = match std::fs::read(self.root.join(file_id)) {
            Ok(data) => data,
            Err(_) => {
                // The blob vanished under the index — prune the stale entry
                // (best-effort) and answer not-found, exactly like the TS.
                index.files.remove(position);
                let _ = write_index_at(&self.root, &index);
                return None;
            }
        };
        Some((meta, data))
    }
}

/// An in-progress streaming upload over one blob file.
///
/// The blob lives at `{root}/{id}` from construction; [`FileSink::finish`]
/// records the metadata index entry, and an abandoned sink (any drop before
/// `finish`, including a `finish` that fails midway) removes the partial
/// blob so no orphan is left behind.
pub struct FileSink {
    root: PathBuf,
    id: String,
    file: std::fs::File,
    size: u64,
    finished: bool,
}

impl FileSink {
    /// Append one chunk of the payload to the blob.
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        self.file.write_all(data)?;
        self.size += data.len() as u64;
        Ok(())
    }

    /// The file id this sink will record (available before finishing).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Seal the blob and record its metadata — the same index append and
    /// defaults [`FileStore::save`] applies.
    pub fn finish(mut self, filename: &str, options: SaveOptions) -> std::io::Result<FileMeta> {
        use std::io::Write as _;
        self.file.flush()?;
        let now = chrono::Utc::now();
        let meta = FileMeta {
            id: self.id.clone(),
            name: options.name.unwrap_or_else(|| filename.to_string()),
            media_type: options
                .mime_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            size: self.size,
            created_at: now.to_rfc3339(),
            expires_at: options
                .expires_in_sec
                .map(|secs| (now + chrono::Duration::seconds(secs as i64)).to_rfc3339()),
        };
        let mut index = load_index_at(&self.root);
        index.files.push(meta.clone());
        write_index_at(&self.root, &index)?;
        // Only now is the upload durable; the Drop guard must not remove it.
        self.finished = true;
        Ok(meta)
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(self.root.join(&self.id));
        }
    }
}

/// Read the metadata index from a root directory.
fn load_index_at(root: &Path) -> IndexFile {
    std::fs::read_to_string(root.join("index.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the metadata index to a root directory.
fn write_index_at(root: &Path, index: &IndexFile) -> std::io::Result<()> {
    let raw = serde_json::to_string(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(root.join("index.json"), raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_file_meta_when_saving_bytes() {
        // Given an empty store
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());

        // When bytes are saved under a filename
        let meta = store
            .save(b"hello", "greeting.txt", SaveOptions::default())
            .unwrap();

        // Then the metadata carries the id vocabulary, name, size, and the
        // default media type — and no expiry
        assert!(
            meta.id.starts_with("f_"),
            "id must be f_-prefixed: {}",
            meta.id
        );
        assert_eq!(meta.name, "greeting.txt");
        assert_eq!(meta.media_type, "application/octet-stream");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.expires_at, None);
    }

    #[test]
    fn should_return_same_bytes_when_getting_saved_file() {
        // Given a store with one saved file
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let data = vec![0xAB; 1024];
        let meta = store
            .save(&data, "blob.bin", SaveOptions::default())
            .unwrap();

        // When the file is fetched back by id
        let (fetched_meta, fetched) = store.get(&meta.id).expect("file should exist");

        // Then the bytes are identical and the metadata round-trips
        assert_eq!(fetched, data);
        assert_eq!(fetched_meta, meta);
    }

    #[test]
    fn should_apply_overrides_when_saving_with_name_and_mime_and_expiry() {
        // Given a store receiving an upload with overrides
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());

        // When the save carries a name, a MIME type, and an expiry
        let meta = store
            .save(
                b"x",
                "upload.bin",
                SaveOptions {
                    name: Some("display.png".to_string()),
                    mime_type: Some("image/png".to_string()),
                    expires_in_sec: Some(3600),
                },
            )
            .unwrap();

        // Then all three land in the metadata
        assert_eq!(meta.name, "display.png");
        assert_eq!(meta.media_type, "image/png");
        let expires_at = meta.expires_at.expect("expiry should be recorded");
        assert!(!expires_at.is_empty(), "expiry must be an ISO timestamp");
    }

    #[test]
    fn should_return_none_when_getting_unknown_or_malformed_id() {
        // Given a store with one file
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        store.save(b"x", "a.txt", SaveOptions::default()).unwrap();

        // When an unknown id (or a malformed, non-f_-prefixed one) is fetched
        // Then the answer is not-found
        assert!(store.get("f_nope").is_none());
        assert!(store.get("../etc/passwd").is_none());
        assert!(store.get("").is_none());
    }

    #[test]
    fn should_prune_index_entry_when_blob_is_missing() {
        // Given a store whose recorded file lost its blob on disk
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let meta = store
            .save(b"data", "a.txt", SaveOptions::default())
            .unwrap();
        std::fs::remove_file(tmp.path().join(&meta.id)).unwrap();

        // When the file is fetched
        assert!(store.get(&meta.id).is_none());

        // Then the stale index entry was pruned — the next get of a second
        // file still works and the index no longer carries the orphan
        let other = store.save(b"y", "b.txt", SaveOptions::default()).unwrap();
        assert!(store.get(&other.id).is_some());
        let index: IndexFile =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("index.json")).unwrap())
                .unwrap();
        assert!(!index.files.iter().any(|m| m.id == meta.id));
    }

    #[test]
    fn should_write_ts_shaped_index_when_saving() {
        // Given a store with one save
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let meta = store.save(b"z", "z.txt", SaveOptions::default()).unwrap();

        // When the index is read as raw JSON
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("index.json")).unwrap())
                .unwrap();

        // Then it matches the TS IndexFile shape (version + snake_case files)
        assert_eq!(raw["version"], 1);
        assert_eq!(raw["files"][0]["id"], meta.id);
        assert_eq!(raw["files"][0]["media_type"], "application/octet-stream");
        assert_eq!(raw["files"][0]["size"], 1);
    }

    #[test]
    fn should_remove_entry_and_blob_when_deleting_file() {
        // Given a store with one saved file
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let meta = store
            .save(b"data", "a.txt", SaveOptions::default())
            .unwrap();
        assert!(tmp.path().join(&meta.id).exists());

        // When the file is deleted by id
        let deleted = store.delete(&meta.id).unwrap();

        // Then the delete reports success, the blob is gone, and the index
        // no longer carries the entry
        assert!(deleted, "a known id must report deleted");
        assert!(
            !tmp.path().join(&meta.id).exists(),
            "the blob file must be removed"
        );
        assert!(store.get(&meta.id).is_none());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("index.json")).unwrap())
                .unwrap();
        assert!(raw["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn should_return_false_when_deleting_unknown_or_malformed_id() {
        // Given a store with one file
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let meta = store.save(b"x", "a.txt", SaveOptions::default()).unwrap();

        // When unknown, malformed, or already-deleted ids are deleted
        assert!(!store.delete("f_nope").unwrap());
        assert!(!store.delete("../etc/passwd").unwrap());
        assert!(!store.delete("").unwrap());
        assert!(store.delete(&meta.id).unwrap());
        assert!(
            !store.delete(&meta.id).unwrap(),
            "a second delete of the same id is not-found"
        );
    }

    #[test]
    fn should_tolerate_missing_blob_when_deleting() {
        // Given a store whose recorded file lost its blob on disk
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let meta = store
            .save(b"data", "a.txt", SaveOptions::default())
            .unwrap();
        std::fs::remove_file(tmp.path().join(&meta.id)).unwrap();

        // When the file is deleted
        // Then the stale index entry is still pruned and success reported
        assert!(store.delete(&meta.id).unwrap());
        assert!(store.get(&meta.id).is_none());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("index.json")).unwrap())
                .unwrap();
        assert!(raw["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn should_roundtrip_when_saving_via_the_streaming_sink() {
        // Given a streaming sink receiving the payload in two chunks
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        let mut sink = store.begin().unwrap();
        let expected_id = sink.id().to_string();
        sink.write(b"hello ").unwrap();
        sink.write(b"world").unwrap();

        // When the sink is finished with overrides
        let meta = sink
            .finish(
                "upload.bin",
                SaveOptions {
                    name: Some("display.png".to_string()),
                    mime_type: Some("image/png".to_string()),
                    expires_in_sec: Some(3600),
                },
            )
            .unwrap();

        // Then the metadata and bytes round-trip like a buffered save
        assert_eq!(meta.id, expected_id);
        assert_eq!(meta.name, "display.png");
        assert_eq!(meta.media_type, "image/png");
        assert_eq!(meta.size, 11);
        assert!(meta.expires_at.is_some());
        let (fetched_meta, fetched) = store.get(&meta.id).expect("file should exist");
        assert_eq!(fetched, b"hello world");
        assert_eq!(fetched_meta, meta);
    }

    #[test]
    fn should_abort_partial_blob_when_sink_is_dropped() {
        // Given a streaming sink that received bytes but was abandoned
        let tmp = tempfile::tempdir().unwrap();
        let store = FileStore::new(tmp.path());
        {
            let mut sink = store.begin().unwrap();
            sink.write(b"partial").unwrap();
            let id = sink.id().to_string();
            drop(sink); // abandoned without finish

            // Then the partial blob is removed and nothing is indexed (the
            // index file may not even exist — `begin` never touches it)
            assert!(!tmp.path().join(&id).exists(), "partial blob must go");
        }
        if let Ok(raw) = std::fs::read_to_string(tmp.path().join("index.json")) {
            let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert!(raw["files"].as_array().unwrap().is_empty());
        }
    }
}
