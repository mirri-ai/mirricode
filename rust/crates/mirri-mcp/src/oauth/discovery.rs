//! OAuth metadata discovery: RFC 9728 (protected resource metadata) and
//! RFC 8414 (authorization server metadata).
//!
//! When an MCP HTTP server responds `401 Unauthorized` with a
//! `WWW-Authenticate: Bearer resource_metadata="..."` header, the URL points
//! at the protected resource metadata document. That document names the
//! authorization server(s), and each authorization server's metadata is
//! fetched from its `/.well-known/oauth-authorization-server` URI.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::oauth::error::{OAuthError, OAuthResult};

/// RFC 9728 Protected Resource Metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// Identifier of the protected resource — the MCP server URL.
    #[serde(default)]
    pub resource: Option<String>,
    /// Authorization servers that can authorize access to the resource.
    /// When empty, the resource itself acts as the authorization server.
    #[serde(rename = "authorization_servers", default)]
    pub authorization_servers: Vec<String>,
}

/// RFC 8414 Authorization Server Metadata document.
///
/// Every field except `issuer` is optional so that discovery tolerates
/// metadata documents for servers that only support some flows; the flow
/// validates the fields it actually needs with precise errors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    /// The issuer identifier, used to locate the well-known metadata URI.
    #[serde(default)]
    pub issuer: Option<String>,
    /// URL of the authorization endpoint.
    #[serde(rename = "authorization_endpoint", default)]
    pub authorization_endpoint: Option<String>,
    /// URL of the token endpoint.
    #[serde(rename = "token_endpoint", default)]
    pub token_endpoint: Option<String>,
    /// URL of the dynamic client registration endpoint (RFC 7591).
    #[serde(rename = "registration_endpoint", default)]
    pub registration_endpoint: Option<String>,
    /// URL of the token revocation endpoint.
    #[serde(rename = "revocation_endpoint", default)]
    pub revocation_endpoint: Option<String>,
    /// OAuth scopes the server supports.
    #[serde(rename = "scopes_supported", default)]
    pub scopes_supported: Vec<String>,
    /// PKCE code challenge methods the server supports.
    #[serde(rename = "code_challenge_methods_supported", default)]
    pub code_challenge_methods_supported: Vec<String>,
}

/// Parse the auth parameters of a `WWW-Authenticate` header value.
///
/// The header shape is `Scheme key="value", key2=token`; parameter names are
/// lowercased (they are case-insensitive per RFC 7235) and quoted values are
/// unescaped.
pub(crate) fn parse_www_authenticate(header_value: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    // Everything after the auth scheme (e.g. `Bearer`).
    let Some(params_start) = header_value.find(char::is_whitespace) else {
        return params;
    };
    let mut chars = header_value[params_start..].char_indices().peekable();
    loop {
        // Skip inter-parameter whitespace and commas.
        while let Some(&(_, next)) = chars.peek() {
            if next.is_whitespace() || next == ',' {
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        // Key: everything up to '=' (or ',' for a malformed parameter).
        let mut key = String::new();
        while let Some(&(_, next)) = chars.peek() {
            if next == '=' || next == ',' {
                break;
            }
            key.push(next);
            chars.next();
        }
        // A well-formed parameter continues with '='; otherwise skip it
        // (the top-of-loop skip consumes the trailing ',' or whitespace).
        if chars.peek().map(|&(_, next)| next) != Some('=') {
            continue;
        }
        chars.next(); // consume '='
        // Skip whitespace before the value.
        while let Some(&(_, next)) = chars.peek() {
            if next.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        let mut value = String::new();
        match chars.peek().map(|&(_, next)| next) {
            Some('"') => {
                chars.next(); // consume opening quote
                let mut escaped = false;
                for (_, value_ch) in chars.by_ref() {
                    if escaped {
                        value.push(value_ch);
                        escaped = false;
                    } else if value_ch == '\\' {
                        escaped = true;
                    } else if value_ch == '"' {
                        break;
                    } else {
                        value.push(value_ch);
                    }
                }
            }
            Some(_) => {
                while let Some(&(_, next)) = chars.peek() {
                    if next == ',' {
                        break;
                    }
                    value.push(next);
                    chars.next();
                }
                value = value.trim().to_string();
            }
            None => {}
        }
        params.push((key.to_ascii_lowercase(), value));
    }
    params
}

/// Extract the `resource_metadata` URI from a `WWW-Authenticate` header
/// value (RFC 9728 §3):
///
/// ```text
/// Bearer resource_metadata="https://example.com/.well-known/oauth-protected-resource"
/// ```
pub fn extract_resource_metadata_url(header_value: &str) -> Option<String> {
    parse_www_authenticate(header_value)
        .into_iter()
        .find(|(name, _)| name == "resource_metadata")
        .map(|(_, value)| value)
}

/// Build the RFC 8414 well-known metadata URI for an authorization server
/// issuer: `{issuer}/.well-known/oauth-authorization-server`.
pub fn authorization_server_metadata_url(issuer: &str) -> OAuthResult<String> {
    let mut url = Url::parse(issuer).map_err(|error| {
        OAuthError::Discovery(format!(
            "invalid authorization server issuer {issuer:?}: {error}"
        ))
    })?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(OAuthError::Discovery(format!(
            "authorization server issuer must not contain a query or fragment: {issuer:?}"
        )));
    }
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url.join(".well-known/oauth-authorization-server")
        .map(|joined| joined.to_string())
        .map_err(|error| {
            OAuthError::Discovery(format!(
                "failed to build well-known metadata URI for issuer {issuer:?}: {error}"
            ))
        })
}

/// Fetch and parse the RFC 9728 protected resource metadata document.
pub async fn fetch_protected_resource_metadata(
    http: &reqwest::Client,
    resource_metadata_url: &str,
) -> OAuthResult<ProtectedResourceMetadata> {
    let response = http.get(resource_metadata_url).send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OAuthError::Discovery(format!(
            "failed to fetch protected resource metadata from {resource_metadata_url}: HTTP {status}: {text}"
        )));
    }
    serde_json::from_str(&text).map_err(|error| {
        OAuthError::Discovery(format!(
            "invalid protected resource metadata from {resource_metadata_url}: {error}: {text}"
        ))
    })
}

/// Fetch and parse the RFC 8414 authorization server metadata document from
/// its well-known URI.
pub async fn fetch_authorization_server_metadata(
    http: &reqwest::Client,
    issuer: &str,
) -> OAuthResult<AuthorizationServerMetadata> {
    let metadata_url = authorization_server_metadata_url(issuer)?;
    let response = http.get(&metadata_url).send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(OAuthError::Discovery(format!(
            "failed to fetch authorization server metadata from {metadata_url}: HTTP {status}: {text}"
        )));
    }
    serde_json::from_str(&text).map_err(|error| {
        OAuthError::Discovery(format!(
            "invalid authorization server metadata from {metadata_url}: {error}: {text}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_extract_resource_metadata_url_from_a_bearer_challenge_when_parsing_www_authenticate_header()
     {
        let header = "Bearer resource_metadata=\"https://example.com/.well-known/oauth-protected-resource\", scope=\"read\"";
        assert_eq!(
            extract_resource_metadata_url(header).as_deref(),
            Some("https://example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn should_extract_unescaped_quoted_values_when_parsing_quoted_header_value() {
        let header = r#"Bearer resource_metadata="https://example.com/a\"b""#;
        assert_eq!(
            extract_resource_metadata_url(header).as_deref(),
            Some("https://example.com/a\"b")
        );
    }

    #[test]
    fn should_return_none_when_metadata_is_absent() {
        assert_eq!(
            extract_resource_metadata_url("Basic realm=\"example\""),
            None
        );
        assert_eq!(extract_resource_metadata_url("Bearer"), None);
        assert_eq!(extract_resource_metadata_url(""), None);
    }

    #[test]
    fn should_ignore_parameter_name_case_when_parsing_header_parameters() {
        let header = "Bearer RESOURCE_METADATA=\"https://example.com/meta\"";
        assert_eq!(
            extract_resource_metadata_url(header).as_deref(),
            Some("https://example.com/meta")
        );
    }

    #[test]
    fn should_build_the_rfc8414_well_known_url_from_an_issuer_when_constructing_metadata_url() {
        assert_eq!(
            authorization_server_metadata_url("https://auth.example.com").unwrap(),
            "https://auth.example.com/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            authorization_server_metadata_url("https://auth.example.com/as").unwrap(),
            "https://auth.example.com/as/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            authorization_server_metadata_url("http://127.0.0.1:8080/as").unwrap(),
            "http://127.0.0.1:8080/as/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn should_reject_issuers_with_query_or_fragment_when_building_metadata_url() {
        assert!(authorization_server_metadata_url("https://auth.example.com?x=1").is_err());
        assert!(authorization_server_metadata_url("https://auth.example.com#frag").is_err());
    }

    #[test]
    fn should_parse_rfc9728_protected_resource_metadata_when_deserializing_json() {
        let json = r#"{
            "resource": "https://example.com/api",
            "authorization_servers": ["https://auth.example.com"],
            "unknown_field": "ignored"
        }"#;
        let parsed: ProtectedResourceMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.resource.as_deref(), Some("https://example.com/api"));
        assert_eq!(
            parsed.authorization_servers,
            vec!["https://auth.example.com"]
        );
    }

    #[test]
    fn should_parse_rfc8414_authorization_server_metadata_when_deserializing_json() {
        let json = r#"{
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "registration_endpoint": "https://auth.example.com/register",
            "revocation_endpoint": "https://auth.example.com/revoke",
            "scopes_supported": ["read", "write"],
            "code_challenge_methods_supported": ["S256"]
        }"#;
        let parsed: AuthorizationServerMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.issuer.as_deref(), Some("https://auth.example.com"));
        assert_eq!(
            parsed.authorization_endpoint.as_deref(),
            Some("https://auth.example.com/authorize")
        );
        assert_eq!(parsed.scopes_supported, vec!["read", "write"]);
        assert_eq!(parsed.code_challenge_methods_supported, vec!["S256"]);
        assert!(
            parsed
                .code_challenge_methods_supported
                .iter()
                .any(|method| method == "S256")
        );
    }
}
