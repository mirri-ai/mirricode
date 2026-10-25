// ---------------------------------------------------------------------------
// plans/07 T8 — MCP OAuth wiring, auto-reconnect, F-T1-3
// ---------------------------------------------------------------------------

use super::*;

/// One recorded request to the OAuth+MCP mock server.
#[derive(Debug, Clone)]
#[allow(dead_code)] // the recorded fields exist for debugging assertions
struct OAuthMockRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

/// A mock server playing both the OAuth-protected MCP server (JSON-RPC
/// POSTs: 401 with an RFC 9728 challenge without a valid bearer, the full
/// handshake with one) and the authorization server (RFC 9728/8414
/// metadata, RFC 7591 registration, the authorize redirect, and the token
/// endpoint) — one loopback port, the same shape the TS side's
/// `agent/mcp/tools/auth.ts` e2e drives.
#[derive(Clone)]
struct MockOAuthMcpServer {
    url: String,
    base_url: String,
    requests: Arc<std::sync::Mutex<Vec<OAuthMockRequest>>>,
}

impl MockOAuthMcpServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the OAuth mock server");
        let port = listener.local_addr().expect("local addr").port();
        let base_url = format!("http://127.0.0.1:{port}");
        let server = Self {
            url: format!("{base_url}/mcp"),
            base_url: base_url.clone(),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let requests = Arc::clone(&server.requests);
        let well_known_base = base_url.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let requests = Arc::clone(&requests);
                let well_known_base = well_known_base.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut head = String::new();
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        if stream.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push_str(&String::from_utf8_lossy(&line));
                        if line == b"\r\n" {
                            break;
                        }
                    }
                    let request_line = head.lines().next().unwrap_or_default().to_string();
                    let authorization = head.lines().find_map(|l| {
                        l.split_once(':').and_then(|(name, value)| {
                            name.trim()
                                .eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_string())
                        })
                    });
                    let content_length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && stream.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let body = String::from_utf8_lossy(&body).to_string();

                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or("/").to_string();
                    let path = target.split('?').next().unwrap_or("/").to_string();
                    let query = target.split_once('?').map(|(_, q)| q.to_string());

                    requests.lock().unwrap().push(OAuthMockRequest {
                        method: method.clone(),
                        path: path.clone(),
                        authorization: authorization.clone(),
                        body: body.clone(),
                    });

                    let response = oauth_mock_route(
                        &well_known_base,
                        &method,
                        &path,
                        query.as_deref(),
                        &authorization,
                        &body,
                    );
                    let _ = stream.get_mut().write_all(response.as_bytes()).await;
                    let _ = stream.get_mut().shutdown().await;
                });
            }
        });
        server
    }

    fn requests_of(&self, path: &str) -> Vec<OAuthMockRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .cloned()
            .collect()
    }
}

/// Route one request of the OAuth+MCP mock: the MCP endpoint enforces the
/// bearer, the OAuth endpoints serve discovery, registration, the authorize
/// redirect, and both token grants.
#[allow(clippy::too_many_arguments)]
fn oauth_mock_route(
    base_url: &str,
    method: &str,
    path: &str,
    query: Option<&str>,
    authorization: &Option<String>,
    body: &str,
) -> String {
    let ok_json = |value: serde_json::Value| {
        let payload = value.to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    };
    match path {
        "/mcp" => {
            let authorized = authorization
                .as_deref()
                .map(|value| {
                    value == "Bearer mock-access-token" || value == "Bearer mock-refreshed-token"
                })
                .unwrap_or(false);
            if !authorized {
                return format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    base = base_url
                );
            }
            let request: serde_json::Value =
                serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            let id = request.get("id").cloned();
            let Some(id) = id.filter(|value| !value.is_null()) else {
                return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string();
            };
            let result = match request["method"].as_str().unwrap_or_default() {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "oauth-mock", "version": "1.0"}
                }),
                "tools/list" => json!({
                    "tools": [{
                        "name": "echo",
                        "description": "echo tool",
                        "inputSchema": {"type": "object", "properties": {}}
                    }]
                }),
                "tools/call" => json!({
                    "content": [{"type": "text", "text": "echoed"}],
                    "isError": false
                }),
                _ => json!({}),
            };
            ok_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
        }
        "/.well-known/oauth-protected-resource" => ok_json(json!({
            "resource": format!("{base_url}/mcp"),
            "authorization_servers": [format!("{base_url}/as")],
        })),
        "/as/.well-known/oauth-authorization-server" => ok_json(json!({
            "issuer": format!("{base_url}/as"),
            "authorization_endpoint": format!("{base_url}/as/authorize"),
            "token_endpoint": format!("{base_url}/as/token"),
            "registration_endpoint": format!("{base_url}/as/register"),
            "scopes_supported": ["read"],
            "code_challenge_methods_supported": ["S256"],
        })),
        "/as/register" if method == "POST" => ok_json(json!({
            "client_id": "test-client",
            "client_id_issued_at": 0,
        })),
        "/as/authorize" => {
            // Redirect to the flow's loopback callback with a code echoing
            // the state (the "browser" follows it with reqwest).
            let params: std::collections::HashMap<String, String> = query
                .unwrap_or_default()
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                .map(|(key, value)| (key.to_string(), percent_decode(value)))
                .collect();
            let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
            let state = params.get("state").cloned().unwrap_or_default();
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {redirect_uri}?code=mock-auth-code&state={state}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
        }
        "/as/token" if method == "POST" => {
            let form: std::collections::HashMap<String, String> = body
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                .map(|(key, value)| (key.to_string(), percent_decode(value)))
                .collect();
            match form.get("grant_type").map(String::as_str) {
                Some("authorization_code") => ok_json(json!({
                    "access_token": "mock-access-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "mock-refresh-token",
                    "scope": "read",
                })),
                Some("refresh_token") => ok_json(json!({
                    "access_token": "mock-refreshed-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "mock-refresh-token-2",
                    "scope": "read",
                })),
                _ => ok_json(json!({
                    "error": "unsupported_grant_type",
                    "error_description": "mock only answers code and refresh grants",
                })),
            }
        }
        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    }
}

/// Percent-decode a URL component (`%XX` escapes and `+` as space), the way
/// query parameters and form bodies arrive from the OAuth flows.
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

/// Given a workspace-layer OAuth server with no cached tokens, when the
/// background connect observes the 401, then the connection lands in
/// `needs-auth` and the session's agent gets the synthetic authenticate
/// tool; calling it runs the full flow (discovery → registration →
/// authorize URL → callback → token exchange), persists the token set,
/// reconnects, and swaps in the real tools — every MCP request from the
/// reconnected server carries the bearer.
#[tokio::test]
async fn should_complete_the_oauth_flow_through_the_authenticate_tool() {
    let mock = MockOAuthMcpServer::start().await;
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({
            "authsrv": {
                "transport": "http",
                "url": mock.url,
                "auth": {"type": "oauth", "scopes": ["read"]}
            }
        })),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-oauth-flow"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));

    // The lazy connect lands the server in needs-auth (the real 401 leg).
    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&manager, "authsrv", "NeedsAuth", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::NeedsAuth))
    })
    .await;
    let entry = mcp_list_entry_for(&server, "authsrv").await.expect("entry");
    assert_eq!(entry["status"], json!("needs-auth"));

    // A session created afterwards sees the authenticate tool.
    let created = server
        .call_method("session.create", json!({ "title": "t8-oauth" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    wait_for_agent_tool(&server, &session_id, "mcp__authsrv__authenticate", true).await;

    // Calling it returns the authorization URL (the model hands it to the
    // user); the "browser" then follows it to the loopback callback.
    let result = run_main_agent_tool(
        &server,
        &session_id,
        "mcp__authsrv__authenticate",
        json!({}),
    )
    .await;
    assert!(!result.is_error, "the authenticate call must succeed");
    let output = match result.output {
        mirri_core::ToolOutput::Text(text) => text,
        other => panic!("expected text output, got {other:?}"),
    };
    let authorization_url = output
        .lines()
        .find(|line| line.starts_with("http://"))
        .expect("the output carries the authorization URL")
        .to_string();
    assert!(
        authorization_url.contains("/as/authorize"),
        "the URL must point at the authorization endpoint: {authorization_url}"
    );
    tokio::spawn(async move {
        let browser = reqwest::Client::new();
        let _ = browser.get(authorization_url).send().await;
    });

    // The flow completes in the background: tokens persist, the server
    // reconnects, the real tool replaces the synthetic one.
    wait_for_pool_state(&manager, "authsrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    wait_for_agent_tool(&server, &session_id, "mcp__authsrv__echo", true).await;
    wait_for_agent_tool(&server, &session_id, "mcp__authsrv__authenticate", false).await;

    // The token set is persisted under the Mirri home.
    let token_path = home.path().join("mcp-tokens").join("authsrv.json");
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&token_path).expect("the token file must exist"),
    )
    .expect("the token file must parse");
    assert_eq!(persisted["access_token"], json!("mock-access-token"));

    // The reconnected server's requests carried the bearer: once the first
    // authenticated request happens, no later one may be bare (the earlier
    // ones are the legitimate pre-auth 401 leg: the connect attempt and the
    // RFC 9728 resource-metadata probe).
    let mcp_requests = mock.requests_of("/mcp");
    let first_bearer = mcp_requests
        .iter()
        .position(|request| request.authorization.is_some())
        .expect("at least one request must carry the bearer");
    assert!(
        mcp_requests[first_bearer..]
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer mock-access-token")),
        "every post-auth request must carry the token: {mcp_requests:?}"
    );
    // The token endpoint saw the authorization-code exchange.
    assert!(
        mock.requests_of("/as/token")
            .iter()
            .any(|request| request.body.contains("grant_type=authorization_code")),
        "the code exchange must hit the token endpoint"
    );

    // The real tool works end to end.
    let echoed = run_main_agent_tool(&server, &session_id, "mcp__authsrv__echo", json!({})).await;
    assert!(!echoed.is_error);
    assert!(
        matches!(&echoed.output, mirri_core::ToolOutput::Text(text) if text.contains("echoed")),
        "the echo tool must work: {:?}",
        echoed.output
    );
}

/// Given a persisted token set from a completed flow, when a fresh engine
/// (same home) connects, then the server connects directly — no authorize
/// redirect is ever issued (reconnect without re-authorization).
#[tokio::test]
async fn should_reconnect_without_a_new_authorization_when_tokens_persist() {
    let mock = MockOAuthMcpServer::start().await;
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({
            "authsrv": {
                "transport": "http",
                "url": mock.url,
                "auth": {"type": "oauth", "scopes": ["read"]}
            }
        })),
    );
    // Pre-seed the token store (the state a previous flow left behind).
    let token_dir = home.path().join("mcp-tokens");
    std::fs::create_dir_all(&token_dir).expect("create token dir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    std::fs::write(
        token_dir.join("authsrv.json"),
        serde_json::json!({
            "access_token": "mock-access-token",
            "refresh_token": "mock-refresh-token",
            "expires_at": now + 3600,
            "token_type": "Bearer",
            "client_id": "test-client",
            "token_endpoint": format!("{}/as/token", mock.base_url),
        })
        .to_string(),
    )
    .expect("seed token set");

    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-oauth-reuse"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&manager, "authsrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;

    // No authorization happened: no register, no authorize redirect, no
    // code exchange — only the bearer-carrying MCP requests.
    assert!(
        mock.requests_of("/as/register").is_empty()
            && mock.requests_of("/as/authorize").is_empty()
            && mock.requests_of("/as/token").is_empty(),
        "a persisted token must not trigger a new authorization flow"
    );
    let mcp_requests = mock.requests_of("/mcp");
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref() == Some("Bearer mock-access-token")),
        "the reused token must be attached: {mcp_requests:?}"
    );
}

/// Given a persisted but expired token, when the agent calls the server's
/// tool, then the provider refreshes first (the refresh grant) and the
/// request carries the refreshed token.
#[tokio::test]
async fn should_refresh_the_expired_token_through_the_tool_call() {
    let mock = MockOAuthMcpServer::start().await;
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({
            "authsrv": {
                "transport": "http",
                "url": mock.url,
                "auth": {"type": "oauth", "scopes": ["read"]}
            }
        })),
    );
    // An expired token with a refresh token — refresh must happen before
    // the first request of a new connection.
    let token_dir = home.path().join("mcp-tokens");
    std::fs::create_dir_all(&token_dir).expect("create token dir");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    std::fs::write(
        token_dir.join("authsrv.json"),
        serde_json::json!({
            "access_token": "mock-access-token",
            "refresh_token": "mock-refresh-token",
            "expires_at": now - 60,
            "token_type": "Bearer",
            "client_id": "test-client",
            "token_endpoint": format!("{}/as/token", mock.base_url),
        })
        .to_string(),
    )
    .expect("seed expired token set");

    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-oauth-refresh"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&manager, "authsrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;

    // The connect path refreshed the expired token before its requests.
    assert!(
        mock.requests_of("/as/token")
            .iter()
            .any(|request| request.body.contains("grant_type=refresh_token")),
        "the expired token must be refreshed"
    );
    let mcp_requests = mock.requests_of("/mcp");
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref()
                    == Some("Bearer mock-refreshed-token")),
        "the refreshed token must be attached: {mcp_requests:?}"
    );
}

/// Given a connected HTTP server that is killed (its port closes), when a
/// tool call observes the loss, then the bridge marks the connection lost
/// (the tools are dropped), the automatic reconnection retries with
/// backoff, and once the server is back the tools are re-registered and the
/// next call succeeds.
#[tokio::test]
async fn should_auto_reconnect_and_restore_tools_when_the_server_returns() {
    let mock = spawn_mock_mcp_http_server(0, false).await;
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({"mock": {"transport": "http", "url": mock.url.clone()}})),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-auto-reconnect"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&manager, "mock", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    let created = server
        .call_method("session.create", json!({ "title": "t8-reconnect" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    wait_for_agent_tool(&server, &session_id, "mcp__mock__echo", true).await;

    // Kill the server (abort the accept loop; the port closes).
    mock.handle.abort();
    // The call through the bridge observes the loss and schedules the
    // automatic reconnection.
    let failed = run_main_agent_tool(&server, &session_id, "mcp__mock__echo", json!({})).await;
    assert!(
        failed.is_error,
        "the call against the killed server must fail"
    );
    wait_for_pool_state(&manager, "mock", "Failed", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Failed { .. }))
    })
    .await;

    // Restart the server on the same port, then let the backoff retries
    // land (default backoff: 1s base, cap 30s — a restarted server inside
    // the first few retries recovers).
    let url = mock.url.clone();
    restart_mock_mcp_http_server(&url).await;

    wait_for_pool_state(&manager, "mock", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    wait_for_agent_tool(&server, &session_id, "mcp__mock__echo", true).await;

    let recovered = run_main_agent_tool(&server, &session_id, "mcp__mock__echo", json!({})).await;
    assert!(
        !recovered.is_error,
        "the next call must succeed after recovery"
    );
}

/// Given a session whose main agent holds a connected user-layer MCP tool,
/// when the session is forked, then the fork's main agent holds the same
/// tool and a call through it routes to the live server — the tool
/// registration the create/resume paths perform must extend to the fork
/// path too (a fork that silently loses MCP tools breaks the session).
#[tokio::test]
async fn should_register_user_pool_mcp_tools_when_a_session_is_forked() {
    let mock = spawn_mock_mcp_http_server(0, false).await;
    let (home, workspace) = layered_mcp_fixture(
        json!({"usersrv": {"transport": "http", "url": mock.url.clone()}}),
        None,
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-fork-mcp"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let user_pool = Arc::clone(server.user_mcp_pool().expect("user pool attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&user_pool.manager(), "usersrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    let created = server
        .call_method("session.create", json!({ "title": "t8-fork-mcp" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    wait_for_agent_tool(&server, &session_id, "mcp__usersrv__echo", true).await;

    let forked = server
        .call_method("session.fork", json!({ "session_id": &session_id }))
        .await
        .expect("session.fork");
    let forked_id = forked["id"]
        .as_str()
        .expect("forked session id")
        .to_string();

    wait_for_agent_tool(&server, &forked_id, "mcp__usersrv__echo", true).await;
    let echoed = run_main_agent_tool(&server, &forked_id, "mcp__usersrv__echo", json!({})).await;
    assert!(
        !echoed.is_error,
        "the forked session's MCP call must succeed: {:?}",
        echoed.output
    );
}

/// Given a session whose main agent holds a connected user-layer MCP tool,
/// when `session.createChild` forks the session, then the child's main
/// agent holds the same tool and a call through it routes to the live
/// server — createChild shares the fork pipeline, so it needs the same MCP
/// registration the fork path got.
#[tokio::test]
async fn should_register_user_pool_mcp_tools_when_create_child_forks_a_session() {
    let mock = spawn_mock_mcp_http_server(0, false).await;
    let (home, workspace) = layered_mcp_fixture(
        json!({"usersrv": {"transport": "http", "url": mock.url.clone()}}),
        None,
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-child-mcp"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let user_pool = Arc::clone(server.user_mcp_pool().expect("user pool attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&user_pool.manager(), "usersrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    let created = server
        .call_method("session.create", json!({ "title": "t8-child-mcp" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    wait_for_agent_tool(&server, &session_id, "mcp__usersrv__echo", true).await;

    let child = server
        .call_method(
            "session.createChild",
            json!({ "sourceSessionId": &session_id }),
        )
        .await
        .expect("session.createChild");
    let child_id = child["id"].as_str().expect("child session id").to_string();

    wait_for_agent_tool(&server, &child_id, "mcp__usersrv__echo", true).await;
    let echoed = run_main_agent_tool(&server, &child_id, "mcp__usersrv__echo", json!({})).await;
    assert!(
        !echoed.is_error,
        "the child session's MCP call must succeed: {:?}",
        echoed.output
    );
}

/// Rebind the mock MCP HTTP server on the port its URL names (the
/// "restarted server" of the reconnect test).
async fn restart_mock_mcp_http_server(url: &str) {
    let port: u16 = url
        .rsplit(':')
        .next()
        .and_then(|tail| tail.split('/').next())
        .and_then(|tail| tail.parse().ok())
        .expect("the mock URL carries a port");
    // The killed listener's socket frees the port immediately; a bind
    // racing the abort retries briefly.
    for _ in 0..40 {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (delay, error) = (0u64, false);
                    let tools = vec![("echo".to_string(), "echo tool".to_string())];
                    tokio::spawn(serve_mock_mcp_connection(stream, delay, error, tools));
                }
            });
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("failed to rebind the mock MCP server on port {port}");
}

/// Given a slow (3s handshake) workspace server, when `session.create`
/// runs as the first engine call, then the response returns immediately —
/// the lazy-connect spawn never blocks the session (the S2 fix pinned).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // the hermetic $HOME pin holds the crate home lock across the async body
async fn should_return_session_create_immediately_when_the_mcp_server_is_slow() {
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({"slow": {"command": "sleep", "args": ["3"]}})),
    );
    // Pin an isolated $HOME: the lazy skill-watcher mount reads
    // `~/.agents/skills` and an un-pinned real home (a large skill tree)
    // would make the first call's awaited mount dominate the elapsed
    // budget — the pin keeps the timing assertion about the MCP connect
    // only, and the test hermetic against the machine's state.
    let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
    let _home_lock = crate::bootstrap::test_env::home_lock();
    let previous_home = std::env::var_os("HOME");
    // SAFETY: held for the rest of the test; restored by the guard's
    // drop while the crate home lock is still held.
    unsafe { std::env::set_var("HOME", isolated_home.path()) };
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            // SAFETY: the pre-test value; the crate home lock (still
            // alive in the test body by drop order) serializes the
            // write.
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("HOME", value) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }
    let _restore_home = RestoreHome(previous_home);
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-lazy-connect-pin"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );

    // session.create as the very first call: it spawns the connect set and
    // must not wait for the 3s server.
    let started = std::time::Instant::now();
    server
        .call_method("session.create", json!({ "title": "t8-lazy" }))
        .await
        .expect("session.create must succeed");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "session.create must not wait for the slow MCP server (took {elapsed:?})"
    );

    // The connect still happens in the background (the entry registers,
    // then times out into Failed after its own 3s+ window).
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));
    wait_for_pool_state(&manager, "slow", "registered", |state| state.is_some()).await;
}

/// Given a connected user-layer server, when a workspace config write
/// hot-adds a same-name server (the T1 review F-T1-3 shape), then the
/// workspace entry owns the name from that moment: its tools are the ones
/// registered, and a later user-pool event for the name cannot take it
/// back.
#[tokio::test]
async fn should_refresh_the_user_pool_shadow_when_a_workspace_server_is_hot_added() {
    // Two distinct mock servers: the user layer's (whose tool errors) and
    // the workspace layer's (whose tool echoes) — which pool a call routed
    // to is observable from the outcome.
    let user_mock = spawn_mock_mcp_http_server(0, true).await;
    let workspace_mock = spawn_mock_mcp_http_server(0, false).await;
    let (home, workspace) = layered_mcp_fixture(
        json!({"shared": {"transport": "http", "url": user_mock.url.clone()}}),
        None,
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-shadow-refresh"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let user_pool = Arc::clone(server.user_mcp_pool().expect("user pool attached"));

    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&user_pool.manager(), "shared", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    let created = server
        .call_method("session.create", json!({ "title": "t8-shadow" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    wait_for_agent_tool(&server, &session_id, "mcp__shared__echo", true).await;

    // Before the hot-add, the tool routes to the user layer (the erroring
    // mock) — proving the registration really points there.
    let user_routed =
        run_main_agent_tool(&server, &session_id, "mcp__shared__echo", json!({})).await;
    assert!(
        user_routed.is_error,
        "the user layer's mock must be the one routed"
    );

    // The hot-add: the workspace layer now defines the same name.
    server
        .call_method(
            "mcp.config.set",
            json!({
                "scope": "workspace",
                "name": "shared",
                "config": {"transport": "http", "url": workspace_mock.url.clone()}
            }),
        )
        .await
        .expect("the adding write must succeed");
    let workspace_manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));
    wait_for_pool_state(&workspace_manager, "shared", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    // The workspace registration replaced the user one.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A later user-pool event for the name (a reconnect, say) must NOT
    // re-take the registration (the refreshed shadow set filters it).
    user_pool
        .manager()
        .reconnect("shared")
        .await
        .expect("the user-pool reconnect itself succeeds");
    wait_for_pool_state(&user_pool.manager(), "shared", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The call routes to the workspace mock: echoed, not errored.
    let workspace_routed =
        run_main_agent_tool(&server, &session_id, "mcp__shared__echo", json!({})).await;
    assert!(
        !workspace_routed.is_error,
        "the workspace entry must own the name after the hot-add (F-T1-3): {:?}",
        workspace_routed.output
    );
}

// ---------------------------------------------------------------------------
// W7 — per-workspace project layers with the trust gate (design 08 §2/§5 D6)
// ---------------------------------------------------------------------------

/// Write one workspace's project-root MCP layer (`<root>/.mcp.json`).
fn write_project_mcp_layer(root: &std::path::Path, servers: serde_json::Value) {
    std::fs::write(
        root.join(".mcp.json"),
        json!({ "mcpServers": servers }).to_string(),
    )
    .expect("write the project-root .mcp.json");
}

/// A multi-workspace MCP fixture (the W7 shape): a home whose user layer
/// declares `usersrv` and whose `workspaces.json` registers a SECOND
/// workspace row, plus a default workspace root. Each test plants the two
/// roots' project layers itself. Returns the second workspace's registry
/// key.
fn multi_workspace_mcp_fixture(
    home: &std::path::Path,
    _default_root: &std::path::Path,
    second_root: &std::path::Path,
) -> String {
    let mirri_home = home.join(".mirri-code");
    std::fs::create_dir_all(&mirri_home).expect("create the mirri home dir");
    std::fs::write(
        mirri_home.join("mcp.json"),
        json!({"mcpServers": {"usersrv": {"command": "echo", "enabled": false}}}).to_string(),
    )
    .expect("write the user-layer mcp.json");
    let second_key = mirri_core::encode_work_dir_key(second_root);
    std::fs::write(
        home.join("workspaces.json"),
        json!({
            "version": 1,
            "workspaces": {
                second_key.as_str(): {
                    "root": second_root.to_string_lossy(),
                    "name": "second",
                    "created_at": "2026-01-01T00:00:00.000Z",
                    "last_opened_at": "2026-06-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        })
        .to_string(),
    )
    .expect("write workspaces.json");
    second_key
}

/// Build the second workspace's runtime and run one engine call (the lazy
/// connect pass runs on `call_method`), then hand back the built runtime.
async fn build_second_and_sync(
    server: &EmbeddedServer,
    second_key: &str,
) -> Arc<crate::workspace_registry::WorkspaceRuntime> {
    let runtime = server
        .registry
        .ensure_built(second_key)
        .expect("the second workspace row is registered")
        .expect("the second workspace runtime builds");
    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("the engine call runs the lazy connect pass");
    runtime
}

/// Given an UNTRUSTED registered workspace whose project layer declares a
/// server, when the engine runs (the lazy connect pass and an explicit
/// `mcp.sync`), then ZERO project-layer servers are ever connected on that
/// workspace's pool — the D6 red line — while the default (pin) root and
/// the user layer load as before.
#[tokio::test]
async fn should_connect_zero_project_layer_servers_for_an_untrusted_workspace() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    write_project_mcp_layer(
        default_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["default"], "enabled": false}}),
    );
    write_project_mcp_layer(
        second_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["second"], "enabled": false}}),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t7-untrusted-gate"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );

    // When the second workspace is built and the engine runs
    let runtime = build_second_and_sync(&server, &second_key).await;
    let second_manager = runtime
        .mcp_manager
        .as_ref()
        .expect("the untrusted workspace still gets a pool (user-layer writes address it)");
    wait_for_connection(
        server.mcp_manager().expect("the default pool is attached"),
        "projsrv",
    )
    .await;

    // Then the untrusted workspace's project layer connected NOTHING (the
    // D6 red line: zero project-layer connections).
    assert!(
        second_manager.list_connections().await.is_empty(),
        "an untrusted workspace must hold zero project-layer connections"
    );
    assert!(!runtime.trusted(), "the runtime records the gate verdict");

    // And an explicit reconcile (the kernel's own trust gate) changes
    // nothing: the untrusted root is skipped, not just the build-time load.
    server
        .call_method("mcp.sync", json!([]))
        .await
        .expect("mcp.sync succeeds");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        second_manager.list_connections().await.is_empty(),
        "the reconcile kernel must keep an untrusted workspace's pool empty"
    );

    // And the user layer still loaded exactly once (the user pool is the
    // only thing an untrusted workspace shares).
    let user_pool = server.user_mcp_pool().expect("the user pool is attached");
    wait_for_connection(&user_pool.manager(), "usersrv").await;
    let user_connections = user_pool.manager().list_connections().await;
    assert_eq!(
        user_connections.len(),
        1,
        "the user layer must hold exactly the one user-layer server: {user_connections:?}"
    );
    assert_eq!(user_connections[0].0, "usersrv");
}

/// Given a registered workspace the home's trust store marks trusted, when
/// a fresh engine (the reloaded runtime) builds it, then the project layer
/// loads: its own pool connects the workspace's project servers (the W2
/// untrusted-for-everything-new transition is superseded), the default
/// root keeps its own, and the user pool still connects each user-layer
/// server exactly once.
#[tokio::test]
async fn should_load_the_project_layer_when_the_workspace_is_trusted() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    write_project_mcp_layer(
        default_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["default"], "enabled": false}}),
    );
    write_project_mcp_layer(
        second_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["second"], "enabled": false}}),
    );
    // The trust grant (the W8 `workspace.setTrust` write face lands next;
    // here the store is seeded directly).
    crate::workspace_trust::WorkspaceTrustStore::open(Some(home.path()))
        .expect("the trust store opens")
        .set_trusted(&second_key, true)
        .expect("the trust grant persists");

    // When a fresh engine assembles over the same home (the reloaded
    // runtime) and the second workspace is built
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t7-trusted-load"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let runtime = build_second_and_sync(&server, &second_key).await;
    let second_manager = runtime
        .mcp_manager
        .as_ref()
        .expect("the trusted workspace's pool is wired");

    // Then the trusted workspace's project layer connected its server
    assert!(runtime.trusted());
    wait_for_connection(second_manager, "projsrv").await;
    let second_connections = second_manager.list_connections().await;
    assert_eq!(second_connections.len(), 1);
    assert_eq!(second_connections[0].0, "projsrv");

    // The default root keeps its own project server (a distinct pool).
    let default_manager = server.mcp_manager().expect("the default pool is attached");
    wait_for_connection(default_manager, "projsrv").await;
    assert!(
        !Arc::ptr_eq(second_manager, default_manager,),
        "the two workspaces must hold distinct workspace pools"
    );

    // And the user layer connected each user server exactly once (the
    // per-workspace fan-out never touches the user pool).
    let user_pool = server.user_mcp_pool().expect("the user pool is attached");
    wait_for_connection(&user_pool.manager(), "usersrv").await;
    let user_connections = user_pool.manager().list_connections().await;
    assert_eq!(user_connections.len(), 1);
    assert_eq!(user_connections[0].0, "usersrv");
}

/// Given two workspaces whose project layers each define the SAME server
/// name with DIFFERENT configs, when both load, then the shadowing applies
/// per workspace independently — each pool holds its own connection under
/// its own definition, never the other's — and the user pool connects
/// each user-layer server once (the D2 process-single invariant).
#[tokio::test]
async fn should_shadow_the_same_server_name_per_workspace_independently() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    // Two DIFFERENT project definitions of the same name.
    write_project_mcp_layer(
        default_root.path(),
        json!({"shared": {"command": "echo", "args": ["workspace-a"], "enabled": false}}),
    );
    write_project_mcp_layer(
        second_root.path(),
        json!({"shared": {"command": "echo", "args": ["workspace-b"], "enabled": false}}),
    );
    // The user layer defines the same name too — the workspace entries own
    // it per workspace (the user pool keeps its own layer's definition and
    // never connects this name).
    let mirri_home = home.path().join(".mirri-code");
    std::fs::write(
        mirri_home.join("mcp.json"),
        json!({"mcpServers": {
            "shared": {"command": "echo", "args": ["user"], "enabled": false},
            "usersrv": {"command": "echo", "enabled": false}
        }})
        .to_string(),
    )
    .expect("rewrite the user-layer mcp.json");
    crate::workspace_trust::WorkspaceTrustStore::open(Some(home.path()))
        .expect("the trust store opens")
        .set_trusted(&second_key, true)
        .expect("the trust grant persists");

    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t7-per-workspace-shadow"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let runtime = build_second_and_sync(&server, &second_key).await;
    let second_manager = runtime
        .mcp_manager
        .as_ref()
        .expect("the second workspace's pool is wired");
    let default_manager = server.mcp_manager().expect("the default pool is attached");
    wait_for_connection(default_manager, "shared").await;
    wait_for_connection(second_manager, "shared").await;

    // Then each workspace's pool holds the name under ITS OWN definition
    // (independent shadowing), never the other workspace's.
    let default_config = default_manager
        .registered_configs()
        .await
        .get("shared")
        .cloned()
        .expect("the default workspace's own entry");
    let second_config = second_manager
        .registered_configs()
        .await
        .get("shared")
        .cloned()
        .expect("the second workspace's own entry");
    let args_of = |config: &mirri_mcp::McpServerConfig| match &config.transport {
        mirri_mcp::McpTransportConfig::Stdio { args, .. } => args.clone(),
        _ => Vec::new(),
    };
    assert_eq!(args_of(&default_config), vec!["workspace-a".to_string()]);
    assert_eq!(args_of(&second_config), vec!["workspace-b".to_string()]);

    // And the user pool connected each of its layer's servers exactly ONCE
    // (the D2 process-single invariant): `usersrv` unshadowed, and `shared`
    // whose USER connection stays up while its tools are shadowed (spec
    // §4.2: "a user server whose name a project entry shares stays
    // connected in the pool, but its tools never reach agents").
    let user_pool = server.user_mcp_pool().expect("the user pool is attached");
    wait_for_connection(&user_pool.manager(), "usersrv").await;
    wait_for_connection(&user_pool.manager(), "shared").await;
    let user_connections = user_pool.manager().list_connections().await;
    let mut names: Vec<String> = user_connections.into_iter().map(|(name, _)| name).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["shared".to_string(), "usersrv".to_string()],
        "each user-layer server connects exactly once across both workspaces"
    );

    // And the shadow view owns the workspace names: a user-pool event for
    // `shared` must never re-take the tool registration (per-workspace
    // shadowing of the user layer), while `usersrv` stays user-owned.
    let shadow = server
        .mcp_user_pool_shadow
        .read()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        shadow.contains("shared"),
        "the workspace entries own the shadowed name: {shadow:?}"
    );
    assert!(
        !shadow.contains("usersrv"),
        "an unshadowed user-layer name stays user-owned: {shadow:?}"
    );
}

/// Given an untrusted registered workspace whose root carries an
/// AGENTS.md, when a session is created on that workspace's runtime, then
/// the agent's system prompt does NOT carry the project section (D6's
/// second half) — and a trusted engine over the same home does inject it.
#[tokio::test]
async fn should_not_inject_the_project_agents_md_for_an_untrusted_workspace_session() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    std::fs::write(
        second_root.path().join("AGENTS.md"),
        "W7_PROJECT_RULES_MARKER",
    )
    .unwrap();

    // Given an engine with NO trust grant for the second workspace
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t7-agents-gate"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let runtime = server
        .registry
        .ensure_built(&second_key)
        .expect("the second workspace row is registered")
        .expect("the second workspace runtime builds");

    // When a session with a main agent is created on that runtime (the
    // host-API path — the session never touches the default workspace)
    let now = chrono::Utc::now();
    let session = runtime
        .manager
        .create_session(
            mirri_core::SessionMetadata {
                title: "t7-agents-gate".to_string(),
                created_at: now,
                updated_at: now,
                tags: Default::default(),
                archived: false,
                is_custom_title: false,
                work_dir: None,
                last_prompt: None,
                forked_from: None,
                cache_root_id: None,
                custom: None,
                last_used_at: None,
            },
            mirri_core::SessionScopeConfig::default(),
        )
        .await
        .expect("the untrusted workspace creates sessions");
    session
        .create_agent(
            mirri_core::AgentId::new(DEFAULT_MAIN_AGENT_ID),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .expect("the main agent is creatable");

    // Then the agent's system prompt carries NO project AGENTS.md content
    let agent = session
        .get_agent(&mirri_core::AgentId::new(DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("the main agent is live");
    let prompt = agent
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .system_prompt
        .clone()
        .unwrap_or_default();
    assert!(
        !prompt.contains("W7_PROJECT_RULES_MARKER"),
        "the untrusted workspace's project AGENTS.md must not be injected: {prompt}"
    );

    // And when the SAME home trusts the workspace (a fresh engine — the
    // reloaded runtime), the project section rides again.
    crate::workspace_trust::WorkspaceTrustStore::open(Some(home.path()))
        .expect("the trust store opens")
        .set_trusted(&second_key, true)
        .expect("the trust grant persists");
    let trusted_server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t7-agents-gate-trusted"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let trusted_runtime = trusted_server
        .registry
        .ensure_built(&second_key)
        .expect("the second workspace row is registered")
        .expect("the trusted workspace runtime builds");
    let trusted_session = trusted_runtime
        .manager
        .create_session(
            mirri_core::SessionMetadata::default(),
            mirri_core::SessionScopeConfig::default(),
        )
        .await
        .expect("the trusted workspace creates sessions");
    trusted_session
        .create_agent(
            mirri_core::AgentId::new(DEFAULT_MAIN_AGENT_ID),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .expect("the main agent is creatable");
    let trusted_agent = trusted_session
        .get_agent(&mirri_core::AgentId::new(DEFAULT_MAIN_AGENT_ID))
        .await
        .expect("the main agent is live");
    let trusted_prompt = trusted_agent
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .system_prompt
        .clone()
        .unwrap_or_default();
    assert!(
        trusted_prompt.contains("W7_PROJECT_RULES_MARKER"),
        "a trusted workspace's project AGENTS.md must inject: {trusted_prompt}"
    );
}

// W8 — the trust management RPC + the live gate/linkage flip
// ---------------------------------------------------------------------------

/// Poll until the pool holds exactly `expected` connections (the
/// post-flip convergence face of `wait_for_connection`).
async fn wait_for_connection_count(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    expected: usize,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count = manager.list_connections().await.len();
        if count == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("connection count never converged to {expected}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The `trusted` field of one `workspace.list` row.
async fn listed_trust_of(server: &EmbeddedServer, workspace_id: &str) -> bool {
    let rows = server
        .call_method("workspace.list", json!({}))
        .await
        .expect("workspace.list resolves");
    rows.as_array()
        .expect("the bare array")
        .iter()
        .find(|row| row["id"] == json!(workspace_id))
        .unwrap_or_else(|| panic!("the row {workspace_id} must be listed"))["trusted"]
        .as_bool()
        .expect("trusted is a boolean")
}

/// Given an UNTRUSTED registered workspace (built, engine running, zero
/// project-layer connections — the W7 red line), when `workspace.setTrust`
/// grants trust, then the result is the TS trust face `{trusted: true}`,
/// the workspace's project layer connects on its own pool WITHOUT a
/// runtime rebuild, and `workspace.list` reports the row trusted (the
/// store read, not a snapshot) while the default row stays trusted.
#[tokio::test]
async fn should_connect_the_project_layer_through_the_set_trust_rpc() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    write_project_mcp_layer(
        default_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["default"], "enabled": false}}),
    );
    write_project_mcp_layer(
        second_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["second"], "enabled": false}}),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-set-trust-connect"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let runtime = build_second_and_sync(&server, &second_key).await;
    let second_manager = runtime
        .mcp_manager
        .as_ref()
        .expect("the untrusted workspace still gets a pool");
    assert!(
        second_manager.list_connections().await.is_empty(),
        "the W7 zero-connect red line holds before the grant"
    );
    assert!(!listed_trust_of(&server, &second_key).await);

    // When trust is granted through the W8 RPC (the snake_case param pins
    // the alias fold)
    let result = server
        .call_method(
            "workspace.setTrust",
            json!({ "workspace_id": second_key, "trusted": true }),
        )
        .await
        .expect("workspace.setTrust resolves");

    // Then the result is the TS trust face (workspaceTrustResponseSchema)
    assert_eq!(result, json!({ "trusted": true }));

    // And the project layer connected on the workspace's own pool — the
    // same runtime, no rebuild (the runtime Arc identity is unchanged)
    wait_for_connection(second_manager, "projsrv").await;
    let after = server
        .registry
        .built_runtime(&second_key)
        .expect("still built");
    assert!(
        Arc::ptr_eq(&runtime, &after),
        "the trust flip must not rebuild the runtime"
    );
    assert!(after.trusted(), "the live verdict flipped");

    // And the listing reads the store: the second row flipped, the
    // default row stays trusted by construction.
    assert!(listed_trust_of(&server, &second_key).await);
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    assert!(listed_trust_of(&server, &default_key).await);

    // And an idempotent re-grant answers without touching anything.
    let again = server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": second_key, "trusted": true }),
        )
        .await
        .expect("the idempotent re-grant resolves");
    assert_eq!(again, json!({ "trusted": true }));
}

/// Given a TRUSTED workspace whose project layer is connected, when
/// `workspace.setTrust` revokes trust, then the workspace's project-layer
/// connections disconnect IMMEDIATELY (the gate symmetry, US4.1), the
/// listing reports untrusted, the user layer keeps its single connection,
/// and untrusting the pinned default workspace is refused.
#[tokio::test]
async fn should_disconnect_the_project_layer_immediately_when_trust_is_revoked() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    write_project_mcp_layer(
        default_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["default"], "enabled": false}}),
    );
    write_project_mcp_layer(
        second_root.path(),
        json!({"projsrv": {"command": "echo", "args": ["second"], "enabled": false}}),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-set-trust-revoke"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    // The pre-granted state (the W7 trusted-load shape)
    crate::workspace_trust::WorkspaceTrustStore::open(Some(home.path()))
        .expect("the trust store opens")
        .set_trusted(&second_key, true)
        .expect("the trust grant persists");
    let runtime = build_second_and_sync(&server, &second_key).await;
    let second_manager = runtime
        .mcp_manager
        .as_ref()
        .expect("the trusted workspace's pool is wired");
    wait_for_connection(second_manager, "projsrv").await;

    // When trust is revoked through the W8 RPC
    let result = server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": second_key, "trusted": false }),
        )
        .await
        .expect("the revoke resolves");

    // Then the result carries the new state and the project layer is gone
    // NOW (the kernel's converge-to-empty arm — no restart, no rebuild)
    assert_eq!(result, json!({ "trusted": false }));
    wait_for_connection_count(second_manager, 0).await;
    assert!(!listed_trust_of(&server, &second_key).await);
    assert!(
        !server
            .registry
            .built_runtime(&second_key)
            .expect("still built")
            .trusted()
    );

    // And the user layer keeps its single connection (untrust never
    // touches the shared user pool).
    let user_pool = server.user_mcp_pool().expect("the user pool is attached");
    wait_for_connection(&user_pool.manager(), "usersrv").await;
    assert_eq!(user_pool.manager().list_connections().await.len(), 1);

    // And untrusting the pinned default workspace is refused (the
    // workspace.delete refusal's mirror).
    let default_key = mirri_core::encode_work_dir_key(default_root.path());
    let refusal = server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": default_key, "trusted": false }),
        )
        .await
        .expect_err("untrusting the default must be refused");
    assert!(
        refusal.to_string().contains("-32602"),
        "the default-refusal is an invalid_params error: {refusal}"
    );

    // And an unknown id is the W4/W5 not-found spelling.
    let unknown = server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": "wd_missing_000000000000", "trusted": true }),
        )
        .await
        .expect_err("an unknown workspace must be refused");
    assert!(
        unknown.to_string().contains("does not exist"),
        "the unknown-id refusal spells the not-found text: {unknown}"
    );
}

/// Given a LIVE session on an untrusted workspace whose agent prompt has
/// no project AGENTS.md section, when `workspace.setTrust` grants and
/// later revokes trust, then the SAME live session's agent prompt hot-swaps
/// the project section both ways (the TS v2 same-process reload parity —
/// the session keeps running, nothing is closed).
#[tokio::test]
async fn should_hot_swap_the_project_agents_md_section_when_trust_flips_on_a_live_session() {
    let home = tempfile::tempdir().expect("home tempdir");
    let default_root = tempfile::tempdir().expect("default root tempdir");
    let second_root = tempfile::tempdir().expect("second root tempdir");
    let second_key =
        multi_workspace_mcp_fixture(home.path(), default_root.path(), second_root.path());
    std::fs::write(second_root.path().join("AGENTS.md"), "W8_LIVE_TRUST_MARKER").unwrap();
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-live-agents-swap"),
        default_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let runtime = server
        .registry
        .ensure_built(&second_key)
        .expect("the second workspace row is registered")
        .expect("the second workspace runtime builds");
    let session = runtime
        .manager
        .create_session(
            mirri_core::SessionMetadata::default(),
            mirri_core::SessionScopeConfig::default(),
        )
        .await
        .expect("the untrusted workspace creates sessions");
    let agent_id = mirri_core::AgentId::new(DEFAULT_MAIN_AGENT_ID);
    // A profile-resolved base prompt (every real main agent carries one —
    // the refresh path rebuilds the workspace suffixes ON TOP of it).
    session
        .create_agent(
            agent_id.clone(),
            mirri_core::AgentScopeConfig {
                system_prompt: Some("base prompt".to_string()),
                ..mirri_core::AgentScopeConfig::default()
            },
        )
        .await
        .expect("the main agent is creatable");
    let prompt_of = || async {
        session
            .get_agent(&agent_id)
            .await
            .expect("the main agent is live")
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .system_prompt
            .clone()
            .unwrap_or_default()
    };
    assert!(
        !prompt_of().await.contains("W8_LIVE_TRUST_MARKER"),
        "the untrusted workspace's project AGENTS.md must start excluded"
    );

    // When trust is granted on the LIVE runtime
    server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": second_key, "trusted": true }),
        )
        .await
        .expect("the grant resolves");

    // Then the SAME live agent's prompt gained the project section (the
    // best-effort refresh — no new session, no rebuild).
    assert!(
        prompt_of().await.contains("W8_LIVE_TRUST_MARKER"),
        "trusting must hot-add the project AGENTS.md to the live agent"
    );

    // And a session created AFTER the grant inherits the trusted gate.
    let later = runtime
        .manager
        .create_session(
            mirri_core::SessionMetadata::default(),
            mirri_core::SessionScopeConfig::default(),
        )
        .await
        .expect("the trusted workspace creates sessions");
    later
        .create_agent(agent_id.clone(), mirri_core::AgentScopeConfig::default())
        .await
        .expect("the later agent is creatable");
    assert!(
        later
            .get_agent(&agent_id)
            .await
            .expect("the later agent is live")
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .system_prompt
            .clone()
            .unwrap_or_default()
            .contains("W8_LIVE_TRUST_MARKER"),
        "sessions created after the grant inject the project section"
    );

    // And when trust is revoked, the live agent's prompt loses the
    // section again (the gate symmetry's AGENTS half).
    server
        .call_method(
            "workspace.setTrust",
            json!({ "workspaceId": second_key, "trusted": false }),
        )
        .await
        .expect("the revoke resolves");
    assert!(
        !prompt_of().await.contains("W8_LIVE_TRUST_MARKER"),
        "untrusting must strip the project AGENTS.md from the live agent"
    );
}

// ---------------------------------------------------------------------------
// Worker-pinning deadlock: registrations vs a turn-held tool-table read
// guard (the production incident — every tokio worker blocked inside
// `tool_manager.write()` froze the whole `mirri web` runtime).
// ---------------------------------------------------------------------------

/// A plain MCP-over-HTTP mock: no auth, answers `initialize` and
/// `tools/list` (one tool) — enough for the pool to reach `Connected`.
struct PlainMcpMock {
    url: String,
}

impl PlainMcpMock {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the plain MCP mock");
        let port = listener.local_addr().expect("local addr").port();
        let url = format!("http://127.0.0.1:{port}/mcp");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut head = String::new();
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        if stream.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push_str(&String::from_utf8_lossy(&line));
                        if line == b"\r\n" {
                            break;
                        }
                    }
                    let content_length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; content_length];
                    if content_length > 0 && stream.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let body = String::from_utf8_lossy(&body).to_string();
                    let request: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let id = request.get("id").cloned();
                    let Some(id) = id.filter(|value| !value.is_null()) else {
                        let _ = stream
                            .get_mut()
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    };
                    let result = match request["method"].as_str().unwrap_or_default() {
                        "initialize" => json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": {"name": "plain-mock", "version": "1.0"}
                        }),
                        "tools/list" => json!({
                            "tools": [{
                                "name": "probe",
                                "description": "probe tool",
                                "inputSchema": {"type": "object", "properties": {}}
                            }]
                        }),
                        _ => json!({}),
                    };
                    let payload = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = stream.get_mut().write_all(response.as_bytes()).await;
                    let _ = stream.get_mut().shutdown().await;
                });
            }
        });
        Self { url }
    }
}

/// Given a live session whose turn holds the agent tool table's read guard
/// across an await (exactly what the turn dispatch in `message_handlers`
/// does for the whole turn), when several MCP registrations contend for the
/// tool table behind that guard, Then the registrations must keep yielding
/// their workers instead of blocking them — the runtime may not deadlock.
/// The production shape: N servers' `Connected` transitions each parked a
/// worker inside `tool_manager.write()` until every worker was pinned and
/// the whole `mirri web` runtime (HTTP, WebSocket, timers) froze.
///
/// The collision runs on a dedicated single-threaded runtime in its own OS
/// thread so a wedged runtime cannot take the test's own timers down with
/// it — the outer test observes completion through a channel timeout.
// The read guard is deliberately held across an await (the turn shape
// under test) — the production pattern, not a bug to fix in the test.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn should_not_deadlock_when_registrations_collide_with_a_turns_tool_table_read_guard() {
    let mock = PlainMcpMock::start().await;
    let (home, workspace) = layered_mcp_fixture(
        json!({}),
        Some(json!({
            "mocksrv": { "transport": "http", "url": mock.url }
        })),
    );
    let server = EmbeddedServer::for_workspace_root(
        WorkspaceId::new("t8-lock-collision"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    );
    let manager = Arc::clone(server.mcp_manager().expect("workspace manager attached"));

    // Lazy connect → Connected with one tool.
    server
        .call_method("mcp.list", json!({}))
        .await
        .expect("first call");
    wait_for_pool_state(&manager, "mocksrv", "Connected", |state| {
        matches!(state, Some(mirri_mcp::McpConnectionState::Connected { .. }))
    })
    .await;

    let created = server
        .call_method("session.create", json!({ "title": "t8-lock-collision" }))
        .await
        .expect("session.create");
    let session_id = created["id"].as_str().expect("session id").to_string();
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let agent_ids = session.list_agents().await;
    let agent = session
        .get_agent(agent_ids.first().expect("the main agent exists"))
        .await
        .expect("the main agent is live");
    let tool_manager = agent.tool_manager();

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let session_manager = server.session_manager();
    let manager = Arc::clone(&manager);
    let tool_manager = Arc::clone(&tool_manager);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("collision runtime");
        rt.block_on(async move {
            // The turn shape: the read guard is held across an await (the
            // turn dispatch holds it for the entire turn). Deliberate —
            // this is exactly the production pattern under test, not a bug
            // to fix here (the function-level allow silences the lint).
            let _turn_guard = tool_manager.read();
            let mut joins = Vec::new();
            for _ in 0..12 {
                let manager = Arc::clone(&manager);
                let session_manager = Arc::clone(&session_manager);
                joins.push(tokio::spawn(async move {
                    super::super::register_server_tools_to_sessions(
                        &manager,
                        &[session_manager],
                        "mocksrv",
                    )
                    .await
                }));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(_turn_guard);
            for join in joins {
                let _ = join.await;
            }
            tx.send(()).ok();
        });
    });

    let completed = rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(
        completed.is_ok(),
        "registration must complete: a blocking tool-table write under a \
         turn-held read guard pins the worker and deadlocks the runtime"
    );
}
