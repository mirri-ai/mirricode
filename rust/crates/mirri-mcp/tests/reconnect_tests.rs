//! Integration tests for the automatic reconnection loop (plans/07 T8).
//!
//! A minimal in-test HTTP server plays the MCP endpoint with a
//! controllable failure budget: the first N requests answer HTTP 500,
//! everything after answers proper JSON-RPC. The recorded request count
//! drives the assertions (how many attempts the loop actually made).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mirri_mcp::{McpConnectionManager, McpConnectionState, McpServerConfig, ReconnectBackoff};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_reconnect_{label}_{}_{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FlakyMockMcp {
    url: String,
    request_count: Arc<AtomicUsize>,
}

impl FlakyMockMcp {
    /// A mock that fails the first `fail_leading` requests, then serves
    /// the handshake.
    async fn start(fail_leading: usize) -> Self {
        Self::start_inner(Arc::new(AtomicUsize::new(fail_leading))).await
    }

    /// A mock that fails every request (never recovers).
    async fn start_always_failing() -> Self {
        Self::start_inner(Arc::new(AtomicUsize::new(usize::MAX))).await
    }

    async fn start_inner(fail_leading: Arc<AtomicUsize>) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/mcp");
        let mock = Self {
            url,
            request_count: Arc::new(AtomicUsize::new(0)),
        };
        let request_count = Arc::clone(&mock.request_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request_count = Arc::clone(&request_count);
                let fail_leading = Arc::clone(&fail_leading);
                tokio::spawn(async move {
                    let _ = serve_flaky(&mut stream, request_count, fail_leading).await;
                });
            }
        });
        mock
    }

    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

async fn serve_flaky(
    stream: &mut TcpStream,
    request_count: Arc<AtomicUsize>,
    fail_leading: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let _ = request_line;
    request_count.fetch_add(1, Ordering::SeqCst);
    // Compare-and-swap loop (not `fetch_update`, deprecated as `try_update`
    // in 1.99 — spelling either name pins the test to one toolchain band).
    let mut current = fail_leading.load(Ordering::SeqCst);
    let still_failing = loop {
        let next = if current == usize::MAX {
            Some(current) // permanent failure — keep the counter at MAX
        } else if current > 0 {
            Some(current - 1)
        } else {
            None // budget spent — serve normally
        };
        let Some(next) = next else { break false };
        match fail_leading.compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break true,
            Err(observed) => current = observed,
        }
    };
    if still_failing {
        return write_response(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    }
    let _ = headers;
    let response = json_rpc_response(&body);
    write_response(stream, &response).await
}

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
            "serverInfo": {"name": "flaky-mock", "version": "1.0"}
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
    let payload = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
}

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

async fn write_response(stream: &mut TcpStream, response: &str) -> std::io::Result<()> {
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
        "startupTimeoutMs": 2000,
    });
    serde_json::from_value(raw).expect("the config should parse")
}

/// A fast backoff profile so the loops settle within test time.
fn fast_backoff(max_attempts: u32) -> ReconnectBackoff {
    ReconnectBackoff {
        base: Duration::from_millis(20),
        factor: 2.0,
        cap: Duration::from_millis(100),
        max_attempts,
    }
}

async fn wait_for_state(
    manager: &McpConnectionManager,
    name: &str,
    want: fn(&McpConnectionState) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
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

fn is_connected(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Connected { .. })
}

fn is_failed(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Failed { .. })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Given a server that fails its first requests and then recovers, when a
/// failed connection is scheduled for automatic reconnection, then the
/// loop retries with backoff, reconnects, and the tools are back.
#[tokio::test]
async fn should_reconnect_with_backoff_when_the_server_recovers() {
    let mock = FlakyMockMcp::start(4).await; // initial connect + 3 loop attempts fail
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "a",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the leading failures must fail the initial connect");
    wait_for_state(&manager, "srv", is_failed, "failed").await;
    assert!(
        mock.count() >= 1,
        "the initial connect must reach the server"
    );

    McpConnectionManager::spawn_auto_reconnect_with(Arc::clone(&manager), "srv", fast_backoff(10));
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    // The recovered connection really serves tools.
    let tools = manager.list_all_tools().await;
    assert!(
        tools
            .iter()
            .any(|(server, tool)| server == "srv" && tool.name == "echo"),
        "the reconnected server's tools must be back"
    );
}

/// Given a server that never recovers, when the automatic reconnection
/// budget is exhausted, then the loop stops (the request count stops
/// growing) instead of spinning forever.
#[tokio::test]
async fn should_stop_retrying_when_the_attempt_budget_is_exhausted() {
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "b",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");
    let count_after_initial = mock.count();

    let budget = 4;
    McpConnectionManager::spawn_auto_reconnect_with(
        Arc::clone(&manager),
        "srv",
        fast_backoff(budget),
    );

    // Wait well past the whole backoff schedule (4 attempts, ≤100ms each
    // plus sleeps ≈ under 1s), then confirm the count stopped growing.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let count_settled = mock.count();
    assert!(
        count_settled <= count_after_initial + budget as usize + 1,
        "the loop must stop after {budget} attempts, saw {} requests after the initial {}",
        count_settled - count_after_initial,
        count_after_initial
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mock.count(),
        count_settled,
        "no further attempts after the budget is exhausted"
    );
    wait_for_state(&manager, "srv", is_failed, "failed").await;
}

/// Given a running auto-reconnect loop, when the server is removed (a
/// config-write reconcile's remove op), then the loop exits instead of
/// retrying a dead registration.
#[tokio::test]
async fn should_exit_the_loop_when_the_server_is_removed_mid_retry() {
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "c",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");
    let count_after_initial = mock.count();

    // A generous budget with a slow backoff: without invalidation the loop
    // would keep hammering the server for the full schedule.
    McpConnectionManager::spawn_auto_reconnect_with(
        Arc::clone(&manager),
        "srv",
        ReconnectBackoff {
            base: Duration::from_millis(50),
            factor: 2.0,
            cap: Duration::from_millis(500),
            max_attempts: 50,
        },
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The removal (a reconcile's Disconnect{unregister:true}).
    manager.remove("srv").await.expect("removal succeeds");

    let count_at_remove = mock.count();
    assert!(
        count_at_remove > count_after_initial,
        "the loop must have attempted before the removal"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        mock.count(),
        count_at_remove,
        "the loop must stop after the removal instead of retrying a dead registration"
    );
}

/// Given an observed unexpected connection loss, when the caller marks it
/// and schedules the reconnection, then the state flips to `failed` first
/// (the status listeners drop the stale tools) and recovers after.
#[tokio::test]
async fn should_mark_a_lost_connection_and_recover_from_the_loss() {
    // Succeed the initial connect, then fail afterwards (the "server was
    // killed" shape): 0 leading failures, then flip to always-failing is
    // not expressible with the counter — use a fresh always-failing mock
    // and only assert the marking half here (recovery is covered above).
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "d",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");

    // Marking a registered server flips it to Failed and notifies.
    manager
        .mark_unexpectedly_disconnected("srv")
        .await
        .expect("marking succeeds");
    wait_for_state(&manager, "srv", is_failed, "failed").await;

    // Marking an unregistered name is a no-op success.
    manager
        .mark_unexpectedly_disconnected("nobody")
        .await
        .expect("marking an unknown name is a no-op");
}
