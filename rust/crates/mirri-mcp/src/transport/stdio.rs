//! stdio transport — JSON-RPC over a child process's stdin/stdout.
//!
//! This is the most common MCP transport for local tool servers (e.g.
//! `@modelcontextprotocol/server-filesystem`). The transport:
//!
//! 1. Spawns the child process with inherited + overridden env / cwd
//! 2. Writes JSON-RPC requests to the child's stdin (newline-delimited)
//! 3. Reads JSON-RPC responses from the child's stdout (newline-delimited)
//! 4. Captures stderr into a bounded tail buffer for diagnostics
//!
//! Requests are serialized through a `tokio::sync::Mutex` so only one
//! request is in-flight at a time. A dedicated reader task drains stdout
//! and routes responses to the waiting caller via a oneshot channel keyed
//! by request id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, warn};

use crate::error::{McpError, McpResult};
use crate::transport::{McpTransport, TransportFuture};
use crate::types::{JsonRpcId, JsonRpcResponse};

/// Capacity of the stderr tail buffer (4 KiB, matching the TS implementation).
const STDERR_BUFFER_CAPACITY: usize = 4 * 1024;

/// A bounded tail buffer for stderr — keeps the last `capacity` bytes.
///
/// When the buffer exceeds capacity the oldest bytes are discarded, so the
/// snapshot always contains the most recent output.
struct BoundedTail {
    buffer: String,
    capacity: usize,
}

impl BoundedTail {
    fn new(capacity: usize) -> Self {
        BoundedTail {
            buffer: String::new(),
            capacity,
        }
    }

    fn push(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        if self.buffer.len() > self.capacity {
            let excess = self.buffer.len() - self.capacity;
            // Use char boundaries to avoid splitting a multi-byte char.
            let split_point = self.buffer.ceil_char_boundary(excess);
            self.buffer = self.buffer[split_point..].to_string();
        }
    }

    fn snapshot(&self) -> &str {
        &self.buffer
    }
}

/// Shared state between the transport and the stdout reader task.
struct SharedState {
    /// Pending request id → response channel. The reader task looks up the
    /// id from each response line and completes the matching channel.
    pending: Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>,
    /// stderr tail buffer.
    stderr: Mutex<BoundedTail>,
    /// Set to true when the child process exits or the reader task ends.
    connected: AtomicBool,
}

impl SharedState {
    fn make_key(id: &JsonRpcId) -> String {
        match id {
            JsonRpcId::Num(n) => n.to_string(),
            JsonRpcId::Str(s) => s.clone(),
            JsonRpcId::Null => "null".to_string(),
        }
    }
}

/// stdio transport implementation.
pub struct StdioTransport {
    /// Child process handle; dropped on `close` to kill the process.
    child: tokio::process::Child,
    /// stdin writer; guarded by a mutex so requests are serialized.
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    state: Arc<SharedState>,
    /// Monotonic request id counter.
    #[allow(dead_code)]
    next_id: AtomicU64,
}

impl StdioTransport {
    /// Spawn the child process and start the stdout/stderr reader tasks.
    ///
    /// Returns an error if the process cannot be spawned.
    pub fn new(
        command: String,
        args: Vec<String>,
        env_overrides: Option<HashMap<String, String>>,
        cwd: Option<PathBuf>,
    ) -> McpResult<Self> {
        let mut cmd = Command::new(&command);
        cmd.args(&args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Set cwd if provided.
        if let Some(ref cwd) = cwd {
            cmd.current_dir(cwd);
        }

        // Merge environment: inherit parent env, then apply overrides.
        // On non-Windows platforms `env_clear` + manual inheritance is more
        // explicit, but `Command` inherits by default, so we only need to
        // apply overrides.
        if let Some(ref overrides) = env_overrides {
            for (key, value) in overrides {
                cmd.env(key, value);
            }
        }

        // Kill the child if the parent process exits.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::ConnectionFailed(format!("failed to spawn '{command}': {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpError::Transport("child stderr not captured".into()))?;

        let state = Arc::new(SharedState {
            pending: Mutex::new(HashMap::new()),
            stderr: Mutex::new(BoundedTail::new(STDERR_BUFFER_CAPACITY)),
            connected: AtomicBool::new(true),
        });

        // Spawn stdout reader task.
        {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                read_stdout(stdout, state).await;
            });
        }

        // Spawn stderr reader task.
        {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                read_stderr(stderr, state).await;
            });
        }

        debug!(command = %command, "stdio MCP transport started");

        Ok(StdioTransport {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            state,
            next_id: AtomicU64::new(1),
        })
    }

    /// Allocate the next request id.
    #[allow(dead_code)]
    pub(crate) fn next_request_id(&self) -> JsonRpcId {
        JsonRpcId::Num(self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl McpTransport for StdioTransport {
    fn send_request(
        &mut self,
        request_json: &str,
        timeout: Duration,
    ) -> TransportFuture<'_, JsonRpcResponse> {
        let request_json = request_json.to_string();
        Box::pin(async move {
            if !self.state.connected.load(Ordering::Relaxed) {
                return Err(McpError::Disconnected);
            }

            // Parse the request to extract its id — we need it to match the
            // response.
            let request: crate::types::JsonRpcRequest = serde_json::from_str(&request_json)
                .map_err(|e| McpError::Protocol(format!("invalid request JSON: {e}")))?;

            let key = SharedState::make_key(&request.id);

            // Register the pending response channel.
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = self.state.pending.lock().await;
                pending.insert(key.clone(), tx);
            }

            // Write the request line to stdin.
            {
                let mut stdin = self.stdin.lock().await;
                let line = format!("{request_json}\n");
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| McpError::Transport(format!("failed to write to stdin: {e}")))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| McpError::Transport(format!("failed to flush stdin: {e}")))?;
            }

            // Wait for the response with a timeout.
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => {
                    // The sender was dropped — the reader task ended.
                    self.state.connected.store(false, Ordering::Relaxed);
                    Err(McpError::Disconnected)
                }
                Err(_) => {
                    // Timeout — remove the pending entry so stale responses don't
                    // leak.
                    let mut pending = self.state.pending.lock().await;
                    pending.remove(&key);
                    Err(McpError::Timeout)
                }
            }
        })
    }

    fn send_notification(&mut self, notification_json: &str) -> TransportFuture<'_, ()> {
        let notification_json = notification_json.to_string();
        Box::pin(async move {
            if !self.state.connected.load(Ordering::Relaxed) {
                return Err(McpError::Disconnected);
            }

            // Write the notification line to stdin without registering a
            // pending response — notifications don't have an `id` and the
            // server should not respond.
            let mut stdin = self.stdin.lock().await;
            let line = format!("{notification_json}\n");
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| McpError::Transport(format!("failed to write to stdin: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| McpError::Transport(format!("failed to flush stdin: {e}")))?;

            debug!("sent JSON-RPC notification via stdio");
            Ok(())
        })
    }

    fn stderr_snapshot(&self) -> String {
        // Try to lock non-blockingly; if contended, return what we can.
        // Using try_lock avoids a deadlock if the caller is already holding
        // a lock on a different field.
        match self.state.stderr.try_lock() {
            Ok(guard) => guard.snapshot().to_string(),
            Err(_) => {
                // Lock contended — return empty rather than blocking.
                String::new()
            }
        }
    }

    fn close(&mut self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            self.state.connected.store(false, Ordering::Relaxed);

            // Drop all pending requests so callers get a Disconnected error.
            {
                let mut pending = self.state.pending.lock().await;
                pending.clear();
            }

            // Kill the child process. `kill_on_drop` also handles this, but we
            // do it explicitly for determinism.
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;

            debug!("stdio MCP transport closed");
            Ok(())
        })
    }

    fn is_connected(&self) -> bool {
        self.state.connected.load(Ordering::Relaxed)
    }
}

/// Read stdout line-by-line and route responses to waiting callers.
///
/// This task runs for the lifetime of the transport. When stdout closes
/// (child process exited), it marks the transport as disconnected and
/// drops all pending senders.
async fn read_stdout(stdout: tokio::process::ChildStdout, state: Arc<SharedState>) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }

                // Parse the response to extract the id.
                let response: JsonRpcResponse = match serde_json::from_str(&line) {
                    Ok(resp) => resp,
                    Err(e) => {
                        warn!(error = %e, line = %line, "failed to parse stdout line as JSON-RPC response");
                        continue;
                    }
                };

                let key = SharedState::make_key(&response.id);

                // Find and complete the matching pending request.
                let sender = {
                    let mut pending = state.pending.lock().await;
                    pending.remove(&key)
                };

                match sender {
                    Some(tx) => {
                        let _ = tx.send(response);
                    }
                    None => {
                        // No pending request for this id — could be a
                        // duplicate or a notification. Log and continue.
                        debug!(id = %key, "received response with no pending request");
                    }
                }
            }
            Ok(None) => {
                // stdout closed — child exited.
                debug!("stdio MCP child stdout closed");
                break;
            }
            Err(e) => {
                warn!(error = %e, "error reading stdout from MCP child");
                break;
            }
        }
    }

    // Mark disconnected and wake all pending callers.
    state.connected.store(false, Ordering::Relaxed);
    let mut pending = state.pending.lock().await;
    pending.clear();
}

/// Read stderr and append to the bounded tail buffer.
async fn read_stderr(stderr: tokio::process::ChildStderr, state: Arc<SharedState>) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let mut stderr_buf = state.stderr.lock().await;
        stderr_buf.push(&line);
        stderr_buf.push("\n");
    }
}

// ceil_char_boundary is stabilized in Rust 1.80+, but we provide a fallback
// in case it's not available.
trait CeilCharBoundary {
    fn ceil_char_boundary(&self, idx: usize) -> usize;
}

impl CeilCharBoundary for String {
    fn ceil_char_boundary(&self, idx: usize) -> usize {
        // Try the native method first (stabilized in 1.80).
        // If that fails, fall back to a manual scan.
        if idx >= self.len() {
            return self.len();
        }
        // Walk forward to the next char boundary.
        let bytes = self.as_bytes();
        let mut i = idx;
        while i < bytes.len() && !self.is_char_boundary(i) {
            i += 1;
        }
        i
    }
}
