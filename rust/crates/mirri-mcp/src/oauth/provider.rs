//! Connect-path token provider for one MCP server.
//!
//! [`ServerTokenProvider`] is the wiring piece between the OAuth module and
//! the HTTP/SSE transports: a transport created with it asks for a bearer
//! token before each request, so a cached token is attached and an expired
//! one is refreshed transparently.
//!
//! The provider is attached at connect time only when the server's config
//! declares `auth: {type: "oauth"}` **and** a cached token set already
//! exists (the TS parity stance: without tokens the plain client connects,
//! the server answers 401, and the connection lands in `needs-auth` for the
//! authenticate tool to drive the flow).

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::McpError;
use crate::oauth::flow::OAuthClient;
use crate::transport::{TokenProvider, TransportFuture};

/// A [`TokenProvider`] bound to one MCP server name.
///
/// Created by the connection manager's connect path (and by the embedded
/// authenticate tool's already-authorized fast path); one instance per
/// connect attempt sharing the token store directory.
pub struct ServerTokenProvider {
    client: Arc<OAuthClient>,
    server: String,
}

impl ServerTokenProvider {
    /// A provider reading and refreshing tokens for `server_name` under
    /// `storage_dir`.
    pub fn new(http: reqwest::Client, storage_dir: PathBuf, server_name: &str) -> Self {
        Self {
            client: Arc::new(OAuthClient::new(http, storage_dir)),
            server: server_name.to_string(),
        }
    }
}

impl TokenProvider for ServerTokenProvider {
    fn get_token(&self) -> TransportFuture<'_, String> {
        let client = Arc::clone(&self.client);
        let server = self.server.clone();
        Box::pin(async move {
            // `get_valid_token` serves the cached token when it is still
            // valid and refreshes it otherwise (persisting the rotation).
            client.get_valid_token(&server).await.map_err(|error| {
                tracing::warn!(
                    server = %server, %error,
                    "MCP OAuth token fetch failed; the server requires authorization"
                );
                McpError::AuthRequired
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::oauth::TokenSet;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirri_mcp_provider_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn unexpired_set() -> TokenSet {
        TokenSet {
            access_token: "cached-token".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(
                SystemTime::now()
                    .checked_add(Duration::from_secs(3600))
                    .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64)
                    .unwrap_or(1_800_000_000),
            ),
            token_type: "Bearer".to_string(),
            scope: None,
            client_id: Some("client".to_string()),
            token_endpoint: None,
        }
    }

    /// Given a valid cached token set, when the provider is asked for a
    /// token, then the cached value is returned with no network round-trip.
    #[tokio::test]
    async fn should_return_the_cached_token_when_it_is_still_valid() {
        let dir = unique_temp_dir("valid");
        crate::oauth::TokenStore::new(dir.clone())
            .save("srv", &unexpired_set())
            .unwrap();
        let provider = ServerTokenProvider::new(reqwest::Client::new(), dir.clone(), "srv");
        assert_eq!(provider.get_token().await.unwrap(), "cached-token");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Given no cached token set, when the provider is asked for a token,
    /// then it surfaces the authorization requirement (AuthRequired) — the
    /// connect path maps that into the `needs-auth` state.
    #[tokio::test]
    async fn should_surface_auth_required_when_no_tokens_are_cached() {
        let dir = unique_temp_dir("missing");
        let provider = ServerTokenProvider::new(reqwest::Client::new(), dir, "srv");
        assert!(matches!(
            provider.get_token().await,
            Err(McpError::AuthRequired)
        ));
    }
}
