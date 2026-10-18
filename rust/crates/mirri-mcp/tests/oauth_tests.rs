//! Integration tests for the MCP OAuth 2.0 authorization-code flow.
//!
//! A mock authorization server runs on a random loopback port inside each
//! test. It speaks the minimal protocol the flow needs: the MCP server's 401
//! challenge (RFC 9728), protected-resource metadata, authorization-server
//! metadata (RFC 8414), dynamic client registration (RFC 7591), the
//! authorization redirect, and the token endpoint (RFC 6749).
//!
//! The user's browser is simulated by a background task that fetches the
//! authorization URL (with reqwest's default redirect following) once the
//! flow hands it over.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mirri_mcp::oauth::discovery::{
    authorization_server_metadata_url, extract_resource_metadata_url,
};
use mirri_mcp::oauth::{
    OAuthClient, OAuthConfig, OAuthError, TokenSet, TokenStore, generate_pkce_pair,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_oauth_{label}_{}_{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Simulate the user opening the authorization URL in a browser: fetch it
/// and follow the authorization server's redirect to the callback listener.
fn open_url_in_background(url: &str) {
    let url = url.to_string();
    tokio::spawn(async move {
        let browser = reqwest::Client::new();
        let _ = browser.get(&url).send().await;
    });
}

// ---------------------------------------------------------------------------
// Mock authorization server
// ---------------------------------------------------------------------------

/// Behavior knobs for the mock authorization server.
#[derive(Debug, Clone, Copy, Default)]
struct MockBehavior {
    /// Corrupt the `state` echoed back in the authorization redirect.
    break_state: bool,
    /// Redirect with `error=access_denied` instead of an authorization code.
    deny: bool,
    /// Reject token requests with `{"error": "invalid_grant"}`.
    reject_token_request: bool,
    /// Omit `token_endpoint` from the authorization server metadata.
    omit_token_endpoint: bool,
    /// Omit `authorization_endpoint` from the authorization server metadata.
    omit_authorization_endpoint: bool,
    /// Omit `registration_endpoint` from the authorization server metadata.
    omit_registration_endpoint: bool,
}

/// Requests the mock authorization server received, for assertions.
#[derive(Debug, Clone, Default)]
struct RecordedCalls {
    registrations: Vec<serde_json::Value>,
    authorize_requests: Vec<HashMap<String, String>>,
    token_requests: Vec<HashMap<String, String>>,
}

struct MockAuthServer {
    base_url: String,
    recorded: Arc<Mutex<RecordedCalls>>,
}

impl MockAuthServer {
    async fn start(behavior: MockBehavior) -> (Self, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");
        let recorded = Arc::new(Mutex::new(RecordedCalls::default()));
        let server = Self {
            base_url: base_url.clone(),
            recorded: recorded.clone(),
        };
        let handle = tokio::spawn(async move {
            run_mock_server(listener, base_url, behavior, recorded).await;
        });
        (server, handle)
    }

    fn recorded(&self) -> RecordedCalls {
        self.recorded.lock().unwrap().clone()
    }
}

async fn run_mock_server(
    listener: TcpListener,
    base_url: String,
    behavior: MockBehavior,
    recorded: Arc<Mutex<RecordedCalls>>,
) {
    loop {
        let Ok((mut stream, _peer)) = listener.accept().await else {
            break;
        };
        if let Err(error) =
            handle_mock_connection(&mut stream, &base_url, behavior, &recorded).await
        {
            eprintln!("mock server connection error: {error}");
            break;
        }
    }
}

async fn handle_mock_connection(
    stream: &mut TcpStream,
    base_url: &str,
    behavior: MockBehavior,
    recorded: &Arc<Mutex<RecordedCalls>>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query_str) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    let query = parse_query_params(&query_str);
    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    let response = mock_route(
        base_url,
        behavior,
        recorded,
        &method,
        &path,
        &query,
        &body,
        &content_type,
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[allow(clippy::too_many_arguments)]
fn mock_route(
    base_url: &str,
    behavior: MockBehavior,
    recorded: &Arc<Mutex<RecordedCalls>>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &str,
    content_type: &str,
) -> String {
    match path {
        // The MCP server endpoint: challenge with RFC 9728 discovery info.
        "/" | "/api" => http_response(
            "401 Unauthorized",
            &[(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                ),
            )],
            "",
        ),
        "/.well-known/oauth-protected-resource" => http_response(
            "200 OK",
            &[],
            &serde_json::json!({
                "resource": format!("{base_url}/api"),
                "authorization_servers": [format!("{base_url}/as")],
            })
            .to_string(),
        ),
        "/as/.well-known/oauth-authorization-server" => {
            let mut metadata = serde_json::Map::new();
            metadata.insert("issuer".into(), serde_json::json!(format!("{base_url}/as")));
            if !behavior.omit_authorization_endpoint {
                metadata.insert(
                    "authorization_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/authorize")),
                );
            }
            if !behavior.omit_token_endpoint {
                metadata.insert(
                    "token_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/token")),
                );
            }
            if !behavior.omit_registration_endpoint {
                metadata.insert(
                    "registration_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/register")),
                );
            }
            metadata.insert(
                "scopes_supported".into(),
                serde_json::json!(["read", "write"]),
            );
            metadata.insert(
                "code_challenge_methods_supported".into(),
                serde_json::json!(["S256"]),
            );
            http_response(
                "200 OK",
                &[],
                &serde_json::Value::Object(metadata).to_string(),
            )
        }
        "/as/register" => {
            let registration: serde_json::Value =
                serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            recorded.lock().unwrap().registrations.push(registration);
            http_response(
                "201 Created",
                &[],
                &serde_json::json!({
                    "client_id": "test-client",
                    "client_secret": null,
                    "client_id_issued_at": now_unix_seconds(),
                })
                .to_string(),
            )
        }
        "/as/authorize" => {
            require_query_params(
                query,
                &[
                    "response_type",
                    "client_id",
                    "redirect_uri",
                    "state",
                    "code_challenge",
                    "code_challenge_method",
                ],
            );
            let redirect_uri = query.get("redirect_uri").cloned().unwrap_or_default();
            let state = query.get("state").cloned().unwrap_or_default();
            recorded
                .lock()
                .unwrap()
                .authorize_requests
                .push(query.clone());
            if behavior.deny {
                let location = format!(
                    "{redirect_uri}?error=access_denied&error_description=mock%20denial&state={state}"
                );
                return http_response("302 Found", &[("Location", location)], "");
            }
            let echoed_state = if behavior.break_state {
                "evil-state"
            } else {
                &state
            };
            let location = format!("{redirect_uri}?code=mock-auth-code&state={echoed_state}");
            http_response("302 Found", &[("Location", location)], "")
        }
        "/as/token" if method == "POST" && content_type.contains("form-urlencoded") => {
            let form = parse_query_params(body);
            let grant_type = form.get("grant_type").cloned().unwrap_or_default();
            recorded.lock().unwrap().token_requests.push(form);
            if behavior.reject_token_request {
                return http_response(
                    "400 Bad Request",
                    &[],
                    &serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "mock rejection",
                    })
                    .to_string(),
                );
            }
            match grant_type.as_str() {
                "authorization_code" => http_response(
                    "200 OK",
                    &[],
                    &serde_json::json!({
                        "access_token": "mock-access-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "mock-refresh-token",
                        "scope": "read write",
                    })
                    .to_string(),
                ),
                "refresh_token" => http_response(
                    "200 OK",
                    &[],
                    &serde_json::json!({
                        "access_token": "mock-refreshed-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "mock-refresh-token-2",
                        "scope": "read write",
                    })
                    .to_string(),
                ),
                other => http_response(
                    "400 Bad Request",
                    &[],
                    &serde_json::json!({
                        "error": "unsupported_grant_type",
                        "error_description": format!("no mock support for {other}"),
                    })
                    .to_string(),
                ),
            }
        }
        _ => http_response("404 Not Found", &[], "no such route"),
    }
}

/// Assert the given query params are present, panicking with a clear message.
fn require_query_params(query: &HashMap<String, String>, required: &[&str]) {
    for param in required {
        assert!(
            query.contains_key(*param),
            "mock /as/authorize expected query param {param:?}, got: {query:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP plumbing for the mock server
// ---------------------------------------------------------------------------

/// Read the request line, headers, and (per Content-Length) body.
async fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before request completed",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    // Body data may have already been read into `buffer` along with the headers
    // (common on localhost where the entire request arrives in one TCP segment).
    // Use that data first, then read any remainder from the stream.
    let body_already_in_buffer = buffer.len().saturating_sub(header_end);
    let usable = body_already_in_buffer.min(content_length);
    let mut body = String::from_utf8_lossy(&buffer[header_end..header_end + usable]).to_string();
    let mut remaining = content_length.saturating_sub(body.len());
    while remaining > 0 {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.push_str(&String::from_utf8_lossy(&chunk[..read]));
        remaining = remaining.saturating_sub(read);
    }
    Ok((request_line, headers, body))
}

/// Parse `application/x-www-form-urlencoded` style query parameters.
fn parse_query_params(query_str: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query_str.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                    continue;
                }
                decoded.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn http_response(status_line: &str, extra_headers: &[(&str, String)], body: &str) -> String {
    let mut head = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    format!("{head}\r\n{body}")
}

// ---------------------------------------------------------------------------
// Helper: run a flow to completion against the mock server
// ---------------------------------------------------------------------------

fn default_config(client_id: Option<&str>) -> OAuthConfig {
    OAuthConfig {
        client_id: client_id.map(str::to_string),
        scopes: vec!["read".to_string(), "write".to_string()],
        redirect_port: None,
    }
}

/// Run the full flow and return the persisted token set plus the recorded
/// mock calls.
async fn run_full_flow(
    server: &MockAuthServer,
    storage_dir: PathBuf,
    config: &OAuthConfig,
) -> (TokenSet, RecordedCalls) {
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let tokens = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            config,
            open_url_in_background,
        ),
    )
    .await
    .expect("oauth flow timed out")
    .expect("oauth flow failed");
    (tokens, server.recorded())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_complete_the_authorization_code_flow_when_using_dynamic_registration() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("full_flow");
    let (tokens, recorded) =
        run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // The exchanged token set.
    assert_eq!(tokens.access_token, "mock-access-token");
    assert_eq!(tokens.token_type, "Bearer");
    assert_eq!(tokens.refresh_token.as_deref(), Some("mock-refresh-token"));
    assert_eq!(tokens.scope.as_deref(), Some("read write"));
    let expires_at = tokens.expires_at.expect("expires_at should be set");
    assert!(
        expires_at > now_unix_seconds() && expires_at <= now_unix_seconds() + 3600,
        "expires_at should be now + expires_in"
    );
    assert_eq!(tokens.client_id.as_deref(), Some("test-client"));
    let expected_token_endpoint = format!("{}/as/token", server.base_url);
    assert_eq!(
        tokens.token_endpoint.as_deref(),
        Some(expected_token_endpoint.as_str())
    );

    // Dynamic client registration body.
    let registrations = &recorded.registrations;
    assert_eq!(registrations.len(), 1);
    let registration = &registrations[0];
    assert_eq!(registration["client_name"], "mirri-code");
    assert_eq!(
        registration["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(registration["token_endpoint_auth_method"], "none");
    assert_eq!(registration["scope"], "read write");
    let registered_redirect_uri = registration["redirect_uris"][0].as_str().unwrap();
    assert!(registered_redirect_uri.ends_with("/callback"));
    assert!(registered_redirect_uri.starts_with("http://localhost:"));

    // The authorization request.
    let authorize = &recorded.authorize_requests[0];
    assert_eq!(
        authorize.get("response_type").map(String::as_str),
        Some("code")
    );
    assert_eq!(
        authorize.get("client_id").map(String::as_str),
        Some("test-client")
    );
    assert_eq!(
        authorize.get("redirect_uri").map(String::as_str),
        Some(registered_redirect_uri)
    );
    assert_eq!(
        authorize.get("scope").map(String::as_str),
        Some("read write")
    );
    assert_eq!(
        authorize.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(authorize.get("code_challenge").map(String::len), Some(43));
    assert_eq!(authorize.get("state").map(String::len), Some(43));

    // The token exchange.
    let exchange = &recorded.token_requests[0];
    assert_eq!(
        exchange.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        exchange.get("client_id").map(String::as_str),
        Some("test-client")
    );
    assert_eq!(
        exchange.get("code").map(String::as_str),
        Some("mock-auth-code")
    );
    assert_eq!(exchange.get("code_verifier").map(String::len), Some(43));
    assert_eq!(
        exchange.get("redirect_uri").map(String::as_str),
        Some(registered_redirect_uri)
    );

    // The token set is persisted and reloads.
    let store = TokenStore::new(storage_dir);
    assert_eq!(store.load("mock-server"), Some(tokens));
}

#[tokio::test]
async fn should_use_configured_client_id_and_skip_registration_when_client_id_is_pre_configured() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("configured_client");
    let config = default_config(Some("pre-registered-client"));

    let (tokens, recorded) = run_full_flow(&server, storage_dir, &config).await;

    assert!(
        recorded.registrations.is_empty(),
        "no DCR when client_id is configured"
    );
    assert_eq!(tokens.client_id.as_deref(), Some("pre-registered-client"));
    assert_eq!(
        recorded.authorize_requests[0]
            .get("client_id")
            .map(String::as_str),
        Some("pre-registered-client")
    );
}

#[tokio::test]
async fn should_return_cached_token_without_network_when_cached_token_is_valid() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("cached_token");
    run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // A fresh client with the same storage dir must reuse the persisted token.
    let initial_request_count = server.recorded().token_requests.len();
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let token = client
        .get_valid_token("mock-server")
        .await
        .expect("cached token should be returned");
    assert_eq!(token, "mock-access-token");
    let after = server.recorded();
    assert_eq!(
        after.token_requests.len(),
        initial_request_count,
        "no new token request should be issued for a valid cached token"
    );
}

#[tokio::test]
async fn should_refresh_expired_access_token_when_refresh_token_is_available() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("refresh");
    run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // Expire the cached tokens, then ask for a valid token.
    let store = TokenStore::new(storage_dir.clone());
    let mut cached = store.load("mock-server").expect("cached tokens exist");
    cached.expires_at = Some(now_unix_seconds() - 60);
    store.save("mock-server", &cached).unwrap();

    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let token = client
        .get_valid_token("mock-server")
        .await
        .expect("refresh should succeed");
    assert_eq!(token, "mock-refreshed-token");

    let recorded = server.recorded();
    // run_full_flow already issued 1 token request (authorization_code exchange);
    // the refresh adds a second.
    assert_eq!(recorded.token_requests.len(), 2);
    let refresh_request = &recorded.token_requests[1];
    assert_eq!(
        refresh_request.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        refresh_request.get("refresh_token").map(String::as_str),
        Some("mock-refresh-token")
    );
    assert_eq!(
        refresh_request.get("client_id").map(String::as_str),
        Some("test-client")
    );

    // The refreshed token set is persisted with the rotated refresh token.
    let refreshed = store
        .load("mock-server")
        .expect("refreshed tokens persisted");
    assert_eq!(refreshed.access_token, "mock-refreshed-token");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("mock-refresh-token-2")
    );
    assert!(refreshed.expires_at.unwrap() > now_unix_seconds());
    assert_eq!(refreshed.client_id.as_deref(), Some("test-client"));
}

#[tokio::test]
async fn should_error_when_no_tokens_are_cached() {
    let (_server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("no_tokens");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = client.get_valid_token("never-authenticated").await;
    assert!(matches!(result, Err(OAuthError::NoCachedTokens(_))));

    let result = client.refresh("never-authenticated").await;
    assert!(matches!(result, Err(OAuthError::NoCachedTokens(_))));
}

#[tokio::test]
async fn should_error_when_cached_tokens_are_expired_without_a_refresh_token() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("no_refresh_token");
    let store = TokenStore::new(storage_dir.clone());
    store
        .save(
            "mock-server",
            &TokenSet {
                access_token: "stale".to_string(),
                refresh_token: None,
                expires_at: Some(now_unix_seconds() - 60),
                token_type: "Bearer".to_string(),
                scope: None,
                client_id: Some("test-client".to_string()),
                token_endpoint: Some(format!("{}/as/token", server.base_url)),
            },
        )
        .unwrap();

    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let result = client.get_valid_token("mock-server").await;
    assert!(matches!(result, Err(OAuthError::NoRefreshToken(_))));
}

#[tokio::test]
async fn should_error_when_the_callback_state_does_not_match() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        break_state: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("state_mismatch");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir.clone());

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    assert!(matches!(result, Err(OAuthError::StateMismatch)));
    assert!(
        TokenStore::new(storage_dir).load("mock-server").is_none(),
        "no tokens should be persisted after a failed flow"
    );
}

#[tokio::test]
async fn should_error_when_the_user_denies_authorization() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        deny: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("denied");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir.clone());

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    match result {
        Err(OAuthError::UserDenied(description)) => {
            assert_eq!(description, "mock denial");
        }
        other => panic!("expected UserDenied, got {other:?}"),
    }
    assert!(TokenStore::new(storage_dir).load("mock-server").is_none());
}

#[tokio::test]
async fn should_error_when_the_token_exchange_is_rejected() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        reject_token_request: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("rejected_exchange");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("token exchange should fail");
    assert!(matches!(error, OAuthError::TokenExchange(_)));
    assert!(
        error.to_string().contains("invalid_grant"),
        "expected the OAuth error code in the message, got: {error}"
    );
}

#[tokio::test]
async fn should_error_when_a_client_id_is_needed_but_registration_is_unavailable() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_registration_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_registration");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    assert!(matches!(result, Err(OAuthError::NoRegistrationEndpoint)));
}

#[tokio::test]
async fn should_error_when_the_authorization_endpoint_is_missing() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_authorization_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_authorize");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("flow should fail without an authorization endpoint");
    assert!(matches!(error, OAuthError::InvalidResponse(_)));
    assert!(error.to_string().contains("authorization_endpoint"));
}

#[tokio::test]
async fn should_error_when_the_token_endpoint_is_missing() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_token_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_token_endpoint");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("flow should fail without a token endpoint");
    assert!(matches!(error, OAuthError::InvalidResponse(_)));
    assert!(error.to_string().contains("token_endpoint"));
}

#[test]
fn should_extract_resource_metadata_url_when_parsing_www_authenticate_header() {
    let header =
        "Bearer resource_metadata=\"https://example.com/.well-known/oauth-protected-resource\"";
    assert_eq!(
        extract_resource_metadata_url(header).as_deref(),
        Some("https://example.com/.well-known/oauth-protected-resource")
    );
    assert_eq!(extract_resource_metadata_url("Bearer scope=\"read\""), None);
}

#[test]
fn should_build_well_known_metadata_url_when_constructing_rfc8414_url() {
    assert_eq!(
        authorization_server_metadata_url("https://auth.example.com").unwrap(),
        "https://auth.example.com/.well-known/oauth-authorization-server"
    );
    assert_eq!(
        authorization_server_metadata_url("https://auth.example.com/issuer").unwrap(),
        "https://auth.example.com/issuer/.well-known/oauth-authorization-server"
    );
}

#[test]
fn should_generate_pkce_material_when_creating_rfc7636_pair() {
    let pair = generate_pkce_pair().unwrap();
    assert_eq!(pair.method, "S256");
    assert_eq!(pair.code_verifier.len(), 43);
    assert_eq!(pair.code_challenge.len(), 43);
    assert_ne!(pair.code_challenge, pair.code_verifier);
}

#[test]
fn should_persist_and_reload_token_sets_when_saving_to_and_loading_from_disk() {
    let storage_dir = unique_temp_dir("roundtrip");
    let store = TokenStore::new(storage_dir.clone());
    let tokens = TokenSet {
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        expires_at: Some(now_unix_seconds() + 3600),
        token_type: "Bearer".to_string(),
        scope: Some("read".to_string()),
        client_id: Some("client".to_string()),
        token_endpoint: Some("https://auth.example.com/token".to_string()),
    };
    store.save("example-server", &tokens).unwrap();
    assert_eq!(store.load("example-server"), Some(tokens));
    assert_eq!(
        store.path_for("example-server"),
        storage_dir.join("example-server.json")
    );
    let _ = std::fs::remove_dir_all(storage_dir);
}
