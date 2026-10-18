//! Stdio transport for ACP — reads JSON-RPC frames from stdin, writes to stdout.
//!
//! Each frame is a single line of JSON (newline-delimited JSON / NDJSON).
//!
//! Besides the server role (relaying the host's notifications and answering
//! client requests), the channel carries the *client* role of the reverse
//! RPC: [`JsonRpcChannel::request`] sends a JSON-RPC request (with an id)
//! to the peer, parks it in the pending-request table (id → oneshot sender),
//! and resolves when the matching response arrives — or fails cleanly on
//! timeout (entry removed) or disconnect (every pending entry failed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use async_trait::async_trait;
use mirri_transport::frame::JsonRpcFrame;
use mirri_transport::frame::RequestId;
use mirri_transport::frame::{JsonRpcRequest, JsonRpcResponse};
use mirri_transport::transport::{Transport, TransportError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A newline-delimited JSON-RPC channel over any reader/writer pair, with a
/// pending-request table for the requests this side issues to the peer.
///
/// `recv` intercepts response frames that match a pending request and
/// completes the parked oneshot instead of surfacing the frame; unmatched
/// responses (protocol violations from the peer) still surface to the
/// caller. Reading EOF fails every pending request, so an in-flight
/// `session/request_permission` resolves to an error — never a hang.
pub struct JsonRpcChannel<R, W> {
    /// The inbound frame stream (one JSON object per line).
    reader: tokio::sync::Mutex<R>,
    /// The outbound frame stream.
    writer: tokio::sync::Mutex<W>,
    /// Whether the channel is still usable.
    connected: AtomicBool,
    /// Id mint for outbound requests (starts at 1; 0 is reserved for the
    /// parse-error response's null-ish id and never issued).
    next_request_id: AtomicI64,
    /// The pending-request table: request id → the oneshot that resolves
    /// the awaiting [`JsonRpcChannel::request`] caller.
    pending: std::sync::Mutex<
        HashMap<i64, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>,
    >,
}

impl<R, W> JsonRpcChannel<R, W>
where
    R: tokio::io::AsyncBufRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: tokio::sync::Mutex::new(reader),
            writer: tokio::sync::Mutex::new(writer),
            connected: AtomicBool::new(true),
            next_request_id: AtomicI64::new(1),
            pending: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Send a JSON-RPC request to the peer and await its response.
    ///
    /// The request is parked in the pending table under its id; the next
    /// `recv` that observes the matching response resolves it. The wait is
    /// bounded by `timeout` (the timed-out entry is removed so a late
    /// response finds no parked request) and by the connection's lifetime
    /// (a disconnect fails every pending request).
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, reply);
        let frame = JsonRpcFrame::Request(JsonRpcRequest::new(Some(id), method, params));
        if let Err(error) = self.send(frame).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(format!("failed to send {method} request: {error}"));
        }
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            // The parked sender was dropped by a disconnect/close cleanup.
            Ok(Err(_recv_error)) => Err(format!("{method} request failed: client disconnected")),
            Err(_elapsed) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(format!(
                    "{method} request timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    /// How many requests are currently parked in the pending table.
    #[cfg(test)]
    fn pending_request_count(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Fail every parked request (disconnect / close) — the awaiting
    /// `request` callers resolve to an error instead of hanging.
    fn fail_pending(&self, reason: &str) {
        for (_, reply) in self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            let _ = reply.send(Err(reason.to_string()));
        }
    }

    /// Complete (or fail) the parked request matching a response frame.
    ///
    /// Only `Number` ids can match: this side's [`JsonRpcChannel::request`]
    /// mints integers, so a peer answering with a string id is answering a
    /// request we never made.
    fn resolve_pending(&self, response: &JsonRpcResponse) -> bool {
        let Some(RequestId::Number(id)) = response.id else {
            return false;
        };
        let reply = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        match reply {
            Some(reply) => {
                let result = match &response.error {
                    Some(error) => Err(format!(
                        "the client answered the request with an error: {} {}",
                        error.code, error.message
                    )),
                    None => Ok(response.result.clone().unwrap_or(serde_json::Value::Null)),
                };
                let _ = reply.send(result);
                true
            }
            None => false,
        }
    }
}

#[async_trait]
impl<R, W> Transport for JsonRpcChannel<R, W>
where
    R: tokio::io::AsyncBufRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    async fn send(&self, frame: JsonRpcFrame) -> Result<(), TransportError> {
        let value = frame.to_value();
        let mut line =
            serde_json::to_string(&value).map_err(|e| TransportError::Send(e.to_string()))?;
        line.push('\n');

        let mut writer = self.writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(TransportError::Io)?;
        writer.flush().await.map_err(TransportError::Io)?;
        Ok(())
    }

    async fn recv(&self) -> Result<JsonRpcFrame, TransportError> {
        loop {
            let mut line = String::new();
            {
                // The lock guard must be dropped before `continue`: tokio
                // mutexes are not reentrant, so skipping a blank line while
                // still holding the guard would deadlock the next read.
                let mut reader = self.reader.lock().await;
                let n = reader
                    .read_line(&mut line)
                    .await
                    .map_err(TransportError::Io)?;
                if n == 0 {
                    self.connected
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    self.fail_pending("client disconnected");
                    return Err(TransportError::Closed);
                }
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_str(trimmed).map_err(|e| TransportError::Parse(e.to_string()))?;
            let frame = JsonRpcFrame::from_value(value)
                .map_err(|e| TransportError::Parse(e.to_string()))?;
            // A response that matches a parked request resolves it here and
            // never surfaces to the caller; unmatched responses still do
            // (the serve loop reports them as protocol violations).
            if let JsonRpcFrame::Response(response) = &frame
                && self.resolve_pending(response)
            {
                continue;
            }
            return Ok(frame);
        }
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.connected
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.fail_pending("channel closed");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// A stdio transport that reads from stdin and writes to stdout.
pub struct StdioTransport {
    channel: JsonRpcChannel<BufReader<tokio::io::Stdin>, tokio::io::Stdout>,
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            channel: JsonRpcChannel::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout()),
        }
    }

    /// Send a JSON-RPC request to the client and await its response (see
    /// [`JsonRpcChannel::request`]) — the reverse-RPC face the
    /// `session/request_permission` bridge rides on.
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        self.channel.request(method, params, timeout).await
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&self, frame: JsonRpcFrame) -> Result<(), TransportError> {
        self.channel.send(frame).await
    }

    async fn recv(&self) -> Result<JsonRpcFrame, TransportError> {
        self.channel.recv().await
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.channel.close().await
    }

    fn is_connected(&self) -> bool {
        self.channel.is_connected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_transport::frame::JsonRpcFrame;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    /// A `JsonRpcChannel` over one end of an in-memory duplex pipe, plus
    /// the peer end for the test to drive as the "client".
    fn duplex_channel() -> (
        JsonRpcChannel<
            BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
        >,
        tokio::io::DuplexStream,
    ) {
        let (ours, peer) = duplex(4096);
        let (reader, writer) = tokio::io::split(ours);
        (JsonRpcChannel::new(BufReader::new(reader), writer), peer)
    }

    /// Drive `channel.recv()` until it errors, so inbound responses can
    /// resolve parked requests. Returns the error that ended the loop.
    async fn pump_recv<R, W>(channel: &JsonRpcChannel<R, W>) -> Result<JsonRpcFrame, TransportError>
    where
        R: tokio::io::AsyncBufRead + Unpin + Send,
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        loop {
            match channel.recv().await {
                Ok(_frame) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    /// Given a tool call awaiting approval, when the client answers the
    /// parked request, then `request` resolves with the result value and
    /// the pending table is empty again.
    #[tokio::test]
    async fn should_resolve_request_when_peer_answers() {
        let (channel, peer) = duplex_channel();
        let peer = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(peer);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("peer reads a line");
            let frame: Value = serde_json::from_str(&line).expect("request is valid JSON");
            assert_eq!(frame["method"], "session/request_permission");
            assert_eq!(frame["jsonrpc"], "2.0");
            let id = frame["id"].as_i64().expect("request carries an id");
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"outcome": {"outcome": "selected", "optionId": "approve_once"}},
            });
            writer
                .write_all(format!("{}\n", response).as_bytes())
                .await
                .expect("peer writes the response");
        });

        let (result, _) = tokio::join!(
            channel.request(
                "session/request_permission",
                json!({"sessionId": "s1"}),
                Duration::from_secs(5),
            ),
            async { pump_recv(&channel).await },
        );
        peer.await.expect("peer task finishes");
        let result = result.expect("the parked request resolves");
        assert_eq!(
            result["outcome"]["optionId"], "approve_once",
            "the client's result value crosses back verbatim: {result}"
        );
        assert_eq!(
            channel.pending_request_count(),
            0,
            "the answered request must leave the pending table"
        );
    }

    /// Given two requests parked at once (a permission ask and an
    /// elicitation, say, in flight on the same channel), when the peer
    /// answers each by its own id — in either order — then each `request`
    /// resolves with its own answer (the ids never collide; the pending
    /// table keys them apart) and the table empties again (F-P5b-5: the
    /// reverse-RPC bridge multiplexes concurrent client requests).
    #[tokio::test]
    async fn should_resolve_concurrent_parked_requests_by_their_own_ids() {
        let (channel, peer) = duplex_channel();
        let peer = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(peer);
            let mut reader = BufReader::new(reader);
            let mut ids = Vec::new();
            // Both request lines arrive (in send order).
            for _ in 0..2 {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("peer reads a line");
                let frame: Value = serde_json::from_str(&line).expect("request is valid JSON");
                assert_eq!(frame["method"], "session/request_permission");
                ids.push(frame["id"].as_i64().expect("request carries an id"));
            }
            assert_ne!(ids[0], ids[1], "concurrent requests get distinct ids");
            // Answer in reverse order: the second-parked request first —
            // out-of-order replies must still find their caller.
            for (index, id) in ids.iter().enumerate().rev() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"outcome": {"outcome": "selected", "optionId": format!("approve_{index}")}},
                });
                writer
                    .write_all(format!("{}\n", response).as_bytes())
                    .await
                    .expect("peer writes the response");
            }
        });

        let (first, second, _) = tokio::join!(
            channel.request(
                "session/request_permission",
                json!({"sessionId": "s1"}),
                Duration::from_secs(5),
            ),
            channel.request(
                "session/request_permission",
                json!({"sessionId": "s2"}),
                Duration::from_secs(5),
            ),
            async { pump_recv(&channel).await },
        );
        peer.await.expect("peer task finishes");
        let first = first.expect("the first parked request resolves");
        let second = second.expect("the second parked request resolves");
        assert_eq!(
            first["outcome"]["optionId"], "approve_0",
            "each caller gets its own answer: {first}"
        );
        assert_eq!(
            second["outcome"]["optionId"], "approve_1",
            "each caller gets its own answer: {second}"
        );
        assert_eq!(
            channel.pending_request_count(),
            0,
            "both answered requests must leave the pending table"
        );
    }

    /// Given a client request whose id is a string (the id shape ACP
    /// clients like Zed mint — a UUID string on every request), when the
    /// channel receives it, then it surfaces as a request frame carrying
    /// that id — never as a parse error.
    #[tokio::test]
    async fn should_surface_string_id_requests_as_requests_not_parse_errors() {
        let (channel, mut peer) = duplex_channel();
        let request = json!({
            "jsonrpc": "2.0",
            "id": "9b0f0d64-41a7-4b6e-9a4e-2d2f6c8b1f01",
            "method": "initialize",
            "params": {},
        });
        peer.write_all(format!("{request}\n").as_bytes())
            .await
            .expect("peer writes the request");

        let frame = channel
            .recv()
            .await
            .expect("a string-id request must parse, not error");
        let JsonRpcFrame::Request(request) = frame else {
            panic!("expected a request frame, got {frame:?}");
        };
        assert_eq!(request.method, "initialize");
    }

    /// Given a client that answers with a JSON-RPC error, when the parked
    /// request resolves, then `request` fails with the error's message.
    #[tokio::test]
    async fn should_fail_request_when_peer_answers_with_error() {
        let (channel, peer) = duplex_channel();
        let peer = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(peer);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("peer reads a line");
            let frame: Value = serde_json::from_str(&line).expect("request is valid JSON");
            let id = frame["id"].as_i64().expect("request carries an id");
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": "client refused"},
            });
            writer
                .write_all(format!("{}\n", response).as_bytes())
                .await
                .expect("peer writes the error response");
        });

        let (result, _) = tokio::join!(
            channel.request(
                "session/request_permission",
                json!({}),
                Duration::from_secs(5)
            ),
            async { pump_recv(&channel).await },
        );
        peer.await.expect("peer task finishes");
        let error = result.expect_err("an error response fails the request");
        assert!(
            error.contains("client refused"),
            "the failure carries the client's message: {error}"
        );
    }

    /// Given a client that never answers, when the request's timeout
    /// elapses, then `request` fails and the pending entry is cleaned up
    /// (a late response finds no parked request).
    #[tokio::test]
    async fn should_clean_pending_entry_when_request_times_out() {
        let (channel, _peer) = duplex_channel();

        let error = channel
            .request(
                "session/request_permission",
                json!({}),
                Duration::from_millis(50),
            )
            .await
            .expect_err("a silent client times the request out");

        assert!(
            error.contains("timed out"),
            "the failure names the timeout: {error}"
        );
        assert_eq!(
            channel.pending_request_count(),
            0,
            "the timed-out request must be removed from the pending table"
        );
    }

    /// Given a client that disconnects while a request is parked, when the
    /// channel reads EOF, then every pending request fails and the table is
    /// drained.
    #[tokio::test]
    async fn should_fail_pending_requests_when_peer_disconnects() {
        let (channel, peer) = duplex_channel();
        // The peer reads the request, then goes away without answering —
        // the parked request must fail through the disconnect cleanup, not
        // a broken-pipe send.
        tokio::spawn(async move {
            let (reader, _writer) = tokio::io::split(peer);
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("peer reads a line");
            drop(reader);
        });

        let (result, recv) = tokio::join!(
            channel.request(
                "session/request_permission",
                json!({}),
                Duration::from_secs(5)
            ),
            pump_recv(&channel),
        );
        assert!(recv.is_err(), "EOF surfaces as a transport error");
        let error = result.expect_err("a disconnected client fails the request");
        assert!(
            error.contains("disconnected"),
            "the failure names the disconnect: {error}"
        );
        assert_eq!(
            channel.pending_request_count(),
            0,
            "the disconnect must drain the pending table"
        );
    }

    /// Given a parked request, when the channel is closed, then the pending
    /// entry is failed (the awaiting caller never hangs) and the table is
    /// drained — the clean-shutdown counterpart of the disconnect cleanup.
    #[tokio::test]
    async fn should_fail_pending_requests_when_the_channel_closes() {
        let (channel, _peer) = duplex_channel();
        let (early_reply, early_rx) =
            tokio::sync::oneshot::channel::<Result<serde_json::Value, String>>();
        channel
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(7, early_reply);

        channel.close().await.expect("close succeeds");

        let error = early_rx
            .await
            .expect("the parked entry is failed, not dropped")
            .expect_err("closing fails the pending request");
        assert!(
            error.contains("closed"),
            "the failure names the close: {error}"
        );
        assert_eq!(channel.pending_request_count(), 0);
    }

    /// Given a response whose id matches no parked request, when it is
    /// received, then it surfaces to the caller (the serve loop reports it
    /// as a protocol violation) instead of being swallowed.
    #[tokio::test]
    async fn should_surface_unmatched_responses_to_the_caller() {
        let (channel, mut peer) = duplex_channel();
        let response = json!({"jsonrpc": "2.0", "id": 999, "result": {}});
        peer.write_all(format!("{}\n", response).as_bytes())
            .await
            .expect("peer writes an unmatched response");

        let frame = channel
            .recv()
            .await
            .expect("the unmatched response surfaces");
        match frame {
            JsonRpcFrame::Response(response) => {
                assert_eq!(response.id, Some(RequestId::Number(999)))
            }
            other => panic!("expected a response frame, got {other:?}"),
        }
    }
}
