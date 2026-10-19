use std::collections::{HashMap, VecDeque};
use std::pin::Pin;

use anyhow::Result;
use bytes::Bytes;
use serde_json::Value;
use tokio_stream::Stream;

use crate::provider::StreamEvent;

/// A boxed, pinned byte-stream from a `reqwest::Response`.
///
/// `reqwest::Response::bytes_stream()` yields `Result<Bytes, reqwest::Error>`
/// items; we type-erase it behind `Pin<Box<dyn Stream + Send>>` so that
/// [`SseLineReader`] owns a single concrete field regardless of the underlying
/// hyper/reqwest stream type.
type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

// ---------------------------------------------------------------------------
// SseLineReader — generic SSE line parser
// ---------------------------------------------------------------------------

/// Generic SSE line reader — parses the raw byte stream into individual
/// `data:` payload strings. Provider-agnostic.
///
/// The reader handles the SSE wire format: `event:` type headers are skipped,
/// blank lines (event separators) are skipped, and each `data:` line's payload
/// is returned as a string. The special `data: [DONE]` marker is returned as
/// the raw string `"[DONE]"` so that provider dispatchers can decide how to
/// handle stream termination.
pub struct SseLineReader {
    /// Underlying response body byte stream.
    stream: ByteStream,
    /// Bytes received but not yet terminated by a newline.
    line_buffer: String,
}

impl SseLineReader {
    /// Wrap a `reqwest::Response` body into an SSE line reader.
    ///
    /// The response is assumed to have a successful status; only the body is
    /// consumed here.
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            line_buffer: String::new(),
        }
    }

    /// Returns the next `data:` payload as a string, or `None` at EOF.
    ///
    /// Skips `event:` lines and blank lines. Handles `data: [DONE]` by
    /// returning the raw `"[DONE]"` string.
    pub async fn next_data_line(&mut self) -> Option<Result<String>> {
        use tokio_stream::StreamExt;
        loop {
            // Try to extract a complete line from the buffer first to avoid
            // an unnecessary await when we already have buffered data.
            if let Some(line) = self.take_line() {
                if let Some(data) = process_line(&line) {
                    return Some(Ok(data));
                }
                continue;
            }

            // No complete line available; pull more bytes from the network.
            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.line_buffer
                        .push_str(std::str::from_utf8(&chunk).unwrap_or(""));
                }
                Some(Err(err)) => {
                    return Some(Err(anyhow::anyhow!("error reading SSE stream: {err}")));
                }
                None => {
                    // EOF: flush any trailing partial line, then end.
                    if !self.line_buffer.is_empty() {
                        let remaining = std::mem::take(&mut self.line_buffer);
                        if let Some(data) = process_line(&remaining) {
                            return Some(Ok(data));
                        }
                    }
                    return None;
                }
            }
        }
    }

    /// Pop the next `\n`-terminated line from
    /// [`line_buffer`](Self::line_buffer), if one is present. The trailing
    /// newline (and any `\r`) is consumed but not returned. Returns `None`
    /// when no complete line is buffered.
    fn take_line(&mut self) -> Option<String> {
        let newline = self.line_buffer.find('\n')?;
        let mut line = self.line_buffer.split_off(newline + 1);
        std::mem::swap(&mut line, &mut self.line_buffer);
        // `line` now holds the extracted line including its trailing '\n'.
        // Strip the newline and an optional preceding '\r' (CRLF tolerance).
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        Some(trimmed.to_string())
    }
}

/// Inspect a single SSE wire line and return the `data:` payload if present.
///
/// Returns `None` for blank lines (event separators), `event:` type headers,
/// and any line that is not a `data:` line.
fn process_line(line: &str) -> Option<String> {
    // Blank lines separate SSE events; skip them.
    if line.is_empty() {
        return None;
    }
    // We ignore the `event:` prefix line; dispatchers dispatch on the JSON
    // `type` field inside `data:` instead.
    if line.starts_with("event: ") || line == "event:" {
        return None;
    }
    // Standard "data: " prefix.
    if let Some(data) = line.strip_prefix("data: ") {
        return Some(data.to_string());
    }
    // Tolerate "data:" without a space (e.g. `data:[DONE]`).
    if let Some(data) = line.strip_prefix("data:") {
        return Some(data.trim_start().to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// SseDispatcher — provider-specific event dispatch
// ---------------------------------------------------------------------------

/// Provider-specific SSE event dispatcher.
///
/// Processes one `data:` JSON payload and returns zero or more
/// [`StreamEvent`]s. Each provider implements this to translate its native
/// SSE event format into the normalized [`StreamEvent`] enum.
///
/// The trait is `Send` (the dispatcher must be movable across threads) but
/// not `Sync` — it is only ever accessed from a single streaming task at a
/// time.
pub trait SseDispatcher: Send {
    /// Process one `data:` payload string and return the resulting events.
    fn dispatch(&mut self, data: &str) -> Vec<StreamEvent>;
}

// ---------------------------------------------------------------------------
// SseStream — combines SseLineReader + SseDispatcher
// ---------------------------------------------------------------------------

/// Default data-line idle timeout: an SSE stream that produces no complete
/// `data:` line for this long is considered wedged and the read fails.
///
/// This is deliberately ABOVE typical provider first-token latencies and
/// inter-chunk gaps — including reasoning models whose thinking phase sends
/// only SSE keep-alive comments for minutes — but BELOW the engine's 600 s
/// turn-stall watchdog, so the provider-side read error (retryable) fires
/// first and the watchdog stays a backstop. Byte-level liveness alone cannot
/// detect the wedged shape: an upstream that keeps sending SSE keep-alive
/// comments (`: ping`) resets reqwest's `read_timeout` forever while never
/// delivering a single data line (the 2026-10-19 incident hung for 14+
/// minutes this way, until the user aborted). The 300 s window is the
/// deliberate compromise: reasoning models legitimately think for minutes
/// between data lines, so a tighter window (e.g. 120 s) would kill healthy
/// o-series-style requests into a retry loop that can never succeed.
pub(crate) const SSE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// SSE stream parser that combines a generic line reader with a
/// provider-specific dispatcher.
///
/// Wraps the byte stream of a successful HTTP response and exposes
/// [`SseStream::next_event`], which returns normalized [`StreamEvent`]s until
/// the stream ends (`None`). A small pending-event queue lets one raw SSE
/// event (notably Anthropic's `message_delta`) yield more than one
/// `StreamEvent`.
pub struct SseStream {
    /// Generic SSE line reader that parses raw bytes into `data:` payloads.
    reader: SseLineReader,
    /// Provider-specific dispatcher that translates `data:` payloads into
    /// normalized `StreamEvent`s.
    dispatcher: Box<dyn SseDispatcher>,
    /// Logical events produced by a `data:` payload but not yet returned, in
    /// emission order. Drained before reading more from [`reader`](Self::reader).
    pending: VecDeque<StreamEvent>,
    /// How long `next_event` may wait for the next complete `data:` line
    /// before failing with an idle-timeout error. See [`SSE_IDLE_TIMEOUT`].
    idle_timeout: std::time::Duration,
}

impl SseStream {
    /// Wrap a `reqwest::Response` into an SSE event stream using the given
    /// provider-specific [`SseDispatcher`].
    ///
    /// The response is assumed to have a successful status (callers should
    /// check status before constructing an `SseStream`); only the body is
    /// consumed here. The data-line idle timeout defaults to
    /// [`SSE_IDLE_TIMEOUT`] (tests shorten it via [`Self::with_idle_timeout`]).
    pub fn new(response: reqwest::Response, dispatcher: Box<dyn SseDispatcher>) -> Self {
        Self {
            reader: SseLineReader::new(response),
            dispatcher,
            pending: VecDeque::new(),
            idle_timeout: SSE_IDLE_TIMEOUT,
        }
    }

    /// Override the data-line idle timeout (tests inject short windows; the
    /// default suits production).
    pub fn with_idle_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Pull the next logical event from the SSE stream.
    ///
    /// Returns `None` when the stream is fully consumed (EOF). Drains any
    /// [`pending`](Self::pending) events first so that one `data:` payload
    /// can fan out to several `StreamEvent`s.
    ///
    /// Fails with an idle-timeout error when no complete `data:` line has
    /// arrived within [`Self::idle_timeout`] — a wedged upstream (one that
    /// sends keep-alive comments or nothing at all) surfaces as a read error
    /// instead of hanging the turn forever. The error wording keeps the
    /// `error reading SSE stream:` prefix so the engine's retry
    /// classification sees a transport failure (retryable).
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent>> {
        // Drain queued events first — a single data line may produce several.
        if let Some(event) = self.pending.pop_front() {
            return Some(Ok(event));
        }

        loop {
            let line = match tokio::time::timeout(self.idle_timeout, self.reader.next_data_line())
                .await
            {
                // A data line landed inside the window: reset and continue.
                Ok(Some(Ok(data))) => data,
                // `next_data_line` already wraps body errors with the
                // `error reading SSE stream:` prefix — return as-is.
                Ok(Some(Err(err))) => return Some(Err(err)),
                Ok(None) => return None, // EOF
                Err(_elapsed) => {
                    return Some(Err(anyhow::anyhow!(
                        "error reading SSE stream: SSE idle timeout (no data line for {:?})",
                        self.idle_timeout
                    )));
                }
            };
            let events = self.dispatcher.dispatch(&line);
            let mut iter = events.into_iter();
            if let Some(head) = iter.next() {
                self.pending.extend(iter);
                return Some(Ok(head));
            }
            // No events from this data line; continue reading.
        }
    }
}

// ---------------------------------------------------------------------------
// ContentBlockState — Anthropic content-block accumulation
// ---------------------------------------------------------------------------

/// Per-content-block state used to accumulate tool-call arguments across
/// `input_json_delta` events.
///
/// Anthropic streams tool calls as: a `content_block_start` declaring the
/// tool name and call id, followed by one or more `content_block_delta`
/// events whose `partial_json` fragments must be concatenated, and finally a
/// `content_block_stop` that signals the arguments are complete. We stash
/// the fragments here keyed by the block's `index` and assemble the final
/// `ToolCall` on `content_block_stop`.
#[derive(Default)]
struct ContentBlockState {
    /// Either `"text"`, `"tool_use"`, or `"thinking"`.
    block_type: String,
    /// Tool name, populated only for `tool_use` blocks.
    tool_name: String,
    /// Provider-issued tool call id, populated only for `tool_use` blocks.
    call_id: String,
    /// Concatenated `partial_json` fragments for the tool's arguments.
    accumulated_json: String,
}

// ---------------------------------------------------------------------------
// AnthropicDispatcher — Anthropic SSE event dispatch
// ---------------------------------------------------------------------------

/// Anthropic SSE event dispatcher.
///
/// Handles Anthropic's event types: `message_start`, `content_block_start`,
/// `content_block_delta`, `content_block_stop`, `message_delta`,
/// `message_stop`. Translates them into normalized [`StreamEvent`]s.
#[derive(Default)]
pub struct AnthropicDispatcher {
    /// Active content blocks, keyed by Anthropic's `index` field.
    content_blocks: HashMap<u64, ContentBlockState>,
}

impl SseDispatcher for AnthropicDispatcher {
    fn dispatch(&mut self, data: &str) -> Vec<StreamEvent> {
        let json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        self.dispatch_event(event_type, &json)
    }
}

impl AnthropicDispatcher {
    /// Map an Anthropic SSE event type to its logical [`StreamEvent`]s.
    fn dispatch_event(&mut self, event_type: &str, json: &Value) -> Vec<StreamEvent> {
        match event_type {
            "message_start" => {
                let model = json
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                let usage = json.get("message").and_then(|m| m.get("usage"));
                let input_tokens = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                // Prompt-cache buckets: cache reads (hits) and cache writes
                // (creation) are reported alongside the (non-cached) input
                // tokens in `message_start`'s usage object.
                let cache_read_tokens = usage
                    .and_then(|u| u.get("cache_read_input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                let cache_creation_tokens = usage
                    .and_then(|u| u.get("cache_creation_input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                // Emit MessageStart for completeness and a Usage event for
                // the input-token accounting reported up front.
                vec![
                    StreamEvent::MessageStart { model },
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens: 0,
                        cache_read_tokens,
                        cache_creation_tokens,
                    },
                ]
            }
            "content_block_start" => {
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let block = json.get("content_block").unwrap_or(&Value::Null);
                let block_type = block
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();

                let mut state = ContentBlockState {
                    block_type: block_type.clone(),
                    ..Default::default()
                };
                if block_type == "tool_use" {
                    state.tool_name = block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.call_id = block
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                }
                // For "text" and "thinking" blocks, we record the state but
                // do not need any special initialization here — deltas are
                // emitted directly and no accumulation is required.
                self.content_blocks.insert(index, state);
                Vec::new()
            }
            "content_block_delta" => {
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = json.get("delta").unwrap_or(&Value::Null);
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        vec![StreamEvent::TextDelta { text }]
                    }
                    "thinking_delta" => {
                        let text = delta
                            .get("thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        vec![StreamEvent::ThinkDelta { text }]
                    }
                    "input_json_delta" => {
                        // Accumulate partial JSON for tool calls; emit nothing
                        // until content_block_stop assembles the ToolCall.
                        if let Some(state) = self.content_blocks.get_mut(&index)
                            && let Some(partial) =
                                delta.get("partial_json").and_then(|p| p.as_str())
                        {
                            state.accumulated_json.push_str(partial);
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => {
                let index = json.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if let Some(state) = self.content_blocks.remove(&index)
                    && state.block_type == "tool_use"
                    && !state.tool_name.is_empty()
                {
                    let args: Value = serde_json::from_str::<Value>(&state.accumulated_json)
                        .ok()
                        .filter(|v| v.is_object())
                        // Degrade malformed or non-object arguments to an
                        // empty object — never `null`, which downstream
                        // servers reject on replay.
                        .unwrap_or(Value::Object(serde_json::Map::new()));
                    return vec![StreamEvent::ToolCall {
                        tool: state.tool_name,
                        args,
                        call_id: state.call_id,
                    }];
                }
                Vec::new()
            }
            "message_delta" => {
                let stop_reason = json
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("end_turn")
                    .to_string();
                let usage = json.get("usage");
                let output_tokens = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                // Anthropic may repeat the cumulative prompt-cache buckets in
                // `message_delta`'s usage object; read them so a stream that
                // only reports them there still surfaces them.
                let cache_read_tokens = usage
                    .and_then(|u| u.get("cache_read_input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                let cache_creation_tokens = usage
                    .and_then(|u| u.get("cache_creation_input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                // Emit the final output-token Usage, then the terminal Done.
                let mut events = Vec::with_capacity(2);
                if output_tokens > 0 || cache_read_tokens > 0 || cache_creation_tokens > 0 {
                    events.push(StreamEvent::Usage {
                        input_tokens: 0,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                    });
                }
                events.push(StreamEvent::Done { stop_reason });
                events
            }
            "message_stop" => {
                // Explicit end of stream; next_event() will return None on the
                // next call once pending is drained. Emit nothing here.
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}
