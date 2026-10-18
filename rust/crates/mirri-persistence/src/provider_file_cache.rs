//! [`ProviderFileCache`] — the provider-side media file cache (T11a,
//! F-P8-5).
//!
//! The media materializer (`mirri-embedded`'s `AnthropicLlm`) uploads an
//! inline image once through the provider's Files API and records the
//! returned file id here, keyed by `(namespace, sha256(image bytes))`. Every
//! later chat that carries the same image rewrites it onto the cached
//! reference instead of re-uploading (or re-inlining base64).
//!
//! Layout: `{root}/{ns}/{sha256}.json` sidecars, where `ns` is
//! `{provider_type}@{sha256("{base_url}\0{api_key}")[0..16]}` — the
//! namespace isolates cache entries per provider *endpoint and
//! credential*: the same image against two gateways is two remote files
//! (T-11a-7), and so is the same image through two api keys on one shared
//! gateway, because a file id uploaded under one account is invisible to
//! the other (F-T11a-1, fixed in the T11b batch). `root` is the Mirri
//! home's
//! `provider-files/` subdirectory (home-level lifecycle, adjudication D3:
//! uploads are free, remote files are immutable, and the same id always
//! carries the same bytes — the cache semantics align exactly).
//!
//! Two layers: an in-process hot map (a `Mutex`-guarded `HashMap`, promoted
//! on cold-cache hits) and the on-disk sidecars (stateless over `root`, so
//! separate engine instances sharing a home see each other's uploads). A
//! cache *read* failure is a miss (the materializer re-uploads); a cache
//! *write* failure only warns (the hot entry still serves this process).
//! Neither ever fails the caller — the cache is an optimization, never a
//! correctness dependency.
//!
//! Deliberately not the files store (`FileStore`, user-facing uploads) nor
//! the blob service (`AgentBlobService`, wire-log offload): this is a
//! third, provider-scoped concern with different lifecycle and consumers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_utils::write_file_atomically;

/// A cached provider-side file: the uploaded content's remote reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedProviderFile {
    /// The provider's file id (e.g. `"file-abc123"`).
    pub file_id: String,
    /// The media type the content was uploaded under.
    pub mime_type: String,
    /// The uploaded content's size in bytes.
    pub size: u64,
    /// The provider-reported expiry (RFC 3339), when the provider returned
    /// one. `None` means no expiry was reported (Anthropic files default to
    /// no expiry). An expired entry is a lookup miss — the caller re-uploads
    /// and the entry is overwritten.
    pub expires_at: Option<String>,
}

/// The content-addressed provider files cache.
///
/// Construct one per provider adapter (or share one `Arc` across adapters
/// with the same root — the namespaces keep the entries apart).
pub struct ProviderFileCache {
    /// The cache root (e.g. `<mirri home>/provider-files/`).
    root: PathBuf,
    /// In-process layer, promoted from cold-cache hits.
    hot: Mutex<HashMap<(String, String), CachedProviderFile>>,
}

impl ProviderFileCache {
    /// Construct a cache rooted at `root` (created lazily on the first
    /// `record`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            hot: Mutex::new(HashMap::new()),
        }
    }

    /// The cache namespace for a provider endpoint and credential:
    /// `{provider_type}@{sha256("{base_url}\0{api_key}")[0..16]}`. Two
    /// different base URLs (e.g. two gateways speaking the same protocol)
    /// never share a remote file, and neither do two api keys against one
    /// shared gateway — per-user keys address different provider accounts,
    /// and a file id uploaded under one is not visible to the other
    /// (F-T11a-1). The `\0` separator keeps the concatenated identity
    /// unambiguous. Rotating the api key strands the old namespace's
    /// sidecars; they are harmless garbage (never read again under the new
    /// key) and are deliberately not cleaned up.
    pub fn namespace(provider_type: &str, base_url: &str, api_key: &str) -> String {
        let identity = format!("{base_url}\0{api_key}");
        let digest = sha256_hex(identity.as_bytes());
        format!("{provider_type}@{}", &digest[..16])
    }

    /// Look a content hash up in this namespace.
    ///
    /// Hot layer first, then the on-disk sidecar (promoted to hot). An
    /// expired entry is a miss and is invalidated in both layers. A missing
    /// or unreadable sidecar is a plain miss.
    pub fn lookup(&self, ns: &str, sha256: &str) -> Option<CachedProviderFile> {
        let hot_key = (ns.to_string(), sha256.to_string());
        {
            let mut hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = hot.get(&hot_key) {
                if is_expired(entry) {
                    // Drop the expired entry in place (never re-enter the
                    // lock through `invalidate` — `lookup` holds it here).
                    hot.remove(&hot_key);
                    drop(hot);
                    let _ = std::fs::remove_file(self.sidecar_path(ns, sha256));
                    return None;
                }
                return Some(entry.clone());
            }
        }

        let path = self.sidecar_path(ns, sha256);
        let raw = std::fs::read(&path).ok()?;
        let entry: CachedProviderFile = match serde_json::from_slice(&raw) {
            Ok(entry) => entry,
            // A corrupt sidecar is a miss; remove it so the next record
            // overwrites cleanly instead of shadowing forever.
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                return None;
            }
        };
        if is_expired(&entry) {
            self.invalidate(ns, sha256);
            return None;
        }
        self.hot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hot_key, entry.clone());
        Some(entry)
    }

    /// Record an upload result for `(ns, sha256)`.
    ///
    /// Persists the sidecar (best-effort: a write failure only warns — the
    /// hot entry still serves this process) and promotes to the hot layer.
    pub fn record(&self, ns: &str, sha256: &str, file: CachedProviderFile) {
        let path = self.sidecar_path(ns, sha256);
        match serde_json::to_vec(&file) {
            Ok(bytes) => {
                if let Err(error) = write_file_atomically(&path, &bytes) {
                    tracing::warn!(
                        sidecar = %path.display(),
                        %error,
                        "provider file cache sidecar write failed; keeping the in-process entry"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "provider file cache entry failed to serialize");
            }
        }
        self.hot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((ns.to_string(), sha256.to_string()), file);
    }

    /// Drop the entry for `(ns, sha256)` from both layers (an unknown entry
    /// is a no-op).
    pub fn invalidate(&self, ns: &str, sha256: &str) {
        self.hot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(ns.to_string(), sha256.to_string()));
        let _ = std::fs::remove_file(self.sidecar_path(ns, sha256));
    }

    /// `{root}/{ns}/{sha256}.json` — the cold-cache sidecar path.
    fn sidecar_path(&self, ns: &str, sha256: &str) -> PathBuf {
        self.root.join(ns).join(format!("{sha256}.json"))
    }
}

/// Whether an entry's reported expiry is in the past. An unparseable
/// timestamp is treated as not expired (fail-open: keep the cached
/// reference).
fn is_expired(entry: &CachedProviderFile) -> bool {
    entry
        .expires_at
        .as_deref()
        .and_then(|expires_at| chrono::DateTime::parse_from_rfc3339(expires_at).ok())
        .is_some_and(|expiry| expiry.with_timezone(&chrono::Utc) <= chrono::Utc::now())
}

/// Lowercase hex sha256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache over a fresh tempdir, plus a sample entry.
    fn fixture() -> (ProviderFileCache, CachedProviderFile, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = ProviderFileCache::new(dir.path());
        let entry = CachedProviderFile {
            file_id: "file-abc123".to_string(),
            mime_type: "image/png".to_string(),
            size: 8,
            expires_at: None,
        };
        (cache, entry, dir)
    }

    #[test]
    fn should_return_none_when_no_entry_was_recorded() {
        // Given an empty cache
        let (cache, _, _dir) = fixture();

        // When a content hash is looked up
        // Then it is a miss
        assert!(cache.lookup("anthropic@abc", "deadbeef").is_none());
    }

    #[test]
    fn should_return_the_recorded_entry_on_a_hit() {
        // Given a recorded entry
        let (cache, entry, _dir) = fixture();
        cache.record("anthropic@abc", "deadbeef", entry.clone());

        // When it is looked up
        // Then the exact entry comes back
        assert_eq!(cache.lookup("anthropic@abc", "deadbeef"), Some(entry));
    }

    #[test]
    fn should_derive_distinct_namespaces_when_base_urls_differ() {
        // Given two base URLs behind the same api key
        // Then the namespaces differ (endpoint isolation, T-11a-7) and carry
        // the provider type plus a 16-hex digest prefix
        let ns_a =
            ProviderFileCache::namespace("anthropic", "https://api.anthropic.com", "sk-test");
        let ns_b =
            ProviderFileCache::namespace("anthropic", "https://gateway.example.com", "sk-test");
        assert_ne!(ns_a, ns_b);
        for ns in [&ns_a, &ns_b] {
            assert!(ns.starts_with("anthropic@"), "namespace: {ns}");
            let digest = ns.split_once('@').expect("separator").1;
            assert_eq!(digest.len(), 16, "digest prefix: {ns}");
            assert!(
                digest.chars().all(|c| c.is_ascii_hexdigit()),
                "hex digest: {ns}"
            );
        }
    }

    #[test]
    fn should_derive_distinct_namespaces_when_api_keys_differ() {
        // Given two api keys against the same shared gateway base URL
        // Then the namespaces differ (credential isolation, F-T11a-1) with
        // the same shape as the endpoint isolation above — a file id
        // uploaded under one account must not be served to the other
        let ns_a =
            ProviderFileCache::namespace("anthropic", "https://gateway.example.com", "sk-user-a");
        let ns_b =
            ProviderFileCache::namespace("anthropic", "https://gateway.example.com", "sk-user-b");
        assert_ne!(ns_a, ns_b);
        for ns in [&ns_a, &ns_b] {
            assert!(ns.starts_with("anthropic@"), "namespace: {ns}");
            let digest = ns.split_once('@').expect("separator").1;
            assert_eq!(digest.len(), 16, "digest prefix: {ns}");
            assert!(
                digest.chars().all(|c| c.is_ascii_hexdigit()),
                "hex digest: {ns}"
            );
        }
    }

    #[test]
    fn should_isolate_entries_across_namespaces() {
        // Given the same content hash recorded under two namespaces
        let (cache, entry, _dir) = fixture();
        cache.record("anthropic@aaa", "deadbeef", entry.clone());
        cache.record("anthropic@bbb", "deadbeef", entry);

        // When one namespace is invalidated
        cache.invalidate("anthropic@aaa", "deadbeef");

        // Then only that namespace lost the entry
        assert!(cache.lookup("anthropic@aaa", "deadbeef").is_none());
        assert!(cache.lookup("anthropic@bbb", "deadbeef").is_some());
    }

    #[test]
    fn should_see_recorded_entries_from_a_fresh_instance_sharing_the_root() {
        // Given a recorded entry (hot + sidecar)
        let (cache, entry, dir) = fixture();
        cache.record("anthropic@abc", "deadbeef", entry.clone());

        // When a second cache instance is built over the same root
        // (cold cache — the hot map starts empty)
        let cold = ProviderFileCache::new(dir.path());

        // Then the sidecar is found and the entry promoted
        assert_eq!(cold.lookup("anthropic@abc", "deadbeef"), Some(entry));
    }

    #[test]
    fn should_treat_an_expired_entry_as_a_miss_and_drop_it() {
        // Given a recorded entry whose expiry is in the past
        let (cache, entry, _dir) = fixture();
        cache.record(
            "anthropic@abc",
            "deadbeef",
            CachedProviderFile {
                expires_at: Some("2020-01-01T00:00:00Z".to_string()),
                ..entry
            },
        );

        // When it is looked up
        // Then it is a miss
        assert!(cache.lookup("anthropic@abc", "deadbeef").is_none());
        // And the sidecar was removed (a re-record starts clean)
        assert!(cache.lookup("anthropic@abc", "deadbeef").is_none());
    }

    #[test]
    fn should_keep_an_unparseable_expiry_fail_open() {
        // Given a recorded entry whose expiry string is malformed
        let (cache, entry, _dir) = fixture();
        cache.record(
            "anthropic@abc",
            "deadbeef",
            CachedProviderFile {
                expires_at: Some("not-a-timestamp".to_string()),
                ..entry
            },
        );

        // When it is looked up
        // Then the entry is kept (fail-open: keep the cached reference)
        assert!(cache.lookup("anthropic@abc", "deadbeef").is_some());
    }

    #[test]
    fn should_write_the_sidecar_under_the_namespaced_root() {
        // Given a recorded entry
        let (cache, entry, dir) = fixture();
        cache.record("anthropic@abc", "deadbeef", entry);

        // When the sidecar path is inspected
        // Then the entry lives at {root}/{ns}/{sha}.json as JSON
        let path = dir.path().join("anthropic@abc").join("deadbeef.json");
        let raw = std::fs::read(&path).expect("sidecar exists");
        let sidecar: CachedProviderFile = serde_json::from_slice(&raw).expect("valid JSON");
        assert_eq!(sidecar.file_id, "file-abc123");
    }

    #[test]
    fn should_treat_a_corrupt_sidecar_as_a_miss_and_remove_it() {
        // Given a sidecar that is not valid JSON
        let dir = tempfile::tempdir().expect("tempdir");
        let ns_dir = dir.path().join("anthropic@abc");
        std::fs::create_dir_all(&ns_dir).expect("create ns dir");
        let path = ns_dir.join("deadbeef.json");
        std::fs::write(&path, b"not json").expect("write corrupt sidecar");
        let cache = ProviderFileCache::new(dir.path());

        // When the hash is looked up
        // Then it is a miss
        assert!(cache.lookup("anthropic@abc", "deadbeef").is_none());
        // And the corrupt sidecar was removed
        assert!(!path.exists(), "corrupt sidecar removed");
    }
}
