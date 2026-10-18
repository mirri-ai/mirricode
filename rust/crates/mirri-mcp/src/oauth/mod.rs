//! OAuth 2.0 Authorization Code flow with PKCE (RFC 7636) for MCP HTTP
//! servers.
//!
//! The flow is driven entirely by metadata discovery, so no per-server
//! configuration beyond scopes is needed:
//!
//! 1. **Discovery** — the MCP server answers HTTP 401 with a
//!    `WWW-Authenticate: Bearer resource_metadata="..."` header (RFC 9728).
//!    The referenced document names the authorization server(s), whose
//!    metadata is fetched from `/.well-known/oauth-authorization-server`
//!    (RFC 8414).
//! 2. **Dynamic client registration** — when no `client_id` is configured,
//!    a public client is registered (RFC 7591) with
//!    `token_endpoint_auth_method: none`.
//! 3. **Authorization** — a PKCE code challenge (`S256`) and a `state`
//!    value are attached to an authorization URL; the user opens it in a
//!    browser, which redirects to a local loopback callback listener.
//! 4. **Token exchange** — the authorization code is exchanged (with the
//!    code verifier) for an access token and an optional refresh token.
//! 5. **Persistence** — the token set is stored per MCP server under
//!    `~/.mirri-code/mcp-tokens/<server-name>.json` and refreshed
//!    transparently when it expires.
//!
//! The [`OAuthClient`] facade ([`OAuthClient::authenticate`],
//! [`OAuthClient::get_valid_token`], [`OAuthClient::refresh`]) is the
//! primary API; [`AuthorizationCodeFlow::begin`] +
//! [`PendingAuthorization::complete`] expose the two-phase variant for
//! callers that need the authorization URL before waiting (e.g. to spawn a
//! browser).

pub(crate) mod callback;
pub mod discovery;
pub mod error;
pub mod flow;
pub mod pkce;
pub mod provider;
pub mod tokens;

pub use discovery::{
    AuthorizationServerMetadata, ProtectedResourceMetadata, authorization_server_metadata_url,
    extract_resource_metadata_url,
};
pub use error::{OAuthError, OAuthResult};
pub use flow::{
    AuthorizationCodeFlow, DEFAULT_CALLBACK_TIMEOUT, OAuthClient, OAuthConfig, PendingAuthorization,
};
pub use pkce::{
    PkcePair, code_challenge_for, generate_code_verifier, generate_pkce_pair, generate_state,
    random_url_safe_string,
};
pub use provider::ServerTokenProvider;
pub use tokens::{TokenSet, TokenStore};
