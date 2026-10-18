//! The OAuth 2.0 authorization-code flow with PKCE.
//!
//! [`OAuthClient`] is the main entry point: it persists and loads
//! [`TokenSet`]s per MCP server and runs the full flow on demand.
//!
//! [`AuthorizationCodeFlow`] drives the protocol itself: discovery (RFC
//! 9728 + RFC 8414), dynamic client registration (RFC 7591) when no
//! `client_id` is configured, the local callback listener, and the token
//! exchange. Callers that must open the authorization URL themselves (the
//! TUI/API layer) can use [`AuthorizationCodeFlow::begin`] +
//! [`PendingAuthorization::complete`] instead of the convenience
//! [`OAuthClient::authenticate`].

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use url::Url;

use crate::oauth::callback::CallbackServer;
use crate::oauth::discovery::{
    AuthorizationServerMetadata, extract_resource_metadata_url,
    fetch_authorization_server_metadata, fetch_protected_resource_metadata,
};
use crate::oauth::error::{OAuthError, OAuthResult};
use crate::oauth::pkce::{PkcePair, generate_pkce_pair, generate_state};
use crate::oauth::tokens::{TokenSet, TokenStore, current_unix_time_seconds};

/// How long the flow waits for the user to complete authorization in the
/// browser before giving up.
pub const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Configuration for an OAuth authorization-code flow.
#[derive(Debug, Clone, Default)]
pub struct OAuthConfig {
    /// Pre-registered OAuth client id. `None` triggers dynamic client
    /// registration against the authorization server.
    pub client_id: Option<String>,
    /// Scopes to request. May be empty.
    pub scopes: Vec<String>,
    /// Port for the local callback listener. `None` picks a random free port.
    pub redirect_port: Option<u16>,
}

/// The main entry point for MCP OAuth: token persistence + full flow.
pub struct OAuthClient {
    http: reqwest::Client,
    token_store: TokenStore,
    /// The server name for the current session — set by `authenticate`
    /// so the `TokenProvider` impl knows which token file to use.
    current_server: std::sync::Mutex<Option<String>>,
}

impl OAuthClient {
    /// Create a client that persists tokens under `storage_dir`
    /// (one `{server-name}.json` file per MCP server).
    pub fn new(http: reqwest::Client, storage_dir: PathBuf) -> Self {
        Self {
            http,
            token_store: TokenStore::new(storage_dir),
            current_server: std::sync::Mutex::new(None),
        }
    }

    /// The backing token store (useful for tests and inspection).
    pub fn token_store(&self) -> &TokenStore {
        &self.token_store
    }

    /// Try to load the cached token set for a server.
    pub fn load_tokens(&self, server_name: &str) -> Option<TokenSet> {
        self.token_store.load(server_name)
    }

    /// Run the full OAuth flow (discover → register → authorize → exchange)
    /// and persist the resulting token set for `server_name`.
    ///
    /// The authorization URL is logged; use
    /// [`Self::authenticate_with_url_handler`] to receive it programmatically
    /// (e.g. to spawn a browser).
    pub async fn authenticate(
        &self,
        server_url: &str,
        server_name: &str,
        config: &OAuthConfig,
    ) -> OAuthResult<TokenSet> {
        self.authenticate_with_url_handler(server_url, server_name, config, |authorization_url| {
            tracing::info!(
                authorization_url,
                "Open this URL in your browser to authorize mirri-code"
            );
        })
        .await
    }

    /// Like [`Self::authenticate`], but hands the authorization URL to
    /// `open_url` before waiting for the callback — the caller (TUI/API
    /// layer) is responsible for opening it.
    pub async fn authenticate_with_url_handler<F>(
        &self,
        server_url: &str,
        server_name: &str,
        config: &OAuthConfig,
        open_url: F,
    ) -> OAuthResult<TokenSet>
    where
        F: FnOnce(&str),
    {
        // Remember the server name so the TokenProvider impl can find tokens.
        {
            let mut current = self
                .current_server
                .lock()
                .expect("current_server lock poisoned");
            *current = Some(server_name.to_string());
        }
        let pending = AuthorizationCodeFlow::new(&self.http, server_url, config)
            .begin()
            .await?;
        open_url(pending.authorization_url());
        let tokens = pending.complete().await?;
        self.token_store.save(server_name, &tokens)?;
        Ok(tokens)
    }

    /// Refresh an expired access token using the stored refresh token.
    ///
    /// The token endpoint and client id are read from the persisted token
    /// set, so no re-discovery is needed. The refreshed token set is
    /// persisted and returned.
    pub async fn refresh(&self, server_name: &str) -> OAuthResult<TokenSet> {
        let cached = self
            .token_store
            .load(server_name)
            .ok_or_else(|| OAuthError::NoCachedTokens(server_name.to_string()))?;
        let refresh_token = cached
            .refresh_token
            .clone()
            .ok_or_else(|| OAuthError::NoRefreshToken(server_name.to_string()))?;
        let token_endpoint = cached
            .token_endpoint
            .clone()
            .ok_or_else(|| OAuthError::IncompleteCachedTokens(server_name.to_string()))?;
        let client_id = cached
            .client_id
            .clone()
            .ok_or_else(|| OAuthError::IncompleteCachedTokens(server_name.to_string()))?;

        let response =
            refresh_tokens(&self.http, &token_endpoint, &client_id, &refresh_token).await?;
        let mut refreshed =
            response.into_token_set(client_id, token_endpoint, cached.scope.clone());
        // If the server did not rotate the refresh token, keep the old one.
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = cached.refresh_token;
        }
        self.token_store.save(server_name, &refreshed)?;
        Ok(refreshed)
    }

    /// Get a valid access token for a server, refreshing it first when the
    /// cached one is expired.
    ///
    /// Errors with [`OAuthError::NoCachedTokens`] when the server has never
    /// been authenticated (run [`Self::authenticate`]) and with
    /// [`OAuthError::NoRefreshToken`] when the cached token is expired and
    /// cannot be refreshed.
    pub async fn get_valid_token(&self, server_name: &str) -> OAuthResult<String> {
        let cached = self
            .token_store
            .load(server_name)
            .ok_or_else(|| OAuthError::NoCachedTokens(server_name.to_string()))?;
        if !cached.is_expired(SystemTime::now()) {
            return Ok(cached.access_token);
        }
        if cached.refresh_token.is_some() {
            return Ok(self.refresh(server_name).await?.access_token);
        }
        Err(OAuthError::NoRefreshToken(server_name.to_string()))
    }
}

/// Orchestrates one authorization-code flow with PKCE.
pub struct AuthorizationCodeFlow<'a> {
    http: &'a reqwest::Client,
    server_url: &'a str,
    config: &'a OAuthConfig,
}

/// The flow state once everything up to the human step is done: discovery,
/// client registration, callback listener, and the built authorization URL.
pub struct PendingAuthorization<'a> {
    http: &'a reqwest::Client,
    client_id: String,
    token_endpoint: String,
    callback: CallbackServer,
    scopes: Vec<String>,
    state: String,
    code_verifier: String,
    authorization_url: String,
}

impl<'a> AuthorizationCodeFlow<'a> {
    /// Create a flow against `server_url` (the MCP server that challenged
    /// with a `resource_metadata` URL).
    pub fn new(http: &'a reqwest::Client, server_url: &'a str, config: &'a OAuthConfig) -> Self {
        Self {
            http,
            server_url,
            config,
        }
    }

    /// Run everything up to the browser step and return the pending flow:
    ///
    /// 1. probe the MCP server for the 401 `WWW-Authenticate` challenge
    /// 2. fetch the RFC 9728 protected resource metadata
    /// 3. fetch the RFC 8414 authorization server metadata
    /// 4. bind the local callback listener
    /// 5. dynamically register a client when no `client_id` is configured
    /// 6. generate PKCE material and build the authorization URL
    pub async fn begin(self) -> OAuthResult<PendingAuthorization<'a>> {
        let resource_metadata_url = probe_server_auth_challenge(self.http, self.server_url).await?;
        let resource_metadata =
            fetch_protected_resource_metadata(self.http, &resource_metadata_url).await?;

        // The first listed authorization server wins; when none are listed
        // the protected resource itself is the authorization server.
        let issuer = resource_metadata
            .authorization_servers
            .first()
            .cloned()
            .or(resource_metadata.resource)
            .ok_or(OAuthError::NoAuthorizationServer)?;
        let as_metadata = fetch_authorization_server_metadata(self.http, &issuer).await?;

        let token_endpoint = as_metadata.token_endpoint.clone().ok_or_else(|| {
            OAuthError::InvalidResponse(
                "authorization server metadata is missing token_endpoint".to_string(),
            )
        })?;

        // Bind the listener before registering the client so the registered
        // redirect URI carries the real port.
        let callback = CallbackServer::bind(self.config.redirect_port).await?;
        let redirect_uri = callback.redirect_uri().to_string();

        let client_id = match &self.config.client_id {
            Some(client_id) => client_id.clone(),
            None => {
                register_client(self.http, &as_metadata, &redirect_uri, &self.config.scopes).await?
            }
        };

        let pkce = generate_pkce_pair()?;
        let state = generate_state()?;
        let authorization_url = build_authorization_url(
            &as_metadata,
            &client_id,
            &redirect_uri,
            &self.config.scopes,
            &state,
            &pkce,
        )?;

        tracing::debug!(
            authorization_url,
            "OAuth authorization flow ready; waiting for the browser callback"
        );

        Ok(PendingAuthorization {
            http: self.http,
            client_id,
            token_endpoint,
            callback,
            scopes: self.config.scopes.clone(),
            state,
            code_verifier: pkce.code_verifier,
            authorization_url,
        })
    }
}

impl PendingAuthorization<'_> {
    /// The URL the user must open in their browser.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// Wait for the browser callback, verify the state, exchange the
    /// authorization code for tokens, and return the token set.
    ///
    /// The caller is expected to have presented [`Self::authorization_url`]
    /// to the user already.
    pub async fn complete(self) -> OAuthResult<TokenSet> {
        let redirect_uri = self.callback.redirect_uri().to_string();
        let expected_state = self.state.clone();
        let code = self
            .callback
            .wait_for_authorization_code(&expected_state, DEFAULT_CALLBACK_TIMEOUT)
            .await?;
        let requested_scope = if self.scopes.is_empty() {
            None
        } else {
            Some(self.scopes.join(" "))
        };
        let response = exchange_authorization_code(
            self.http,
            &self.token_endpoint,
            &self.client_id,
            &redirect_uri,
            &code,
            &self.code_verifier,
        )
        .await?;
        tracing::debug!("OAuth token exchange succeeded");
        Ok(response.into_token_set(self.client_id, self.token_endpoint, requested_scope))
    }
}

/// The response from an RFC 7591 dynamic client registration.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClientRegistrationResponse {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(rename = "client_id_issued_at", default)]
    pub client_id_issued_at: Option<i64>,
}

/// A successful token endpoint response (RFC 6749 §5.1).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Lifetime of the access token in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(rename = "token_type", default)]
    pub token_type: String,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    /// Convert into a persisted [`TokenSet`]. `requested_scope` is used when
    /// the response omits `scope`; expiry is computed from `expires_in` (or
    /// left unknown when the server omits it).
    fn into_token_set(
        self,
        client_id: String,
        token_endpoint: String,
        requested_scope: Option<String>,
    ) -> TokenSet {
        let expires_at = self
            .expires_in
            .map(|seconds| current_unix_time_seconds(SystemTime::now()) + seconds as i64);
        if self.expires_in.is_none() {
            tracing::warn!("token response did not include expires_in; expiry unknown");
        }
        TokenSet {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
            token_type: if self.token_type.is_empty() {
                "Bearer".to_string()
            } else {
                self.token_type
            },
            scope: self.scope.or(requested_scope),
            client_id: Some(client_id),
            token_endpoint: Some(token_endpoint),
        }
    }
}

/// An RFC 6749 §5.2 error response body.
#[derive(Debug, Clone, Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Probe the MCP server for the OAuth challenge: expect HTTP 401/403 with a
/// `WWW-Authenticate` header carrying the RFC 9728 `resource_metadata` URL.
async fn probe_server_auth_challenge(
    http: &reqwest::Client,
    server_url: &str,
) -> OAuthResult<String> {
    let response = http.get(server_url).send().await?;
    let status = response.status();
    if !(status.as_u16() == 401 || status.as_u16() == 403) {
        return Err(OAuthError::NotRequired(status.to_string()));
    }
    for header in response
        .headers()
        .get_all(reqwest::header::WWW_AUTHENTICATE)
    {
        if let Ok(header_value) = header.to_str()
            && let Some(metadata_url) = extract_resource_metadata_url(header_value)
        {
            return Ok(metadata_url);
        }
    }
    Err(OAuthError::MissingChallenge)
}

/// Dynamically register an OAuth client (RFC 7591). The client uses
/// `token_endpoint_auth_method: none` (public client) so no `client_secret`
/// is needed.
async fn register_client(
    http: &reqwest::Client,
    as_metadata: &AuthorizationServerMetadata,
    redirect_uri: &str,
    scopes: &[String],
) -> OAuthResult<String> {
    let registration_endpoint = as_metadata
        .registration_endpoint
        .clone()
        .ok_or(OAuthError::NoRegistrationEndpoint)?;

    let mut body = serde_json::json!({
        "client_name": "mirri-code",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_method": "none",
    });
    if !scopes.is_empty() {
        body["scope"] = serde_json::json!(scopes.join(" "));
    }

    let response = http.post(&registration_endpoint).json(&body).send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OAuthError::Registration(format!("HTTP {status}: {text}")));
    }
    let registration: ClientRegistrationResponse =
        serde_json::from_str(&text).map_err(|error| {
            OAuthError::Registration(format!(
                "invalid registration response (HTTP {status}): {error}: {text}"
            ))
        })?;
    if registration.client_secret.is_some() {
        tracing::warn!(
            "authorization server issued a client_secret despite token_endpoint_auth_method=none; it is not used"
        );
    }
    tracing::debug!(
        client_id = %registration.client_id,
        client_id_issued_at = ?registration.client_id_issued_at,
        "dynamically registered OAuth client"
    );
    Ok(registration.client_id)
}

/// Build the authorization URL for the code flow with PKCE.
fn build_authorization_url(
    as_metadata: &AuthorizationServerMetadata,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    pkce: &PkcePair,
) -> OAuthResult<String> {
    let authorization_endpoint = as_metadata.authorization_endpoint.clone().ok_or_else(|| {
        OAuthError::InvalidResponse(
            "authorization server metadata is missing authorization_endpoint".to_string(),
        )
    })?;
    let mut url = Url::parse(&authorization_endpoint).map_err(|error| {
        OAuthError::Discovery(format!(
            "invalid authorization_endpoint {authorization_endpoint:?}: {error}"
        ))
    })?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect_uri);
        if !scopes.is_empty() {
            query.append_pair("scope", &scopes.join(" "));
        }
        query.append_pair("state", state);
        query.append_pair("code_challenge", &pkce.code_challenge);
        query.append_pair("code_challenge_method", pkce.method);
    }
    Ok(url.to_string())
}

/// Exchange the authorization code for tokens (RFC 6749 §4.1.3 + RFC 7636):
/// the `code_verifier` proves the exchange belongs to this authorization
/// request.
async fn exchange_authorization_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> OAuthResult<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    post_token_request(http, token_endpoint, &params, OAuthError::TokenExchange).await
}

/// Refresh an access token (RFC 6749 §6) — the code verifier is not sent
/// on refresh.
async fn refresh_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> OAuthResult<TokenResponse> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    post_token_request(http, token_endpoint, &params, OAuthError::TokenRefresh).await
}

/// POST form-encoded params to the token endpoint and parse the response,
/// surfacing RFC 6749 §5.2 error bodies when the server rejects the grant.
async fn post_token_request(
    http: &reqwest::Client,
    token_endpoint: &str,
    params: &[(&str, &str)],
    into_error: fn(String) -> OAuthError,
) -> OAuthResult<TokenResponse> {
    let response = http.post(token_endpoint).form(params).send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if let Ok(error_body) = serde_json::from_str::<TokenErrorResponse>(&text) {
            let description = error_body
                .error_description
                .as_deref()
                .unwrap_or(&error_body.error);
            return Err(into_error(format!(
                "HTTP {status}: {}: {description}",
                error_body.error
            )));
        }
        return Err(into_error(format!("HTTP {status}: {text}")));
    }
    serde_json::from_str(&text).map_err(|error| {
        into_error(format!(
            "invalid token response (HTTP {status}): {error}: {text}"
        ))
    })
}

/// Async token access for the `TokenProvider` trait — used by the HTTP
/// transport to fetch a fresh token before each request. The server the
/// tokens belong to is the one the client last authenticated.
impl crate::transport::TokenProvider for OAuthClient {
    fn get_token(&self) -> crate::transport::TransportFuture<'_, String> {
        let server_name = {
            let current = self
                .current_server
                .lock()
                .expect("current_server lock poisoned");
            current.clone().unwrap_or_default()
        };
        Box::pin(async move {
            if server_name.is_empty() {
                return Err(crate::error::McpError::AuthRequired);
            }
            // A cached, still-valid token needs no network; an expired one
            // is refreshed (and the rotation persisted) by get_valid_token.
            self.get_valid_token(&server_name)
                .await
                .map_err(|_| crate::error::McpError::AuthRequired)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn metadata_with_authorization_endpoint(endpoint: Option<&str>) -> AuthorizationServerMetadata {
        AuthorizationServerMetadata {
            authorization_endpoint: endpoint.map(str::to_string),
            ..AuthorizationServerMetadata::default()
        }
    }

    fn sample_pkce() -> PkcePair {
        PkcePair {
            code_verifier: "sample-verifier".to_string(),
            code_challenge: "sample-challenge".to_string(),
            method: "S256",
        }
    }

    #[test]
    fn should_build_an_authorization_url_with_pkce_and_state_when_constructing_authorization_url() {
        let url = build_authorization_url(
            &metadata_with_authorization_endpoint(Some("https://auth.example.com/authorize")),
            "client-1",
            "http://localhost:4321/callback",
            &["read".to_string(), "write".to_string()],
            "state-1",
            &sample_pkce(),
        )
        .unwrap();

        let parsed = Url::parse(&url).unwrap();
        let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("client-1"));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some("http://localhost:4321/callback")
        );
        assert_eq!(pairs.get("scope").map(String::as_str), Some("read write"));
        assert_eq!(pairs.get("state").map(String::as_str), Some("state-1"));
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("sample-challenge")
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
    }

    #[test]
    fn should_omit_the_scope_param_when_no_scopes_are_requested() {
        let url = build_authorization_url(
            &metadata_with_authorization_endpoint(Some("https://auth.example.com/authorize")),
            "client-1",
            "http://localhost:4321/callback",
            &[],
            "state-1",
            &sample_pkce(),
        )
        .unwrap();
        assert!(!url.contains("scope="));
    }

    #[test]
    fn should_error_when_the_authorization_endpoint_is_missing() {
        let result = build_authorization_url(
            &metadata_with_authorization_endpoint(None),
            "client-1",
            "http://localhost:4321/callback",
            &[],
            "state-1",
            &sample_pkce(),
        );
        assert!(matches!(result, Err(OAuthError::InvalidResponse(_))));
    }
}
