//! Integration tests for the OAuth connect-path wiring (plans/07 T8).
//!
//! A minimal in-test HTTP server plays both roles on one loopback port:
//! the MCP server (JSON-RPC POSTs — 401 with an RFC 9728 challenge without
//! a valid bearer token, full handshake with one) and the token endpoint
//! (the refresh grant). The recorded requests drive the assertions: what
//! authorization header each request actually carried.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mirri_mcp::oauth::TokenStore;
use mirri_mcp::{McpConnectionManager, McpConnectionState, McpServerConfig, TokenSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_oauth_wiring_{label}_{}_{}",
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

// ---------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // the recorded fields exist for debugging assertions
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Clone)]
struct MockMcpServer {
    url: String,
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// Bearer values the MCP endpoint accepts (the refresh test grows this).
    valid_tokens: Arc<Mutex<Vec<String>>>,
    /// Remaining leading requests to fail with HTTP 500.
    fail_leading: Arc<AtomicUsize>,
}

impl MockMcpServer {
    async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");
        let server = Self {
            url: format!("{base_url}/mcp"),
            base_url,
            requests: Arc::new(Mutex::new(Vec::new())),
            valid_tokens: Arc::new(Mutex::new(vec!["good-token".to_string()])),
            fail_leading: Arc::new(AtomicUsize::new(0)),
        };
        let requests = Arc::clone(&server.requests);
        let valid_tokens = Arc::clone(&server.valid_tokens);
        let fail_leading = Arc::clone(&server.fail_leading);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let requests = Arc::clone(&requests);
                let valid_tokens = Arc::clone(&valid_tokens);
                let fail_leading = Arc::clone(&fail_leading);
                tokio::spawn(async move {
                    if let Err(error) =
                        serve_connection(&mut stream, requests, valid_tokens, fail_leading).await
                    {
                        eprintln!("mock server connection error: {error}");
                    }
                });
            }
        });
        server
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn mcp_requests(&self) -> Vec<RecordedRequest> {
        self.recorded()
            .into_iter()
            .filter(|request| request.path == "/mcp")
            .collect()
    }
}

async fn serve_connection(
    stream: &mut TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    valid_tokens: Arc<Mutex<Vec<String>>>,
    fail_leading: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let authorization = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());

    requests.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization: authorization.clone(),
        body: body.clone(),
    });

    let response = if path == "/token" {
        // The refresh grant: parse the form, hand out a rotated token.
        let form = parse_form(&body);
        if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
            http_json(
                "200 OK",
                &serde_json::json!({
                    "access_token": "refreshed-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "refresh-token-2",
                }),
            )
        } else {
            http_json(
                "400 Bad Request",
                &serde_json::json!({"error": "unsupported_grant_type"}),
            )
        }
    } else if path == "/mcp" {
        if fail_leading
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                if value > 0 { Some(value - 1) } else { None }
            })
            .is_ok()
        {
            return write_response(stream, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        }
        let authorized = authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|token| {
                valid_tokens
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|valid| valid == token)
            })
            .unwrap_or(false);
        if !authorized {
            return write_response(
                stream,
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: Bearer resource_metadata=\"/.well-known/oauth-protected-resource\"\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        }
        json_rpc_response(&body)
    } else {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    };
    write_response(stream, &response).await
}

/// Answer the JSON-RPC request body: `initialize` and `tools/list` get
/// real results, requests with no `id` are notifications (no body).
fn json_rpc_response(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let method = request["method"].as_str().unwrap_or_default();
    let id = request.get("id").cloned();
    let Some(id) = id.filter(|value| !value.is_null()) else {
        return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
    };
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "oauth-mock", "version": "1.0"}
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "echo tool",
                "inputSchema": {"type": "object", "properties": {}}
            }]
        }),
        _ => serde_json::json!({}),
    };
    http_json(
        "200 OK",
        &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

// ---------------------------------------------------------------------------
// Minimal HTTP plumbing (the oauth_tests pattern)
// ---------------------------------------------------------------------------

async fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
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
    let already = buffer.len().saturating_sub(header_end);
    let usable = already.min(content_length);
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

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn http_json(status_line: &str, value: &serde_json::Value) -> String {
    let body = value.to_string();
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn write_response(stream: &mut TcpStream, response: &str) -> std::io::Result<()> {
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn oauth_http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
        "auth": {"type": "oauth", "scopes": ["read"]},
    });
    serde_json::from_value(raw).expect("the config should parse")
}

fn plain_http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
    });
    serde_json::from_value(raw).expect("the config should parse")
}

fn oauth_manager(dir: &std::path::Path) -> Arc<McpConnectionManager> {
    Arc::new(McpConnectionManager::with_oauth_token_dir(
        dir.to_path_buf(),
    ))
}

/// Poll the manager until the named server reaches `want` (or the deadline
/// panics).
async fn wait_for_state(
    manager: &McpConnectionManager,
    name: &str,
    want: fn(&McpConnectionState) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = manager
            .list_connections()
            .await
            .into_iter()
            .find(|(conn, _)| conn == name)
            .map(|(_, state)| state);
        if let Some(state) = &state
            && want(state)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("connection '{name}' never reached {what} (last: {state:?})");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn is_needs_auth(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::NeedsAuth)
}

fn is_connected(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Connected { .. })
}

fn is_failed(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Failed { .. })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Given an OAuth-configured remote server with no cached tokens, when the
/// manager connects, then the initial request carries no bearer, the
/// server's real 401 is observed, and the connection lands in `needs-auth`.
#[tokio::test]
async fn should_land_in_needs_auth_when_an_oauth_server_answers_401_without_tokens() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("needs-auth");
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect_err("the 401 connect must fail");
    wait_for_state(&manager, "srv", is_needs_auth, "needs-auth").await;

    // The 401 was real: the request went out without an Authorization
    // header and the server challenged it.
    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty(),
        "the connect must reach the server"
    );
    assert!(
        mcp_requests
            .iter()
            .all(|request| request.authorization.is_none()),
        "no bearer is attached without cached tokens, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given the same 401 answer with no OAuth configured, when the manager
/// connects, then the connection fails plainly (needs-auth is the OAuth
/// flow's state, not a generic 401 bucket).
#[tokio::test]
async fn should_fail_plainly_when_a_401_server_has_no_oauth_config() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("plain-401");
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", plain_http_config(&mock.url))
        .await
        .expect_err("the 401 connect must fail");
    wait_for_state(&manager, "srv", is_failed, "failed").await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Given a cached, still-valid token, when the manager connects, then the
/// token is attached to every request and the connection succeeds.
#[tokio::test]
async fn should_attach_the_cached_token_when_one_exists() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("cached");
    TokenStore::new(dir.clone())
        .save(
            "srv",
            &TokenSet {
                access_token: "good-token".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at: Some(now_unix_seconds() + 3600),
                token_type: "Bearer".to_string(),
                scope: Some("read".to_string()),
                client_id: Some("client".to_string()),
                token_endpoint: Some(format!("{}/token", mock.base_url)),
            },
        )
        .unwrap();
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect("the authorized connect must succeed");
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref() == Some("Bearer good-token")),
        "every request must carry the cached bearer, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given a cached but expired token with a refresh token, when the manager
/// connects, then the provider refreshes first (the token endpoint sees the
/// refresh grant) and the requests carry the refreshed bearer.
#[tokio::test]
async fn should_refresh_an_expired_token_before_the_first_request() {
    let mock = MockMcpServer::start().await;
    // The refreshed token must be accepted by the MCP endpoint too.
    mock.valid_tokens
        .lock()
        .unwrap()
        .push("refreshed-token".to_string());
    let dir = unique_temp_dir("refresh");
    TokenStore::new(dir.clone())
        .save(
            "srv",
            &TokenSet {
                access_token: "stale-token".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at: Some(now_unix_seconds() - 60),
                token_type: "Bearer".to_string(),
                scope: Some("read".to_string()),
                client_id: Some("client".to_string()),
                token_endpoint: Some(format!("{}/token", mock.base_url)),
            },
        )
        .unwrap();
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect("the refreshed connect must succeed");
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    // The refresh grant hit the token endpoint...
    let refresh = mock
        .recorded()
        .into_iter()
        .find(|request| request.path == "/token")
        .expect("the expired token must be refreshed before the request");
    let form = parse_form(&refresh.body);
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("refresh")
    );
    // ...and the requests carry the refreshed bearer.
    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref() == Some("Bearer refreshed-token")),
        "every request must carry the refreshed bearer, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given an OAuth-configured stdio server (adjudication A4: stdio never
/// produces a 401), when the connect fails, then the state is an ordinary
/// `failed` — the needs-auth mapping is remote-transport only.
#[tokio::test]
async fn should_fail_plainly_when_a_stdio_server_declares_oauth() {
    let raw = serde_json::json!({
        "transport": {"type": "stdio", "command": "echo", "args": []},
        "auth": {"type": "oauth"},
    });
    let config: McpServerConfig = serde_json::from_value(raw).expect("the config should parse");
    let manager = oauth_manager(&unique_temp_dir("stdio"));

    // "echo" speaks no JSON-RPC: the handshake fails.
    let _ = manager.connect_one("srv", config).await;
    wait_for_state(&manager, "srv", is_failed, "failed").await;
}

/// Given a remote OAuth server with no cached tokens, when a token-file
/// accessor question arises, then the manager's public shape round-trips
/// the configured directory (and `new()` stays OAuth-free).
#[test]
fn should_expose_the_configured_oauth_token_dir() {
    let dir = unique_temp_dir("accessor");
    let manager = McpConnectionManager::with_oauth_token_dir(dir.clone());
    assert_eq!(manager.oauth_token_dir(), Some(dir.as_path()));
    assert_eq!(McpConnectionManager::new().oauth_token_dir(), None);
    let _ = std::fs::remove_dir_all(dir);
}
