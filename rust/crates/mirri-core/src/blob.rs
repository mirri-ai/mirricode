//! Content-addressed blob storage for large media payloads.
//!
//! Mirrors the TypeScript `blob` domain. When message content parts carry
//! inline `data:` URIs that exceed a size threshold, the `AgentBlobService`
//! writes the decoded payload to disk under `{home_dir}/blobs/{sha256}`
//! (image mimes append their extension — `{sha256}.{ext}`, the TS
//! `media-originals` naming parity — so agents and OS tools can infer the
//! file type from the archive path) and
//! replaces the URI with a `blobref:{mime_type};{hash}` reference (the TS
//! semicolon spelling; the legacy slash spelling is still accepted when
//! reading). The inverse `load_parts` operation resolves `blobref:` URIs
//! back to `data:` URIs by reading from disk, with an in-memory LRU cache
//! to avoid repeated I/O.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use mirri_os::SyncFs;
use sha2::{Digest, Sha256};

use crate::message::{ContentPart, ImageUrlData};

/// Protocol prefix for blob references (`"blobref:"`).
pub const BLOBREF_PROTOCOL: &str = "blobref:";

/// Text inserted in place of media that cannot be loaded from disk.
pub const MISSING_MEDIA_PLACEHOLDER: &str = "[missing media]";

/// Default size (in bytes) above which an inline `data:` URI is offloaded.
pub const DEFAULT_THRESHOLD: usize = 4096;

/// Default maximum in-memory cache size for the LRU byte cache (50 MB).
pub const DEFAULT_MAX_CACHE_SIZE: usize = 50 * 1024 * 1024;

// ---------------------------------------------------------------------------
// BlobRef helpers
// ---------------------------------------------------------------------------

/// A parsed `blobref:` reference — the MIME type and content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub mime_type: String,
    pub hash: String,
}

/// Format a `blobref:` URI from its components.
///
/// The result is `blobref:{mime_type};{hash}` — the semicolon separator
/// matches the TS kernel's spelling.
pub fn format_blob_ref(mime_type: &str, hash: &str) -> String {
    format!("{BLOBREF_PROTOCOL}{mime_type};{hash}")
}

/// Parse a `blobref:` URI into its [`BlobRef`] components.
///
/// Accepts both the current `;` separator (the TS spelling, written by
/// [`format_blob_ref`]) and the legacy `/` separator written by older Rust
/// kernels (e.g. `blobref:image/png/abc123`). The separator is the *last*
/// occurrence of either character so that MIME types containing `/` (e.g.
/// `image/png`) are preserved correctly.
pub fn parse_blob_ref(url: &str) -> Option<BlobRef> {
    let rest = url.strip_prefix(BLOBREF_PROTOCOL)?;
    let separator_pos = match (rest.rfind(';'), rest.rfind('/')) {
        (Some(semicolon), Some(slash)) => semicolon.max(slash),
        (Some(semicolon), None) => semicolon,
        (None, Some(slash)) => slash,
        (None, None) => return None,
    };
    let mime_type = &rest[..separator_pos];
    let hash = &rest[separator_pos + 1..];
    if mime_type.is_empty() || hash.is_empty() {
        return None;
    }
    Some(BlobRef {
        mime_type: mime_type.to_string(),
        hash: hash.to_string(),
    })
}

/// Whether `url` is a `blobref:` URI.
pub fn is_blob_ref(url: &str) -> bool {
    url.starts_with(BLOBREF_PROTOCOL)
}

/// Format raw bytes as a `data:{mime_type};base64,{encoded}` URI.
pub fn format_data_uri(mime_type: &str, data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    format!("data:{mime_type};base64,{encoded}")
}

/// Parse a `data:{mime_type};base64,{payload}` URI into its MIME type and
/// base64 payload.
pub(crate) fn parse_data_uri(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let separator = ";base64,";
    let pos = rest.find(separator)?;
    let mime_type = &rest[..pos];
    let base64_payload = &rest[pos + separator.len()..];
    if mime_type.is_empty() || base64_payload.is_empty() {
        return None;
    }
    Some((mime_type, base64_payload))
}

// ---------------------------------------------------------------------------
// ByteLruCache
// ---------------------------------------------------------------------------

/// A simple byte-oriented LRU cache.
///
/// Entries are kept in a [`VecDeque`] where the front is the least-recently
/// used and the back is the most-recently used. The cache evicts from the
/// front when `total_size` exceeds `max_size`.
pub struct ByteLruCache {
    entries: VecDeque<(String, Vec<u8>)>,
    total_size: usize,
    max_size: usize,
}

impl ByteLruCache {
    /// Create a new cache that holds at most `max_size` bytes.
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            total_size: 0,
            max_size,
        }
    }

    /// Look up `key`, promoting it to the most-recently-used position.
    ///
    /// Returns a clone of the stored bytes, or `None` if the key is absent.
    pub fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, v) = self.entries.remove(pos)?;
        let result = v.clone();
        self.entries.push_back((k, v));
        Some(result)
    }

    /// Insert or update `key` with `value`.
    ///
    /// If the key already exists the old value is replaced (and its size
    /// subtracted from `total_size`). After insertion, entries are evicted
    /// from the front until `total_size <= max_size` (always retaining at
    /// least the just-inserted entry).
    pub fn put(&mut self, key: String, value: Vec<u8>) {
        // Don't cache items that alone exceed max_size — they would starve
        // the cache of everything else.
        if value.len() > self.max_size {
            return;
        }

        // Remove existing entry if present.
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == key) {
            let (_, old_value) = self.entries.remove(pos).expect("position checked above");
            self.total_size -= old_value.len();
        }

        self.total_size += value.len();
        self.entries.push_back((key, value));

        // Evict least-recently-used entries while over budget.
        while self.total_size > self.max_size && self.entries.len() > 1 {
            if let Some((_, old_value)) = self.entries.pop_front() {
                self.total_size -= old_value.len();
            }
        }
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total byte size of all cached values.
    pub fn total_size(&self) -> usize {
        self.total_size
    }
}

impl std::fmt::Debug for ByteLruCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteLruCache")
            .field("entries", &self.entries.len())
            .field("total_size", &self.total_size)
            .field("max_size", &self.max_size)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// AgentBlobService
// ---------------------------------------------------------------------------

/// Content-addressed blob storage for an agent.
///
/// Large `data:` URIs in message content parts are written to disk as
/// content-addressed blobs and replaced with `blobref:` URIs. Loading
/// reverses the process, with an in-memory LRU cache for recently accessed
/// blobs.
pub struct AgentBlobService {
    /// Root directory; blobs are stored under `{home_dir}/blobs/`.
    pub home_dir: PathBuf,
    /// In-memory LRU cache of blob bytes keyed by hash.
    pub cache: Mutex<ByteLruCache>,
    /// Minimum `data:` URI length to trigger offloading.
    pub threshold: usize,
    /// Filesystem provider backing blob persistence (zero-IO T13c; the
    /// engine default is `LocalSyncFs`).
    fs: Arc<dyn SyncFs>,
}

impl AgentBlobService {
    /// Create a new service rooted at `home_dir` with default settings.
    ///
    /// Filesystem access goes through the host-local `LocalSyncFs`; inject a
    /// double with [`AgentBlobService::with_fs`] instead.
    pub fn new(home_dir: PathBuf) -> Self {
        Self::with_fs(home_dir, Arc::new(mirri_os::LocalSyncFs::new()))
    }

    /// Create a new service whose blob persistence goes through `fs`.
    pub fn with_fs(home_dir: PathBuf, fs: Arc<dyn SyncFs>) -> Self {
        Self {
            home_dir,
            cache: Mutex::new(ByteLruCache::new(DEFAULT_MAX_CACHE_SIZE)),
            threshold: DEFAULT_THRESHOLD,
            fs,
        }
    }

    /// The candidate on-disk paths for a blob hash, in probe order: the
    /// preferred extension (when the mime type is known), the legacy
    /// extension-less spelling, then every supported extension. Reads and
    /// existence checks share this order so a blob written under any
    /// historical spelling resolves.
    fn blob_candidate_paths(&self, hash: &str, preferred: Option<&str>) -> Vec<PathBuf> {
        let dir = self.home_dir.join("blobs");
        let mut paths = Vec::with_capacity(2 + BLOB_EXTENSIONS.len());
        if let Some(ext) = preferred {
            paths.push(dir.join(format!("{hash}.{ext}")));
        }
        paths.push(dir.join(hash));
        for ext in BLOB_EXTENSIONS {
            paths.push(dir.join(format!("{hash}.{ext}")));
        }
        paths
    }

    /// The blob store's root directory (`{home_dir}/blobs`), materialized
    /// on demand (idempotent mkdir through the injected fs) — callers that
    /// admit the store into a path sandbox use this so the directory
    /// exists before the sandbox canonicalizes it.
    pub fn blobs_dir(&self) -> PathBuf {
        let dir = self.home_dir.join("blobs");
        let _ = self.fs.mkdir(&dir, true);
        dir
    }

    /// Write a base64-encoded payload to disk as a content-addressed blob.
    ///
    /// The blob is stored at `{home_dir}/blobs/{sha256}` plus the mime
    /// type's extension when it maps to a known image format. If a blob
    /// with the same hash already exists (under any historical naming
    /// spelling) it is not overwritten. Returns the
    /// `blobref:{mime_type}/{hash}` URI.
    pub fn write_blob(&self, mime_type: &str, base64_payload: &str) -> std::io::Result<String> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(base64_payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let hash_hex = sha256_hex(&data);

        let blob_dir = self.home_dir.join("blobs");
        self.fs.mkdir(&blob_dir, true).map_err(fs_to_io)?;
        let blob_path = blob_dir.join(blob_file_name(mime_type, &hash_hex));
        let known = self
            .blob_candidate_paths(&hash_hex, blob_extension(mime_type))
            .iter()
            .any(|candidate| self.fs.exists(candidate));
        if !known {
            self.fs.write_bytes(&blob_path, &data).map_err(fs_to_io)?;
        }

        // Populate the in-memory cache.
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(hash_hex.clone(), data);
        }

        Ok(format_blob_ref(mime_type, &hash_hex))
    }

    /// Read a blob from disk (or cache) by its hex hash.
    ///
    /// The hash alone names no extension, so every historical spelling is
    /// probed (see [`AgentBlobService::blob_candidate_paths`]); callers
    /// that know the mime type get the preferred extension first via
    /// [`AgentBlobService::load_blob_ref_url`].
    pub fn read_blob(&self, hash: &str) -> Option<Vec<u8>> {
        // Check cache first.
        if let Ok(mut cache) = self.cache.lock()
            && let Some(data) = cache.get(hash)
        {
            return Some(data);
        }

        // Fall back to disk, probing every candidate spelling.
        let data = self
            .blob_candidate_paths(hash, None)
            .into_iter()
            .find_map(|blob_path| self.fs.read_bytes(&blob_path).ok())?;

        // Cache for future lookups.
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(hash.to_string(), data.clone());
        }

        Some(data)
    }

    /// Load a `blobref:` URL and return its `data:` URI representation.
    ///
    /// Returns `None` if the URL is not a valid blob reference or the blob
    /// file cannot be read.
    pub fn load_blob_ref_url(&self, url: &str) -> Option<String> {
        let blob_ref = parse_blob_ref(url)?;
        let data = self
            .blob_candidate_paths(&blob_ref.hash, blob_extension(&blob_ref.mime_type))
            .into_iter()
            .find_map(|blob_path| self.fs.read_bytes(&blob_path).ok())?;
        Some(format_data_uri(&blob_ref.mime_type, &data))
    }

    /// Offload a single `data:` URI string to blob storage if it meets the
    /// threshold.
    ///
    /// Returns the original string if it is not a `data:` URI, is below the
    /// threshold, or the write fails.
    pub fn offload_string(&self, value: &str) -> String {
        if !value.starts_with("data:") || value.len() < self.threshold {
            return value.to_string();
        }
        let Some((mime_type, base64_payload)) = parse_data_uri(value) else {
            return value.to_string();
        };
        match self.write_blob(mime_type, base64_payload) {
            Ok(blob_ref_url) => blob_ref_url,
            Err(_) => value.to_string(),
        }
    }

    /// Multimodal spec E2 / ADR-004: persist the original bytes of every
    /// inline image part — unconditionally, threshold-independent
    /// (compression, when it exists, is a view concern) — and stamp the
    /// blob's filesystem path into the part's hidden `id` channel: the
    /// recovery handle for capability-gap placeholders and subagent
    /// delegation.
    ///
    /// Parts already carrying an archive path are left untouched
    /// (idempotent); remote URLs and engine-internal refs pass through
    /// (TS baseline: url-entry images are not archived).
    pub fn persist_image_originals(&self, parts: &mut [ContentPart]) {
        for part in parts.iter_mut() {
            let ContentPart::ImageUrl { image_url, .. } = part else {
                continue;
            };
            if image_url.id.is_some() {
                continue;
            }
            let Some((mime_type, base64_payload)) = parse_data_uri(&image_url.url) else {
                continue;
            };
            if let Some(path) = self.ensure_blob_path(mime_type, base64_payload) {
                image_url.id = Some(path);
            }
        }
    }

    /// Write (or dedupe) the decoded payload into the blob store and
    /// return the blob's absolute path (carrying the image extension —
    /// the TS `media-originals` naming parity, so agents and OS tools can
    /// infer the type from the handle). `None` on decode/write failure —
    /// fail-open: the part simply keeps no archive path and placeholders
    /// degrade to the no-path form.
    fn ensure_blob_path(&self, mime_type: &str, base64_payload: &str) -> Option<String> {
        let data = base64::engine::general_purpose::STANDARD
            .decode(base64_payload)
            .ok()?;
        let hash_hex = sha256_hex(&data);
        let blob_dir = self.home_dir.join("blobs");
        self.fs.mkdir(&blob_dir, true).ok()?;
        let blob_path = blob_dir.join(blob_file_name(mime_type, &hash_hex));
        let known = self
            .blob_candidate_paths(&hash_hex, blob_extension(mime_type))
            .iter()
            .any(|candidate| self.fs.exists(candidate));
        if !known {
            self.fs.write_bytes(&blob_path, &data).ok()?;
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.put(hash_hex, data);
        }
        Some(blob_path.to_string_lossy().into_owned())
    }

    /// Scan content parts for `data:` URIs at or above the threshold and
    /// replace them with `blobref:` URIs.
    ///
    /// Text and Think parts are passed through unchanged.
    pub fn offload_parts(&self, parts: &[ContentPart]) -> Vec<ContentPart> {
        parts
            .iter()
            .map(|part| match part {
                ContentPart::ImageUrl { image_url, detail } => {
                    let new_url = self.offload_string(&image_url.url);
                    if new_url != image_url.url {
                        ContentPart::ImageUrl {
                            image_url: ImageUrlData {
                                url: new_url,
                                detail: image_url.detail.clone(),
                                id: image_url.id.clone(),
                            },
                            detail: detail.clone(),
                        }
                    } else {
                        part.clone()
                    }
                }
                ContentPart::AudioUrl { audio_url } => {
                    let new_url = self.offload_string(audio_url);
                    if new_url != *audio_url {
                        ContentPart::AudioUrl { audio_url: new_url }
                    } else {
                        part.clone()
                    }
                }
                ContentPart::VideoUrl { video_url } => {
                    let new_url = self.offload_string(video_url);
                    if new_url != *video_url {
                        ContentPart::VideoUrl { video_url: new_url }
                    } else {
                        part.clone()
                    }
                }
                ContentPart::File {
                    file_id,
                    data,
                    name,
                    media_type,
                    size,
                } => {
                    // Offload the resolved inline content; the reference form
                    // (`file_id` set, `data` absent) is never in memory and has
                    // nothing to offload.
                    let new_data = data.as_ref().map(|uri| self.offload_string(uri));
                    if new_data.as_ref() != data.as_ref() {
                        ContentPart::File {
                            file_id: file_id.clone(),
                            data: new_data,
                            name: name.clone(),
                            media_type: media_type.clone(),
                            size: *size,
                        }
                    } else {
                        part.clone()
                    }
                }
                _ => part.clone(),
            })
            .collect()
    }

    /// Scan content parts for `blobref:` URIs and replace them with `data:`
    /// URIs loaded from disk.
    ///
    /// If a blob cannot be loaded, the part is replaced with a Text part
    /// containing [`MISSING_MEDIA_PLACEHOLDER`].
    pub fn load_parts(&self, parts: &[ContentPart]) -> Vec<ContentPart> {
        parts
            .iter()
            .map(|part| match part {
                ContentPart::ImageUrl { image_url, detail } if is_blob_ref(&image_url.url) => {
                    match self.load_blob_ref_url(&image_url.url) {
                        Some(data_uri) => ContentPart::ImageUrl {
                            image_url: ImageUrlData {
                                url: data_uri,
                                detail: image_url.detail.clone(),
                                id: image_url.id.clone(),
                            },
                            detail: detail.clone(),
                        },
                        None => ContentPart::Text {
                            text: MISSING_MEDIA_PLACEHOLDER.to_string(),
                        },
                    }
                }
                ContentPart::AudioUrl { audio_url } if is_blob_ref(audio_url) => {
                    match self.load_blob_ref_url(audio_url) {
                        Some(data_uri) => ContentPart::AudioUrl {
                            audio_url: data_uri,
                        },
                        None => ContentPart::Text {
                            text: MISSING_MEDIA_PLACEHOLDER.to_string(),
                        },
                    }
                }
                ContentPart::VideoUrl { video_url } if is_blob_ref(video_url) => {
                    match self.load_blob_ref_url(video_url) {
                        Some(data_uri) => ContentPart::VideoUrl {
                            video_url: data_uri,
                        },
                        None => ContentPart::Text {
                            text: MISSING_MEDIA_PLACEHOLDER.to_string(),
                        },
                    }
                }
                ContentPart::File {
                    file_id,
                    data,
                    name,
                    media_type,
                    size,
                } if data.as_deref().is_some_and(is_blob_ref) => {
                    match self.load_blob_ref_url(data.as_deref().expect("guarded above")) {
                        Some(data_uri) => ContentPart::File {
                            file_id: file_id.clone(),
                            data: Some(data_uri),
                            name: name.clone(),
                            media_type: media_type.clone(),
                            size: *size,
                        },
                        None => ContentPart::Text {
                            text: MISSING_MEDIA_PLACEHOLDER.to_string(),
                        },
                    }
                }
                _ => part.clone(),
            })
            .collect()
    }
}

/// The filename extension a blob's mime type maps to — the TS
/// `media-originals` parity map (`image-originals.ts` `MIME_EXTENSION`).
/// The extension lets agents and OS tools infer the file type from the
/// archive path (the recovery handle `id` and the wire-offload store share
/// one naming). Unknown mimes stay extension-less — the legacy spelling
/// existing sessions already hold.
fn blob_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        "image/tiff" => Some("tif"),
        _ => None,
    }
}

/// Every extension a blob may carry on disk, in probe order for hash-only
/// lookups (a `blobref:` records the mime, but `read_blob` takes just the
/// hash). The legacy extension-less spelling is probed first.
const BLOB_EXTENSIONS: &[&str] = &["png", "jpg", "gif", "webp", "bmp", "tif"];

/// The on-disk file name for a blob: the content hash plus the mapped
/// extension when the mime type is a known image type.
fn blob_file_name(mime_type: &str, hash: &str) -> String {
    match blob_extension(mime_type) {
        Some(ext) => format!("{hash}.{ext}"),
        None => hash.to_string(),
    }
}

/// Hex-encode the SHA-256 digest of `data` — the content-addressing key of
/// the blob store (shared by `write_blob` and the ingest-side archive path).
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Convert an injected-filesystem failure into the `std::io::Error` this
/// module historically surfaced (the write path degrades to returning the
/// original URI, so only the message needs to survive the boundary).
fn fs_to_io(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(format!("{error:#}"))
}

impl std::fmt::Debug for AgentBlobService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentBlobService")
            .field("home_dir", &self.home_dir)
            .field("threshold", &self.threshold)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- BlobRef parsing / formatting ---------------------------------------

    #[test]
    fn should_return_mime_and_hash_when_parsing_blobref_url() {
        let url = "blobref:image/png/abc123def456";
        let blob_ref = parse_blob_ref(url).expect("should parse");
        assert_eq!(blob_ref.mime_type, "image/png");
        assert_eq!(blob_ref.hash, "abc123def456");
    }

    #[test]
    fn should_return_mime_and_hash_when_parsing_ts_blobref_url_with_semicolon() {
        let url = "blobref:image/png;abc123def456";
        let blob_ref = parse_blob_ref(url).expect("should parse");
        assert_eq!(blob_ref.mime_type, "image/png");
        assert_eq!(blob_ref.hash, "abc123def456");
    }

    #[test]
    fn should_return_none_when_parsing_non_blobref_url() {
        assert!(parse_blob_ref("https://example.com/image.png").is_none());
        assert!(parse_blob_ref("data:image/png;base64,abc").is_none());
    }

    #[test]
    fn should_return_none_when_parsing_blobref_without_separator() {
        assert!(parse_blob_ref("blobref:noseparatorhere").is_none());
    }

    #[test]
    fn should_return_none_when_parsing_blobref_with_empty_mime_or_hash() {
        assert!(parse_blob_ref("blobref:/hashonly").is_none());
        assert!(parse_blob_ref("blobref:image/png/").is_none());
        assert!(parse_blob_ref("blobref:;hashonly").is_none());
        assert!(parse_blob_ref("blobref:image/png;").is_none());
    }

    #[test]
    fn should_produce_ts_blobref_uri_when_formatting_mime_and_hash() {
        let url = format_blob_ref("image/jpeg", "deadbeef");
        assert_eq!(url, "blobref:image/jpeg;deadbeef");
    }

    #[test]
    fn should_detect_blobref_urls_when_checking_is_blob_ref() {
        assert!(is_blob_ref("blobref:image/png/abc"));
        assert!(!is_blob_ref("https://example.com/image.png"));
        assert!(!is_blob_ref("data:image/png;base64,abc"));
    }

    // -- Data URI helpers ----------------------------------------------------

    #[test]
    fn should_round_trip_through_base64_when_formatting_data_uri() {
        let data = b"hello world";
        let uri = format_data_uri("text/plain", data);
        assert!(uri.starts_with("data:text/plain;base64,"));
        let encoded = &uri["data:text/plain;base64,".len()..];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, data);
    }

    // -- ByteLruCache --------------------------------------------------------

    #[test]
    fn should_return_none_when_getting_from_empty_cache() {
        let mut cache = ByteLruCache::new(1024);
        assert!(cache.get("missing").is_none());
    }

    #[test]
    fn should_return_value_and_promote_entry_when_getting_cached_entry() {
        let mut cache = ByteLruCache::new(10);
        cache.put("a".to_string(), vec![0; 4]);
        cache.put("b".to_string(), vec![0; 4]);

        // Access "a" to promote it to most-recently-used.
        let val = cache.get("a").expect("should find key a");
        assert_eq!(val, vec![0; 4]);

        // Now "b" is the least-recently-used (front) and should be evicted first.
        cache.put("c".to_string(), vec![0; 4]);
        // total = 4 + 4 + 4 = 12 > 10; "b" (front) should be evicted.
        assert!(cache.get("b").is_none(), "b should have been evicted");
        assert!(cache.get("a").is_some(), "a should still be cached");
        assert!(cache.get("c").is_some(), "c should still be cached");
    }

    #[test]
    fn should_evict_oldest_entry_when_putting_into_over_limit_cache() {
        let mut cache = ByteLruCache::new(10);
        cache.put("first".to_string(), vec![0; 4]);
        cache.put("second".to_string(), vec![0; 4]);
        cache.put("third".to_string(), vec![0; 4]);
        // total = 12 > 10; "first" should be evicted.
        assert!(cache.get("first").is_none());
        assert!(cache.get("second").is_some());
        assert!(cache.get("third").is_some());
    }

    #[test]
    fn should_not_cache_value_when_putting_value_larger_than_max() {
        let mut cache = ByteLruCache::new(10);
        cache.put("big".to_string(), vec![0; 100]);
        assert!(cache.get("big").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn should_update_value_when_putting_existing_key() {
        let mut cache = ByteLruCache::new(1024);
        cache.put("key".to_string(), vec![1]);
        cache.put("key".to_string(), vec![2, 3]);
        let val = cache.get("key").expect("should find key");
        assert_eq!(val, vec![2, 3]);
        assert_eq!(cache.len(), 1);
    }

    // -- AgentBlobService ----------------------------------------------------

    #[test]
    fn should_return_blobref_when_offloading_large_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0xAB; 8000];
        let data_uri = format_data_uri("image/png", &data);

        let result = service.offload_string(&data_uri);
        assert!(is_blob_ref(&result), "should be a blobref URI: {result}");

        // The blob file should exist on disk, carrying the mapped extension.
        let blob_ref = parse_blob_ref(&result).unwrap();
        let blob_path = dir
            .path()
            .join("blobs")
            .join(format!("{}.png", blob_ref.hash));
        assert!(blob_path.exists(), "blob file should exist");
    }

    #[test]
    fn should_return_original_when_offloading_small_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data_uri = format_data_uri("text/plain", b"hi");
        let result = service.offload_string(&data_uri);
        assert_eq!(result, data_uri, "small URI should not be offloaded");
    }

    #[test]
    fn should_return_original_when_offloading_non_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let result = service.offload_string("https://example.com/image.png");
        assert_eq!(result, "https://example.com/image.png");
    }

    #[test]
    fn should_return_data_uri_when_loading_blobref_url() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0x42; 8000];
        let data_uri = format_data_uri("image/png", &data);
        let blob_ref_url = service.offload_string(&data_uri);
        assert!(is_blob_ref(&blob_ref_url));

        let loaded = service
            .load_blob_ref_url(&blob_ref_url)
            .expect("should load blob");
        assert!(loaded.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn should_return_none_when_loading_missing_blobref() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let result = service.load_blob_ref_url("blobref:image/png/nonexistenthash");
        assert!(result.is_none());
    }

    #[test]
    fn should_replace_image_url_with_blobref_when_offloading_parts() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0x77; 8000];
        let data_uri = format_data_uri("image/png", &data);
        let parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: data_uri,
                detail: None,
                id: None,
            },
            detail: None,
        }];

        let offloaded = service.offload_parts(&parts);
        match &offloaded[0] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(is_blob_ref(&image_url.url));
            }
            other => panic!("expected ImageUrl, got {other:?}"),
        }
    }

    #[test]
    fn should_replace_with_data_uri_when_loading_blobref_parts() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0x99; 8000];
        let data_uri = format_data_uri("audio/wav", &data);
        let blob_ref_url = service.offload_string(&data_uri);

        let parts = vec![ContentPart::AudioUrl {
            audio_url: blob_ref_url,
        }];

        let loaded = service.load_parts(&parts);
        match &loaded[0] {
            ContentPart::AudioUrl { audio_url } => {
                assert!(audio_url.starts_with("data:audio/wav;base64,"));
            }
            other => panic!("expected AudioUrl, got {other:?}"),
        }
    }

    #[test]
    fn should_return_original_data_when_loading_back_offloaded_parts() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let original_data = vec![0x55; 6000];
        let data_uri = format_data_uri("video/mp4", &original_data);

        let parts = vec![ContentPart::VideoUrl {
            video_url: data_uri.clone(),
        }];

        // Offload then load back.
        let offloaded = service.offload_parts(&parts);
        let loaded = service.load_parts(&offloaded);

        match &loaded[0] {
            ContentPart::VideoUrl { video_url } => {
                // Extract the base64 from the loaded data URI and decode.
                let (_, b64) = parse_data_uri(video_url).unwrap();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap();
                assert_eq!(decoded, original_data);
            }
            other => panic!("expected VideoUrl, got {other:?}"),
        }
    }

    #[test]
    fn should_replace_with_placeholder_when_loading_parts_with_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "blobref:image/png/nonexistenthash".to_string(),
                detail: None,
                id: None,
            },
            detail: None,
        }];

        let loaded = service.load_parts(&parts);
        match &loaded[0] {
            ContentPart::Text { text } => {
                assert_eq!(text, MISSING_MEDIA_PLACEHOLDER);
            }
            other => panic!("expected Text placeholder, got {other:?}"),
        }
    }

    #[test]
    fn should_leave_text_parts_unchanged_when_offloading() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let parts = vec![ContentPart::Text {
            text: "hello world".to_string(),
        }];

        let offloaded = service.offload_parts(&parts);
        assert_eq!(offloaded, parts);
    }

    #[test]
    fn should_produce_same_hash_when_writing_blob_twice_with_same_data() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0x11; 5000];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);

        let url1 = service.write_blob("image/png", &encoded).unwrap();
        let url2 = service.write_blob("image/png", &encoded).unwrap();

        assert_eq!(url1, url2, "same data should produce same blobref");
    }

    #[test]
    fn should_return_data_when_reading_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let data = vec![0x33; 5000];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let url = service.write_blob("image/png", &encoded).unwrap();
        let blob_ref = parse_blob_ref(&url).unwrap();

        let read_data = service.read_blob(&blob_ref.hash).expect("should read blob");
        assert_eq!(read_data, data);
    }

    #[test]
    fn should_replace_file_data_with_blobref_when_offloading_parts() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        // Given a file part carrying a large inline data URI
        let data = vec![0x64; 8000];
        let data_uri = format_data_uri("application/pdf", &data);
        let parts = vec![ContentPart::File {
            file_id: None,
            data: Some(data_uri),
            name: Some("report.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            size: Some(8000),
        }];

        // When the parts are offloaded
        let offloaded = service.offload_parts(&parts);
        match &offloaded[0] {
            ContentPart::File { data, name, .. } => {
                let url = data.as_ref().expect("data should remain");
                assert!(is_blob_ref(url), "file data should be a blobref: {url}");
                assert_eq!(name.as_deref(), Some("report.pdf"));
            }
            other => panic!("expected File, got {other:?}"),
        }

        // And loading them back restores the inline data URI
        let loaded = service.load_parts(&offloaded);
        match &loaded[0] {
            ContentPart::File { data, .. } => {
                let url = data.as_ref().expect("data should remain");
                assert!(url.starts_with("data:application/pdf;base64,"));
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn should_replace_with_placeholder_when_loading_file_part_with_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());

        let parts = vec![ContentPart::File {
            file_id: None,
            data: Some("blobref:application/pdf/nonexistenthash".to_string()),
            name: Some("gone.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            size: None,
        }];

        let loaded = service.load_parts(&parts);
        match &loaded[0] {
            ContentPart::Text { text } => {
                assert_eq!(text, MISSING_MEDIA_PLACEHOLDER);
            }
            other => panic!("expected Text placeholder, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // T-13c1-1 — in-memory SyncFs double (zero-IO T13c)
    // -----------------------------------------------------------------------

    /// In-memory [`SyncFs`] double: blob bytes live in a map and the
    /// directory / write operations are recorded so tests can assert the blob
    /// store went through the injected provider (and with which paths).
    struct MemoryBlobFs {
        files: Mutex<std::collections::HashMap<PathBuf, Vec<u8>>>,
        dirs: Mutex<std::collections::HashSet<PathBuf>>,
        mkdir_calls: Mutex<Vec<PathBuf>>,
        write_bytes_calls: Mutex<Vec<(PathBuf, Vec<u8>)>>,
        /// When set, `write_bytes` (and `mkdir`) fail — the injected-fs
        /// failure path the fail-open branches must survive.
        fail_writes: std::sync::atomic::AtomicBool,
    }

    impl MemoryBlobFs {
        fn new() -> Self {
            Self {
                files: Mutex::new(std::collections::HashMap::new()),
                dirs: Mutex::new(std::collections::HashSet::new()),
                mkdir_calls: Mutex::new(Vec::new()),
                write_bytes_calls: Mutex::new(Vec::new()),
                fail_writes: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn mkdir_calls(&self) -> Vec<PathBuf> {
            self.mkdir_calls.lock().unwrap().clone()
        }

        fn write_bytes_calls(&self) -> Vec<(PathBuf, Vec<u8>)> {
            self.write_bytes_calls.lock().unwrap().clone()
        }

        /// Make every subsequent write-path operation fail.
        fn fail_writes(&self) {
            self.fail_writes
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl SyncFs for MemoryBlobFs {
        fn mkdir(&self, path: &std::path::Path, _recursive: bool) -> anyhow::Result<()> {
            self.mkdir_calls.lock().unwrap().push(path.to_path_buf());
            if self.fail_writes.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("injected mkdir failure: {}", path.display());
            }
            self.dirs.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }

        fn write_bytes(&self, path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
            self.write_bytes_calls
                .lock()
                .unwrap()
                .push((path.to_path_buf(), bytes.to_vec()));
            if self.fail_writes.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("injected write failure: {}", path.display());
            }
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }

        fn exists(&self, path: &std::path::Path) -> bool {
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();
            files.contains_key(path) || dirs.contains(path)
        }

        fn read_bytes(&self, path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
        }
    }

    #[test]
    fn should_append_the_image_extension_when_persisting_an_original() {
        // Given 一个内联 PNG data-URI 图片部件
        // When 入口留档
        // Then 留档文件名携带 png 扩展名（TS media-originals 命名对齐），
        // id 指向该真实路径
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());
        let mut parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: format_data_uri("image/png", &[0xAB; 100]),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        service.persist_image_originals(&mut parts);
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected ImageUrl");
        };
        let id = image_url.id.as_deref().expect("id 应被回写");
        assert!(id.ends_with(".png"), "留档路径应携带 png 扩展名: {id}");
        assert!(std::path::Path::new(id).exists(), "留档文件应存在: {id}");
    }

    #[test]
    fn should_map_each_supported_image_mime_to_its_extension() {
        // Given 各受支持的图片 MIME
        // When 求扩展名映射
        // Then 与 TS image-originals 的 MIME_EXTENSION 逐项一致
        let cases = [
            ("image/png", "png"),
            ("image/jpeg", "jpg"),
            ("image/jpg", "jpg"),
            ("image/gif", "gif"),
            ("image/webp", "webp"),
            ("image/bmp", "bmp"),
            ("image/tiff", "tif"),
        ];
        for (mime, expected) in cases {
            assert_eq!(blob_extension(mime), Some(expected), "mime: {mime}");
        }
        // 未知 MIME 不加扩展名（保持存量裸哈希拼写）
        assert_eq!(blob_extension("application/pdf"), None);
        assert_eq!(blob_extension("image/x-unknown"), None);
    }

    #[test]
    fn should_read_a_legacy_extensionless_blob_by_its_bare_hash() {
        // Given 一个按旧拼写（裸哈希文件名）落盘的 blob
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0x42; 32];
        let hash = sha256_hex(&data);
        std::fs::create_dir_all(dir.path().join("blobs")).unwrap();
        std::fs::write(dir.path().join("blobs").join(&hash), &data).unwrap();

        // When 按哈希读回（不带任何扩展名信息）
        let service = AgentBlobService::new(dir.path().to_path_buf());

        // Then 裸哈希拼写仍可读——存量会话的留档/卸载文件不受本次命名变更影响
        assert_eq!(service.read_blob(&hash), Some(data.clone()));
        assert_eq!(
            service.load_blob_ref_url(&format_blob_ref("image/png", &hash)),
            Some(format_data_uri("image/png", &data))
        );
    }

    #[test]
    fn should_persist_image_original_and_stamp_path_into_id() {
        // Given 一个内联 data-URI 图片部件
        // When 入口留档
        // Then 原件写入 blobs/<sha256>.<ext>（TS media-originals 命名对齐）且 id 指向该真实路径（US2.1/ADR-004）
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());
        let data = vec![0xAB; 100]; // 低于 offload 阈值也必须留档（无条件）
        let data_uri = format_data_uri("image/png", &data);
        let mut parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: data_uri.clone(),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        service.persist_image_originals(&mut parts);
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected ImageUrl");
        };
        let id = image_url.id.as_deref().expect("id 应被回写");
        let path = std::path::Path::new(id);
        assert!(path.exists(), "留档文件应存在: {id}");
        assert!(path.starts_with(dir.path().join("blobs")));
        assert_eq!(image_url.url, data_uri, "消息内视图不变（压缩与留档正交）");
    }

    #[test]
    fn should_be_idempotent_when_part_already_carries_id() {
        // Given 已带 id 的部件
        // When 再次留档
        // Then id 不被覆盖（幂等）
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());
        let mut parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: format_data_uri("image/png", &[0xAB; 10]),
                detail: None,
                id: Some("/already/here.png".into()),
            },
            detail: None,
        }];
        service.persist_image_originals(&mut parts);
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected ImageUrl");
        };
        assert_eq!(image_url.id.as_deref(), Some("/already/here.png"));
    }

    #[test]
    fn should_leave_non_data_uri_parts_without_id_when_persisting() {
        // Given 一个远程 URL 图片部件（TS baseline: url-entry images are not archived）
        // When 入口留档
        // Then 部件原样保留，无 id
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());
        let mut parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "https://example.com/image.png".to_string(),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        service.persist_image_originals(&mut parts);
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected ImageUrl");
        };
        assert!(image_url.id.is_none(), "远程 URL 不留档");
        assert_eq!(image_url.url, "https://example.com/image.png");
    }

    #[test]
    fn should_fail_open_when_the_blob_write_fails() {
        // Given 一个写入必失败的注入 fs（Task 2 登记的低成本补测：
        // ensure_blob_path 的 fail-open 分支）
        // When 入口留档
        // Then 部件原样保留、无 id（不 panic、不报错——降级为无路径形态）
        let fs = Arc::new(MemoryBlobFs::new());
        fs.fail_writes();
        let service = AgentBlobService::with_fs(
            PathBuf::from("/virtual/home"),
            Arc::clone(&fs) as Arc<dyn SyncFs>,
        );
        let data_uri = format_data_uri("image/png", &[0xAB; 100]);
        let mut parts = vec![ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: data_uri.clone(),
                detail: None,
                id: None,
            },
            detail: None,
        }];
        service.persist_image_originals(&mut parts);
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected ImageUrl");
        };
        assert!(
            image_url.id.is_none(),
            "写入失败时部件不得携带留档路径: {:?}",
            image_url.id
        );
        assert_eq!(image_url.url, data_uri, "消息内视图原样保留（fail-open）");
    }

    #[test]
    fn should_write_and_read_blobs_through_the_injected_sync_fs_double() {
        // Given a blob service whose filesystem is an in-memory double
        let home = PathBuf::from("/virtual/home");
        let fs = Arc::new(MemoryBlobFs::new());
        let service = AgentBlobService::with_fs(home.clone(), Arc::clone(&fs) as Arc<dyn SyncFs>);

        // When a large data URI is offloaded
        let data = vec![0xAB; 8000];
        let data_uri = format_data_uri("image/png", &data);
        let blob_ref_url = service.offload_string(&data_uri);

        // Then the blob directory is created and the decoded bytes are
        // written under it, content-addressed by hash and carrying the
        // mapped image extension
        let blob_ref = parse_blob_ref(&blob_ref_url).expect("should be a blobref");
        let blob_path = home.join("blobs").join(format!("{}.png", blob_ref.hash));
        assert_eq!(fs.mkdir_calls(), vec![home.join("blobs")]);
        assert_eq!(
            fs.write_bytes_calls(),
            vec![(blob_path.clone(), data.clone())]
        );

        // And offloading the same payload again does not rewrite the blob
        service.offload_string(&data_uri);
        assert_eq!(
            fs.write_bytes_calls().len(),
            1,
            "an existing blob must not be rewritten"
        );

        // And a fresh service with a cold cache reads the blob back through
        // the double and reconstitutes the data URI
        let cold = AgentBlobService::with_fs(home, Arc::clone(&fs) as Arc<dyn SyncFs>);
        assert_eq!(cold.read_blob(&blob_ref.hash), Some(data));
        assert!(
            cold.load_blob_ref_url(&blob_ref_url)
                .expect("should load")
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn should_return_none_when_the_double_has_no_blob() {
        // Given a blob service over an empty in-memory double
        let fs = Arc::new(MemoryBlobFs::new());
        let service = AgentBlobService::with_fs(
            PathBuf::from("/virtual/home"),
            Arc::clone(&fs) as Arc<dyn SyncFs>,
        );

        // When a missing blob reference is loaded
        let loaded = service.load_blob_ref_url("blobref:image/png/nonexistenthash");

        // Then the load fails without touching the host filesystem
        assert!(loaded.is_none());
        assert!(fs.write_bytes_calls().is_empty());
    }
}
