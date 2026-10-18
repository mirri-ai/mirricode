//! Errors produced by the OAuth 2.0 authorization-code flow with PKCE.
//!
//! Each variant maps to one distinct failure mode of the flow (discovery,
//! dynamic client registration, the browser callback, token exchange, or
//! token refresh) so that callers can decide how to surface or retry it.

/// Every error that can arise from running an MCP OAuth flow.
///
/// Not `Clone`: the `Http`/`Io` variants now carry the original
/// [`reqwest::Error`] / [`std::io::Error`] (not `Clone`) to preserve source
/// chains via `#[from]`.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The server responded with a success status instead of an HTTP
    /// 401/403 challenge, so OAuth is not required (or the probe hit the
    /// wrong endpoint).
    #[error("MCP server did not require OAuth (expected HTTP 401/403, got {0})")]
    NotRequired(String),

    /// The server challenged with 401/403 but no `WWW-Authenticate` header
    /// carrying a `resource_metadata` URL was present (RFC 9728).
    #[error("MCP server did not advertise an OAuth resource_metadata URL in WWW-Authenticate")]
    MissingChallenge,

    /// Fetching or parsing a metadata document failed (RFC 9728 / RFC 8414).
    #[error("OAuth discovery failed: {0}")]
    Discovery(String),

    /// The protected resource metadata lists no authorization servers and no
    /// resource identifier to fall back on.
    #[error("protected resource metadata has no authorization_servers and no resource identifier")]
    NoAuthorizationServer,

    /// Dynamic client registration is required but the authorization server
    /// does not advertise a `registration_endpoint`.
    #[error(
        "OAuth dynamic client registration required but the authorization server has no registration_endpoint"
    )]
    NoRegistrationEndpoint,

    /// Dynamic client registration (RFC 7591) was rejected or returned an
    /// unusable response.
    #[error("OAuth dynamic client registration failed: {0}")]
    Registration(String),

    /// Cryptographically secure random bytes could not be obtained.
    #[error("failed to generate OAuth PKCE material: {0}")]
    Pkce(String),

    /// The local callback listener could not be bound or failed to accept a
    /// connection.
    #[error("OAuth callback server error: {0}")]
    CallbackServer(String),

    /// The user did not complete authorization within the callback window.
    #[error("OAuth authorization flow timed out waiting for the browser callback")]
    CallbackTimeout,

    /// The authorization server redirected with `error=...` (e.g. the user
    /// denied the request).
    #[error("OAuth authorization was not granted: {0}")]
    UserDenied(String),

    /// The `state` parameter returned in the callback did not match the one
    /// sent in the authorization request.
    #[error("OAuth callback state parameter did not match the authorization request")]
    StateMismatch,

    /// Exchanging the authorization code for tokens failed.
    #[error("OAuth token exchange failed: {0}")]
    TokenExchange(String),

    /// Refreshing the access token failed.
    #[error("OAuth token refresh failed: {0}")]
    TokenRefresh(String),

    /// No cached token set exists for the given server name.
    #[error("no cached OAuth tokens for '{0}'; run the authorization flow first")]
    NoCachedTokens(String),

    /// The cached token set is expired and does not carry a refresh token.
    #[error("cached OAuth tokens for '{0}' are expired and no refresh token is available")]
    NoRefreshToken(String),

    /// The cached token set lacks the client id / token endpoint needed to
    /// refresh; the server must be re-authenticated.
    #[error(
        "cached OAuth tokens for '{0}' are incomplete (missing client id or token endpoint); re-authenticate"
    )]
    IncompleteCachedTokens(String),

    /// The authorization server returned a document that is missing fields
    /// the flow needs to continue.
    #[error("invalid response from authorization server: {0}")]
    InvalidResponse(String),

    /// An HTTP request to the authorization server or MCP server failed.
    ///
    /// Holds the original [`reqwest::Error`] so the source chain survives
    /// `#[from]` conversion.
    #[error("OAuth HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// A local I/O failure (reading or writing the token store, sockets).
    ///
    /// Holds the original [`std::io::Error`] so the source chain survives
    /// `#[from]` conversion.
    #[error("OAuth I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout the OAuth module.
pub type OAuthResult<T> = Result<T, OAuthError>;
