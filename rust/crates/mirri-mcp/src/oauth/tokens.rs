//! Token persistence for the MCP OAuth flow.
//!
//! Tokens are stored per MCP server as a single JSON file under
//! `<storage-dir>/<server-name>.json` so that they survive restarts. Writes
//! follow an atomic-document pattern: serialize to a temp file in the same
//! directory, then rename over the destination.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::oauth::error::{OAuthError, OAuthResult};

/// Tokens are treated as expired when they expire within this window, so a
/// token that is about to die is refreshed proactively instead of failing a
/// request mid-flight.
pub const EXPIRY_LEEWAY: Duration = Duration::from_secs(30);

/// The OAuth token set for one MCP server, persisted to disk.
///
/// `expires_at` is a Unix timestamp in seconds (since the epoch). `None`
/// means the expiry is unknown; such tokens are never treated as expired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    /// The access token sent as `Authorization: Bearer <access_token>`.
    pub access_token: String,
    /// Refresh token, if the authorization server issued one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds since epoch) at which `access_token` expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Token type from the token response; defaults to `Bearer`.
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Granted scope, if the authorization server returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// OAuth client id used for this server. Persisted so that
    /// [`crate::oauth::OAuthClient::refresh`] can authenticate the client
    /// without re-running discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Token endpoint used to refresh. Persisted for the same reason as
    /// `client_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl TokenSet {
    /// Returns `true` when the access token is expired (or expires within
    /// [`EXPIRY_LEEWAY`]). Tokens with unknown expiry are never expired.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        expires_at <= current_unix_time_seconds(now) + EXPIRY_LEEWAY.as_secs() as i64
    }
}

/// Current time as a Unix timestamp in seconds.
pub(crate) fn current_unix_time_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Reads and writes [`TokenSet`] files under a storage directory.
#[derive(Debug, Clone)]
pub struct TokenStore {
    storage_dir: PathBuf,
}

impl TokenStore {
    /// Create a store rooted at `storage_dir` (created on first save).
    pub fn new(storage_dir: PathBuf) -> Self {
        Self { storage_dir }
    }

    /// The directory token files are written into.
    pub fn storage_dir(&self) -> &Path {
        &self.storage_dir
    }

    /// The path a given server's token set is stored at.
    ///
    /// `server_name` is sanitized so it can never escape the storage
    /// directory (e.g. `a/b` becomes `a_b`).
    pub fn path_for(&self, server_name: &str) -> PathBuf {
        self.storage_dir
            .join(format!("{}.json", sanitize_server_name(server_name)))
    }

    /// Load the cached token set for a server, if one exists and parses.
    ///
    /// A missing file yields `None`; a corrupt file yields `None` with a
    /// warning (the flow can simply re-authenticate).
    pub fn load(&self, server_name: &str) -> Option<TokenSet> {
        let path = self.path_for(server_name);
        let raw = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&raw) {
            Ok(tokens) => Some(tokens),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "ignoring corrupt OAuth token file"
                );
                None
            }
        }
    }

    /// Persist a token set atomically via the shared persistence primitive
    /// (unique temp file, fsync, rename, best-effort directory fsync).
    pub fn save(&self, server_name: &str, tokens: &TokenSet) -> OAuthResult<()> {
        std::fs::create_dir_all(&self.storage_dir).map_err(OAuthError::from)?;
        let path = self.path_for(server_name);
        // A serialization failure is a token-store write problem; wrap the
        // serde message in an `io::Error` so the `Io` variant (which now holds
        // the original `std::io::Error`) stays the single I/O error carrier.
        let raw = serde_json::to_string_pretty(tokens).map_err(|error| {
            OAuthError::Io(std::io::Error::other(format!(
                "failed to serialize tokens: {error}"
            )))
        })?;
        mirri_persistence::write_file_atomically(&path, raw.as_bytes())
            .map_err(OAuthError::from)?;
        tracing::debug!(path = %path.display(), "persisted OAuth token set");
        Ok(())
    }
}

/// Replace every character that is not safe in a file name with `_` so a
/// server name can never navigate out of the storage directory.
fn sanitize_server_name(server_name: &str) -> String {
    server_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tokens() -> TokenSet {
        TokenSet {
            access_token: "access-1".to_string(),
            refresh_token: Some("refresh-1".to_string()),
            expires_at: Some(1_800_000_000),
            token_type: "Bearer".to_string(),
            scope: Some("read write".to_string()),
            client_id: Some("client-1".to_string()),
            token_endpoint: Some("https://auth.example.com/token".to_string()),
        }
    }

    #[test]
    fn should_save_and_load_a_token_set_roundtrip_when_persisting_tokens() {
        let dir = std::env::temp_dir().join(format!(
            "mirri_mcp_token_store_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = TokenStore::new(dir.clone());
        store.save("example-server", &sample_tokens()).unwrap();
        let loaded = store.load("example-server").expect("token set should load");
        assert_eq!(loaded, sample_tokens());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn should_return_none_for_a_missing_token_set_when_loading_tokens() {
        let store = TokenStore::new(PathBuf::from("/nonexistent/mirri-mcp-tokens"));
        assert!(store.load("missing-server").is_none());
    }

    #[test]
    fn should_sanitize_server_names_in_file_paths_when_computing_token_file_path() {
        let store = TokenStore::new(PathBuf::from("/tmp/tokens"));
        assert_eq!(
            store.path_for("my server"),
            PathBuf::from("/tmp/tokens/my_server.json")
        );
        assert_eq!(
            store.path_for("../evil/server"),
            PathBuf::from("/tmp/tokens/.._evil_server.json")
        );
        assert_eq!(
            store.path_for("plain-name"),
            PathBuf::from("/tmp/tokens/plain-name.json")
        );
    }

    #[test]
    fn should_treat_tokens_with_unknown_expiry_as_not_expired_when_checking_expiry() {
        let tokens = TokenSet {
            expires_at: None,
            ..sample_tokens()
        };
        assert!(!tokens.is_expired(SystemTime::now()));
    }

    #[test]
    fn should_treat_tokens_as_expired_within_the_leeway_window_when_checking_expiry() {
        let now = current_unix_time_seconds(SystemTime::now());
        // Expires in 10s — inside the 30s leeway → expired.
        let soon = TokenSet {
            expires_at: Some(now + 10),
            ..sample_tokens()
        };
        assert!(soon.is_expired(SystemTime::now()));
        // Expires in 10 minutes — outside the leeway → still valid.
        let later = TokenSet {
            expires_at: Some(now + 600),
            ..sample_tokens()
        };
        assert!(!later.is_expired(SystemTime::now()));
    }
}
