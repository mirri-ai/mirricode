//! The files REST surface (P4): `POST /api/v1/files` (multipart upload),
//! `GET /api/v1/files/{id}` (binary download), and `DELETE /api/v1/files/{id}`.
//!
//! Wire semantics mirror the v1 TS kap-server `routes/files.ts`:
//!
//! - upload takes `multipart/form-data` with a `file` part plus optional
//!   `name` / `expires_in_sec` fields, and answers the `{code, msg, data,
//!   request_id}` envelope carrying the [`FileMeta`](mirri_embedded::FileMeta)
//!   shape (snake_case, the TS `WireFileMeta`). The body is consumed as a
//!   stream and the payload written straight to the store's blob file —
//!   the upload's memory profile is bounded by the chunk cadence, not the
//!   file size (the TS server's streaming behavior);
//! - download answers the raw bytes with the file's media type,
//!   content-disposition (inline for image/video/audio, attachment
//!   otherwise), and byte-range support (`206 Partial Content`), or a 404
//!   error envelope for an unknown id;
//! - delete removes the store entry and the blob, answering `204 No
//!   Content` on success and the 404 error envelope for an unknown id (the
//!   plans/07 T10 acceptance shape; the TS route answers a `{deleted:
//!   true}` envelope instead).
//!
//! The multipart body is parsed by a hand-written streaming boundary state
//! machine — a single-file, zero-new-dependency implementation that
//! tolerates fields in any order. Delimiters are recognized strictly per
//! RFC 2046: a part boundary is `\r\n--{boundary}` followed by optional
//! transport padding and CRLF, and the closing delimiter is
//! `\r\n--{boundary}--`. A bare `--boundary` byte run inside the payload —
//! or one followed by a non-delimiter suffix — is content, not a
//! delimiter, so binary payloads cannot be truncated by a byte-sequence
//! collision (the F-P4-6 hardening).

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::http::request_id_from;
use crate::rpc::{ApiErrorResponse, ApiResponse, RpcError};
use crate::state::ServerState;
use mirri_persistence::file_store::{FileSink, SaveOptions};

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// The parsed multipart upload: the `file` part's metadata plus its text
/// fields (the payload itself streams into the caller's sink).
#[derive(Debug, Default)]
struct MultipartUpload {
    filename: String,
    mime_type: Option<String>,
    fields: std::collections::HashMap<String, String>,
}

/// `POST /api/v1/files` — store the uploaded file and answer its metadata.
pub(crate) async fn upload_file(
    State(state): State<ServerState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = upload(state, headers, request).await;
    match result {
        Ok(data) => (
            StatusCode::OK,
            axum::Json(
                serde_json::to_value(ApiResponse::ok(data, &request_id)).unwrap_or_default(),
            ),
        )
            .into_response(),
        Err(error) => files_error_response(error, &request_id),
    }
}

/// Drive the streaming multipart parse, piping the file payload into a
/// [`FileSink`] as the chunks arrive.
async fn upload(
    state: ServerState,
    headers: HeaderMap,
    request: Request,
) -> Result<serde_json::Value, RpcError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| invalid("multipart not initialized"))?;
    let boundary = boundary_of(content_type).ok_or_else(|| invalid("multipart not initialized"))?;

    let store = state.file_store();
    let mut sink: Option<FileSink> = None;
    let mut machine = MultipartState::new(&boundary);
    // The event closure is shared by the streaming loop and `finish()`:
    // end-of-body decisions (an EOF-decided close delimiter) flush payload
    // through the same sink as mid-stream ones.
    let mut on_event = |event: MultipartEvent<'_>| -> Result<(), RpcError> {
        match event {
            MultipartEvent::FilePartStarted => {
                if sink.is_some() {
                    return Err(invalid("multiple `file` parts are not allowed"));
                }
                sink = Some(
                    store
                        .begin()
                        .map_err(|e| internal(format!("upload failed: {e}")))?,
                );
                Ok(())
            }
            MultipartEvent::FileData(data) => sink
                .as_mut()
                .expect("file data only arrives inside a file part")
                .write(data)
                .map_err(|e| internal(format!("upload failed: {e}"))),
        }
    };

    let mut stream = request.into_body().into_data_stream();
    use futures::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| invalid(format!("upload body read failed: {e}")))?
            .to_vec();
        machine.push(&chunk, &mut on_event)?;
    }
    let upload = machine.finish(&mut on_event)?;

    let name_override = non_empty_field(&upload.fields, "name");
    let expires_in_sec = upload
        .fields
        .get("expires_in_sec")
        .and_then(|raw| raw.trim().parse::<u64>().ok());

    let sink = sink.expect("a finished parse with saw_file started a sink");
    let meta = sink
        .finish(
            &upload.filename,
            SaveOptions {
                name: name_override,
                mime_type: upload.mime_type,
                expires_in_sec,
            },
        )
        .map_err(|e| internal(format!("upload failed: {e}")))?;
    serde_json::to_value(meta).map_err(|e| internal(format!("serialize meta: {e}")))
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

/// `GET /api/v1/files/{id}` — stream the file's bytes.
pub(crate) async fn get_file(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let Some((meta, data)) = state.file_store().get(&file_id) else {
        return files_error_response(
            RpcError::NotFound("file not found".to_string()),
            &request_id,
        );
    };

    let size = data.len();
    let mut headers_out = axum::http::HeaderMap::new();
    let _ = headers_out.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&meta.media_type)
            .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
    );
    let _ = headers_out.insert(
        header::CONTENT_DISPOSITION,
        build_content_disposition(&meta.name, &meta.media_type),
    );
    let _ = headers_out.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    let _ = headers_out.insert(
        header::ETAG,
        header::HeaderValue::from_str(&format!("\"{}-{size}\"", meta.id))
            .unwrap_or(header::HeaderValue::from_static("\"file\"")),
    );

    // Browsers load <video>/<audio> via byte-range requests; honor a
    // satisfiable Range header with 206 Partial Content (a malformed or
    // unsatisfiable range degrades to the full 200 body, matching the TS
    // server's behavior).
    if let Some(range) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| parse_byte_range(raw, size))
    {
        let _ = headers_out.insert(
            header::CONTENT_RANGE,
            header::HeaderValue::from_str(&format!("bytes {}-{}/{}", range.0, range.1, size))
                .unwrap_or(header::HeaderValue::from_static("bytes */*")),
        );
        let _ = headers_out.insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_str(&format!("{}", range.1 - range.0 + 1))
                .unwrap_or(header::HeaderValue::from_static("0")),
        );
        return (
            StatusCode::PARTIAL_CONTENT,
            headers_out,
            data[range.0..=range.1].to_vec(),
        )
            .into_response();
    }

    let _ = headers_out.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&format!("{size}"))
            .unwrap_or(header::HeaderValue::from_static("0")),
    );
    (StatusCode::OK, headers_out, data).into_response()
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// `DELETE /api/v1/files/{id}` — remove the store entry and the blob file
/// (the TS `IFileService::delete` semantics: both go away together).
pub(crate) async fn delete_file(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    match state.file_store().delete(&file_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => files_error_response(
            RpcError::NotFound("file not found".to_string()),
            &request_id,
        ),
        Err(error) => {
            files_error_response(internal(format!("delete failed: {error}")), &request_id)
        }
    }
}

// ---------------------------------------------------------------------------
// Multipart parsing (streaming boundary state machine)
// ---------------------------------------------------------------------------

/// Cap on one part's header block (a malformed or hostile client must not
/// grow the buffer unboundedly).
const HEADERS_LIMIT: usize = 8 * 1024;

/// Cap on one text field's payload (the fastify default order of magnitude;
/// the file part is uncapped — it streams to the store).
const FIELD_LIMIT: usize = 1024 * 1024;

/// Cap on the transport padding (LWSP run) that may follow a delimiter.
/// RFC 2046 notes implementations may bound it — an unbounded run would
/// otherwise pin the scan buffer and be rescanned on every push (the
/// F-T10-2 DoS vector).
const TRANSPORT_PADDING_LIMIT: usize = 4 * 1024;

/// An event the streaming parser surfaces to its caller.
#[derive(Debug)]
enum MultipartEvent<'a> {
    /// A file part's headers were just consumed — the caller must open a
    /// sink; the payload bytes follow as [`MultipartEvent::FileData`].
    FilePartStarted,
    /// Payload bytes of the current file part (in order, no reordering).
    FileData(&'a [u8]),
}

/// The streaming multipart parser state.
#[derive(Debug)]
enum ParseState {
    /// Waiting for the opening `--{boundary}` (the preamble must be empty).
    Opening,
    /// Reading one part's header block up to `\r\n\r\n`.
    Headers,
    /// Reading one part's payload, watching for the next delimiter. `file`
    /// marks the file part (payload streamed out) versus a text field
    /// (payload accumulated).
    Payload { file: bool },
    /// The closing delimiter was seen; the epilogue is ignored.
    Done,
}

/// An incremental `multipart/form-data` parser over arbitrary chunk
/// boundaries.
///
/// The parser is pure byte-shuffling — no IO — so the unit tests can drive
/// it with one-byte chunks to pin the chunk-boundary hold-back behavior.
/// The file payload is surfaced through [`MultipartEvent::FileData`] as it
/// arrives, so the caller never holds the whole upload in memory.
#[derive(Debug)]
struct MultipartState {
    /// The part-delimiter needle: `\r\n--{boundary}`.
    needle: Vec<u8>,
    /// The body-start form: `--{boundary}`.
    dash_boundary: Vec<u8>,
    state: ParseState,
    upload: MultipartUpload,
    saw_file: bool,
    /// Unconsumed bytes (payload hold-back at chunk boundaries, or the
    /// current header block / field payload).
    buf: Vec<u8>,
    /// The text field being read (its payload accumulates in `field_data`).
    current_field: Option<String>,
    /// The accumulated payload of the current text field.
    field_data: Vec<u8>,
}

impl MultipartState {
    fn new(boundary: &str) -> Self {
        Self {
            needle: format!("\r\n--{boundary}").into_bytes(),
            dash_boundary: format!("--{boundary}").into_bytes(),
            state: ParseState::Opening,
            upload: MultipartUpload::default(),
            saw_file: false,
            buf: Vec::new(),
            current_field: None,
            field_data: Vec::new(),
        }
    }

    /// Feed one body chunk; file payload bytes surface through `on_event`.
    fn push(
        &mut self,
        chunk: &[u8],
        on_event: &mut dyn FnMut(MultipartEvent<'_>) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        self.buf.extend_from_slice(chunk);
        self.process(false, on_event)
    }

    /// Seal the parse at end-of-body and return the collected upload. The
    /// caller's event closure must be passed in: a close/part delimiter can
    /// only be decided at EOF (e.g. a body ending `--boundary--` with no
    /// trailing CRLF — RFC 2046 legal), and the payload bytes such a
    /// decision flushes must reach the real sink, not be dropped (F-T10-1).
    fn finish(
        mut self,
        on_event: &mut dyn FnMut(MultipartEvent<'_>) -> Result<(), RpcError>,
    ) -> Result<MultipartUpload, RpcError> {
        self.process(true, on_event)?;
        // A file part truncated without its terminating delimiter is a
        // malformed body (the payload never completed); a truncated tail
        // field/header part is dropped silently — both match the previous
        // parser's behavior.
        match self.state {
            ParseState::Done | ParseState::Headers | ParseState::Payload { file: false } => {}
            ParseState::Opening | ParseState::Payload { file: true } => {
                return Err(invalid("malformed multipart body"));
            }
        }
        if !self.saw_file {
            return Err(invalid("missing `file` field"));
        }
        Ok(self.upload)
    }

    /// Run the state machine until the buffer runs out of decidable work.
    fn process(
        &mut self,
        at_eof: bool,
        on_event: &mut dyn FnMut(MultipartEvent<'_>) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        loop {
            match self.state {
                ParseState::Done => {
                    // The epilogue after the closing delimiter is ignored.
                    self.buf.clear();
                    return Ok(());
                }
                ParseState::Opening => {
                    if self.buf.len() < self.dash_boundary.len() {
                        if at_eof {
                            return Err(invalid("malformed multipart body"));
                        }
                        return Ok(());
                    }
                    if !self.buf.starts_with(&self.dash_boundary) {
                        return Err(invalid("malformed multipart body"));
                    }
                    match classify_suffix(&self.buf[self.dash_boundary.len()..], at_eof) {
                        (Suffix::NeedMore, _) => return Ok(()),
                        (Suffix::Part, consumed) => {
                            let through = self.dash_boundary.len() + consumed;
                            self.buf.drain(..through);
                            self.state = ParseState::Headers;
                        }
                        (Suffix::Close, _) => {
                            self.state = ParseState::Done;
                        }
                        (Suffix::PaddingTooLong, _) => {
                            return Err(invalid("multipart transport padding too large"));
                        }
                        (Suffix::Content, _) => {
                            return Err(invalid("malformed multipart body"));
                        }
                    }
                }
                ParseState::Headers => {
                    let Some(position) = find_from(&self.buf, 0, b"\r\n\r\n") else {
                        if self.buf.len() > HEADERS_LIMIT {
                            return Err(invalid("multipart part headers too large"));
                        }
                        if at_eof {
                            // A truncated tail part is dropped (the previous
                            // parser's lenient behavior).
                            self.state = ParseState::Done;
                            continue;
                        }
                        return Ok(());
                    };
                    if position > HEADERS_LIMIT {
                        return Err(invalid("multipart part headers too large"));
                    }
                    let headers = self.buf[..position].to_vec();
                    let consumed = position + 4;
                    let part = parse_part_headers(&headers);
                    self.buf.drain(..consumed);
                    match part {
                        Some(PartHeaders {
                            name: _,
                            filename: Some(filename),
                            mime_type,
                        }) if !filename.is_empty() => {
                            if self.saw_file {
                                return Err(invalid("multiple `file` parts are not allowed"));
                            }
                            self.saw_file = true;
                            self.upload.filename = filename.clone();
                            self.upload.mime_type = mime_type;
                            self.state = ParseState::Payload { file: true };
                            on_event(MultipartEvent::FilePartStarted)?;
                        }
                        Some(PartHeaders {
                            name: Some(field), ..
                        }) => {
                            self.current_field = Some(field);
                            self.field_data.clear();
                            self.state = ParseState::Payload { file: false };
                        }
                        _ => {
                            // A part without a name contributes nothing.
                            self.field_data.clear();
                            self.state = ParseState::Payload { file: false };
                        }
                    }
                }
                ParseState::Payload { file } => match self.scan_delimiter(at_eof) {
                    Scan::NeedMore => return Ok(()),
                    Scan::PaddingTooLong => {
                        return Err(invalid("multipart transport padding too large"));
                    }
                    Scan::Content(len) => {
                        if file {
                            on_event(MultipartEvent::FileData(&self.buf[..len]))?;
                        } else {
                            self.field_data.extend_from_slice(&self.buf[..len]);
                            if self.field_data.len() > FIELD_LIMIT {
                                return Err(invalid("multipart field too large"));
                            }
                        }
                        self.buf.drain(..len);
                        if len == 0 {
                            // Nothing was flushed — the whole buffer is
                            // hold-back (or empty at EOF); no progress is
                            // possible until more bytes arrive.
                            return Ok(());
                        }
                    }
                    Scan::Part(end) => {
                        self.end_payload(end, file, on_event)?;
                        let consumed = self.needle.len()
                            + classify_suffix(&self.buf[end + self.needle.len()..], at_eof).1;
                        self.buf.drain(..end + consumed);
                        self.state = ParseState::Headers;
                    }
                    Scan::Close(end) => {
                        self.end_payload(end, file, on_event)?;
                        self.state = ParseState::Done;
                    }
                },
            }
        }
    }

    /// Emit (file) or record (field) the payload ending at byte `end`,
    /// then advance past the delimiter itself.
    fn end_payload(
        &mut self,
        end: usize,
        file: bool,
        on_event: &mut dyn FnMut(MultipartEvent<'_>) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        if file {
            on_event(MultipartEvent::FileData(&self.buf[..end]))?;
        } else if let Some(field) = self.current_field.take() {
            self.field_data.extend_from_slice(&self.buf[..end]);
            if self.field_data.len() > FIELD_LIMIT {
                return Err(invalid("multipart field too large"));
            }
            let value = String::from_utf8_lossy(&self.field_data).to_string();
            self.upload.fields.insert(field, value);
            self.field_data.clear();
        }
        Ok(())
    }

    /// Find the next decidable delimiter in the buffer. `NeedMore` means
    /// the trailing bytes could still grow into (or complete) a delimiter,
    /// so nothing may be flushed yet; `Content(len)` means the first `len`
    /// bytes are payload with the rest held back for the next round.
    fn scan_delimiter(&self, at_eof: bool) -> Scan {
        let needle = &self.needle;
        let mut search = 0;
        while let Some(position) = find_from(&self.buf, search, needle) {
            let after = &self.buf[position + needle.len()..];
            match classify_suffix(after, at_eof).0 {
                Suffix::NeedMore => return Scan::NeedMore,
                Suffix::Close => return Scan::Close(position),
                Suffix::Part => return Scan::Part(position),
                Suffix::Content => search = position + 1,
                Suffix::PaddingTooLong => return Scan::PaddingTooLong,
            }
        }
        if at_eof {
            Scan::Content(self.buf.len())
        } else {
            // Hold back a needle prefix minus one byte: any partial
            // delimiter at the buffer's end is shorter than the needle,
            // so holding `needle.len() - 1` bytes always covers it.
            Scan::Content(self.buf.len().saturating_sub(needle.len() - 1))
        }
    }
}

/// The result of scanning the payload buffer for the next delimiter.
#[derive(Debug, PartialEq, Eq)]
enum Scan {
    NeedMore,
    /// `usize` payload bytes precede the next decision point.
    Content(usize),
    Part(usize),
    Close(usize),
    /// The transport padding after a delimiter exceeded the cap.
    PaddingTooLong,
}

/// The suffix classification of a `\r\n--{boundary}` (or body-start
/// `--{boundary}`) match. `Part`/`Close` carry the number of suffix bytes
/// consumed by the delimiter (padding + CRLF); `Content` means the match is
/// a byte-sequence collision inside the payload — not a delimiter.
fn classify_suffix(after: &[u8], at_eof: bool) -> (Suffix, usize) {
    // The close delimiter: `--`, optional transport padding, then CRLF
    // (or end of body).
    if after.first() == Some(&b'-') {
        if after.len() < 2 {
            return if at_eof {
                (Suffix::Content, 0)
            } else {
                (Suffix::NeedMore, 0)
            };
        }
        if after[1] == b'-' {
            let mut padding = 2;
            while matches!(after.get(padding), Some(b' ') | Some(b'\t')) {
                padding += 1;
                if padding - 2 > TRANSPORT_PADDING_LIMIT {
                    // Padding past the cap can never become a legal suffix —
                    // reject instead of buffering (F-T10-2).
                    return (Suffix::PaddingTooLong, 0);
                }
            }
            return match after.get(padding) {
                None => {
                    if at_eof {
                        (Suffix::Close, padding)
                    } else {
                        (Suffix::NeedMore, 0)
                    }
                }
                Some(b'\r') if after.len() == padding + 1 => {
                    // A final lone CR at end of body is tolerated.
                    if at_eof {
                        (Suffix::Close, padding + 1)
                    } else {
                        (Suffix::NeedMore, 0)
                    }
                }
                Some(b'\r') if after.get(padding + 1) == Some(&b'\n') => {
                    (Suffix::Close, padding + 2)
                }
                _ => (Suffix::Content, 0),
            };
        }
        // A single dash is neither `--` nor padding nor CRLF.
        return (Suffix::Content, 0);
    }
    // The part delimiter: optional transport padding, then CRLF.
    let mut padding = 0;
    while matches!(after.get(padding), Some(b' ') | Some(b'\t')) {
        padding += 1;
        if padding > TRANSPORT_PADDING_LIMIT {
            return (Suffix::PaddingTooLong, 0);
        }
    }
    match after.get(padding) {
        None => {
            if at_eof {
                (Suffix::Content, 0)
            } else {
                (Suffix::NeedMore, 0)
            }
        }
        Some(b'\r') if after.len() == padding + 1 => {
            if at_eof {
                (Suffix::Part, padding + 1)
            } else {
                (Suffix::NeedMore, 0)
            }
        }
        Some(b'\r') if after.get(padding + 1) == Some(&b'\n') => (Suffix::Part, padding + 2),
        _ => (Suffix::Content, 0),
    }
}

/// The suffix classification of a delimiter candidate.
#[derive(Debug, PartialEq, Eq)]
enum Suffix {
    NeedMore,
    Part,
    Close,
    Content,
    /// The transport padding after the delimiter exceeded
    /// [`TRANSPORT_PADDING_LIMIT`] — the body must be rejected.
    PaddingTooLong,
}

/// The parsed headers of one multipart part.
#[derive(Debug)]
struct PartHeaders {
    name: Option<String>,
    filename: Option<String>,
    mime_type: Option<String>,
}

/// Parse one part's header block (the bytes before the `\r\n\r\n`
/// separator).
fn parse_part_headers(header_bytes: &[u8]) -> Option<PartHeaders> {
    let headers = String::from_utf8_lossy(header_bytes);
    let mut name = None;
    let mut filename = None;
    let mut mime_type = None;
    for header_line in headers.split("\r\n") {
        let Some((key, value)) = header_line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("content-disposition") {
            name = parameter_of(value, "name");
            filename = parameter_of(value, "filename");
        } else if key.eq_ignore_ascii_case("content-type") && !value.is_empty() {
            mime_type = Some(value.to_string());
        }
    }
    if name.is_none() && filename.is_none() {
        return None;
    }
    Some(PartHeaders {
        name,
        filename,
        mime_type,
    })
}

/// Extract the `boundary` parameter of a `multipart/form-data`
/// content-type header (quoted or bare).
fn boundary_of(content_type: &str) -> Option<String> {
    if !content_type
        .trim()
        .to_ascii_lowercase()
        .starts_with("multipart/")
    {
        return None;
    }
    let boundary = parameter_of(content_type, "boundary")?;
    if boundary.is_empty() {
        return None;
    }
    Some(boundary)
}

/// Extract one `; key="value"` / `; key=value` parameter of a header value.
fn parameter_of(value: &str, key: &str) -> Option<String> {
    for candidate in value.split(';').skip(1) {
        let candidate = candidate.trim();
        let Some((candidate_key, candidate_value)) = candidate.split_once('=') else {
            continue;
        };
        if !candidate_key.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let candidate_value = candidate_value.trim();
        let unquoted = candidate_value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(candidate_value);
        return Some(unquoted.to_string());
    }
    None
}

/// Find `needle` in `haystack` at or after byte offset `from`.
fn find_from(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// A non-empty `name` field override, if present.
fn non_empty_field(
    fields: &std::collections::HashMap<String, String>,
    key: &str,
) -> Option<String> {
    fields
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Download helpers (the TS route's semantics)
// ---------------------------------------------------------------------------

/// Build the content-disposition header: inline for image/video/audio
/// media, attachment otherwise; the filename is embedded only when it is
/// made of the safe characters the TS regex allows.
fn build_content_disposition(name: &str, media_type: &str) -> header::HeaderValue {
    let disposition = if media_type.starts_with("image/")
        || media_type.starts_with("video/")
        || media_type.starts_with("audio/")
    {
        "inline"
    } else {
        "attachment"
    };
    let safe = !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | ' ' | '(' | ')' | '+' | '[' | ']' | '-' | '_')
        });
    let value = if safe {
        format!("{disposition}; filename=\"{name}\"")
    } else {
        disposition.to_string()
    };
    header::HeaderValue::from_str(&value).unwrap_or(header::HeaderValue::from_static("attachment"))
}

/// A satisfiable byte range (`start..=end`), the TS `parseRange` semantics:
/// `bytes=start-end`, `bytes=start-`, and the suffix form `bytes=-N`; a
/// missing, malformed, or unsatisfiable range answers `None` (the caller
/// then serves the whole file with 200).
fn parse_byte_range(header_value: &str, size: usize) -> Option<(usize, usize)> {
    if size == 0 {
        return None;
    }
    let value = header_value.trim();
    let rest = value.strip_prefix("bytes=")?;
    let (start_str, end_str) = rest.split_once('-')?;
    if start_str.is_empty() && end_str.is_empty() {
        return None;
    }
    let (start, end) = if start_str.is_empty() {
        // Suffix range: the last N bytes.
        let suffix: usize = end_str.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start: usize = start_str.parse().ok()?;
        if start >= size {
            return None;
        }
        let end = if end_str.is_empty() {
            size - 1
        } else {
            end_str.parse::<usize>().ok()?
        };
        (start, end)
    };
    if start > end {
        return None;
    }
    Some((start, end.min(size - 1)))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An invalid-params error (the TS `VALIDATION_FAILED` envelope class).
fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::InvalidParams(message.into())
}

/// An internal error.
fn internal(message: impl Into<String>) -> RpcError {
    RpcError::internal(message.into())
}

/// Render a files-route error: HTTP status mirroring the error class (404
/// for not-found, 200 + envelope otherwise — the upload path's TS behavior)
/// with the `{code, msg, data, request_id}` error envelope.
fn files_error_response(error: RpcError, request_id: &str) -> Response {
    let status = match &error {
        RpcError::NotFound(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::OK,
    };
    let envelope =
        serde_json::to_value(ApiErrorResponse::error(&error, request_id)).unwrap_or_default();
    (status, axum::Json(envelope)).into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the streaming parser over a body, splitting it into `chunk`
    /// sized pieces to pin the chunk-boundary hold-back behavior. The file
    /// payload is collected into a sink alongside the parsed upload.
    fn parse_chunked(
        boundary: &str,
        body: &[u8],
        chunk_size: usize,
    ) -> Result<(MultipartUpload, Vec<u8>), RpcError> {
        let mut machine = MultipartState::new(boundary);
        let mut data = Vec::new();
        // The same closure feeds both the streaming loop and `finish()`, so
        // an EOF-decided delimiter flushes through the collector too.
        let mut on_event = |event: MultipartEvent<'_>| -> Result<(), RpcError> {
            if let MultipartEvent::FileData(bytes) = event {
                data.extend_from_slice(bytes);
            }
            Ok(())
        };
        for chunk in body.chunks(chunk_size.max(1)) {
            machine.push(chunk, &mut on_event)?;
        }
        let upload = machine.finish(&mut on_event)?;
        Ok((upload, data))
    }

    /// Run a body through the parser at several chunk cadences — a
    /// delimiter split across a chunk boundary must behave exactly like an
    /// unsplit one.
    fn parse_all_cadences(boundary: &str, body: &[u8]) -> Vec<(MultipartUpload, Vec<u8>)> {
        [1, 2, 3, 7, usize::MAX]
            .iter()
            .map(|&size| {
                parse_chunked(boundary, body, size)
                    .unwrap_or_else(|e| panic!("parse failed at chunk size {size}: {e}"))
            })
            .collect()
    }

    #[test]
    fn should_extract_boundary_when_content_type_carries_it() {
        assert_eq!(
            boundary_of("multipart/form-data; boundary=----abc123"),
            Some("----abc123".to_string())
        );
        assert_eq!(
            boundary_of("multipart/form-data; boundary=\"quoted\""),
            Some("quoted".to_string())
        );
        assert_eq!(boundary_of("application/json"), None);
        assert_eq!(
            boundary_of("multipart/form-data; boundary="),
            None,
            "an empty boundary is rejected"
        );
    }

    #[test]
    fn should_parse_file_and_fields_when_uploading_multipart_body() {
        // Given a single-file multipart body with fields before the file
        let boundary = "testboundary";
        let body = concat!(
            "--testboundary\r\n",
            "Content-Disposition: form-data; name=\"name\"\r\n",
            "\r\n",
            "renamed.txt\r\n",
            "--testboundary\r\n",
            "Content-Disposition: form-data; name=\"expires_in_sec\"\r\n",
            "\r\n",
            "3600\r\n",
            "--testboundary\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "hello",
            "\r\n--testboundary--\r\n",
        );

        // When it is parsed at every chunk cadence
        for (upload, data) in parse_all_cadences(boundary, body.as_bytes()) {
            // Then the file part and the text fields are captured intact
            assert_eq!(upload.filename, "a.txt");
            assert_eq!(upload.mime_type.as_deref(), Some("text/plain"));
            assert_eq!(data, b"hello");
            assert_eq!(
                upload.fields.get("name").map(String::as_str),
                Some("renamed.txt")
            );
            assert_eq!(
                upload.fields.get("expires_in_sec").map(String::as_str),
                Some("3600")
            );
        }
    }

    #[test]
    fn should_fail_when_multipart_body_has_no_file_part() {
        // Given a multipart body with only text fields
        let boundary = "b";
        let body = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"name\"\r\n",
            "\r\n",
            "x\r\n",
            "--b--\r\n",
        );

        // When it is parsed
        let error = parse_chunked(boundary, body.as_bytes(), usize::MAX).unwrap_err();

        // Then the error is the missing-file-field validation
        let message = error.to_string();
        assert!(message.contains("missing"), "error: {message}");
    }

    #[test]
    fn should_fail_when_body_does_not_open_with_the_boundary() {
        // Given a body with junk before the first boundary
        let error = parse_chunked("b", b"junk--b\r\n--b--\r\n", usize::MAX).unwrap_err();

        // Then the parse is rejected as malformed
        let message = error.to_string();
        assert!(message.contains("malformed"), "error: {message}");
    }

    #[test]
    fn should_fail_when_a_second_file_part_arrives() {
        // Given a body carrying two file parts (the TS route's `files: 1`)
        let body = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
            "\r\n",
            "one\r\n",
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"b.txt\"\r\n",
            "\r\n",
            "two\r\n",
            "--b--\r\n",
        );

        // When it is parsed
        let error = parse_chunked("b", body.as_bytes(), usize::MAX).unwrap_err();

        // Then the parse is rejected
        let message = error.to_string();
        assert!(message.contains("multiple"), "error: {message}");
    }

    #[test]
    fn should_reject_a_file_part_truncated_at_end_of_body() {
        // Given a body whose file payload never reaches a delimiter
        let body = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n",
            "\r\n",
            "partial payload without a terminator",
        );

        // When it is parsed
        let error = parse_chunked("b", body.as_bytes(), usize::MAX).unwrap_err();

        // Then the parse is rejected as malformed
        let message = error.to_string();
        assert!(message.contains("malformed"), "error: {message}");
    }

    #[test]
    fn should_preserve_file_bytes_when_content_contains_the_bare_boundary() {
        // Given a payload embedding a bare `--b` byte run inline
        let body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n",
            "\r\n",
            "AA--bZZ",
            "\r\n--b--\r\n",
        )
        .as_bytes()
        .to_vec();

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", &body) {
            // Then the payload is intact — the bare run is not a delimiter
            assert_eq!(upload.filename, "evil.bin");
            assert_eq!(data, b"AA--bZZ");
        }
    }

    #[test]
    fn should_preserve_file_bytes_when_content_collides_with_the_crlf_boundary_prefix() {
        // Given a payload embedding `\r\n--b` followed by a non-delimiter
        // suffix (the prefix collision the naive scanner fell for)
        let body = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n\r\nkeep\r\n--bXYZ\r\nalso keep\r\n--b-\r\n\r\n--b--\r\n";

        // When it is parsed at every cadence
        for (_, data) in parse_all_cadences("b", body.as_bytes()) {
            // Then all collision runs are preserved as content
            assert_eq!(data, b"keep\r\n--bXYZ\r\nalso keep\r\n--b-\r\n");
        }
    }

    #[test]
    fn should_preserve_file_bytes_when_content_collides_with_close_delimiter_variants() {
        // Given payloads embedding close-delimiter shapes followed by junk
        // (`--b--junk`, `--b--` + padding + junk)
        let body = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n\r\nA\r\n--b--junkB\r\n--b-- \tTail\r\n\r\n--b--\r\n";

        // When it is parsed at every cadence
        for (_, data) in parse_all_cadences("b", body.as_bytes()) {
            // Then the variants are all preserved as content
            assert_eq!(data, b"A\r\n--b--junkB\r\n--b-- \tTail\r\n");
        }
    }

    #[test]
    fn should_keep_later_parts_intact_when_file_content_collides() {
        // Given a file payload with boundary collisions followed by a real
        // text field part and a real file terminator
        let body = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n\r\nx\r\n--bNOT-A-DELIMITERy\r\n\r\n--b\r\nContent-Disposition: form-data; name=\"label\"\r\n\r\nafter-the-file\r\n--b--\r\n";

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", body.as_bytes()) {
            // Then the file payload is intact AND the trailing field is
            // captured — the collision neither truncated the file nor
            // bled the parser across parts
            assert_eq!(data, b"x\r\n--bNOT-A-DELIMITERy\r\n");
            assert_eq!(
                upload.fields.get("label").map(String::as_str),
                Some("after-the-file")
            );
        }
    }

    #[test]
    fn should_flush_pending_payload_when_close_delimiter_ends_at_eof() {
        // Given a body that ends with the close delimiter and no trailing
        // CRLF (RFC 2046 legal — the delimiter is complete without it; a
        // hand-rolled client may end the body right after `--boundary--`)
        let body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"eof.bin\"\r\n",
            "Content-Type: application/octet-stream\r\n",
            "\r\n",
            "8-bytes!\r\n--b--",
        )
        .as_bytes()
        .to_vec();

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", &body) {
            // Then the pending payload is flushed — the EOF-decided close is
            // a legal termination, not a silent truncation (F-T10-1)
            assert_eq!(upload.filename, "eof.bin");
            assert_eq!(data, b"8-bytes!");
        }
    }

    #[test]
    fn should_flush_pending_payload_when_close_delimiter_ends_with_lone_cr_at_eof() {
        // Given a body ending with the close delimiter followed by a lone CR
        // (the tolerated end-of-body shape)
        let body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"cr.bin\"\r\n",
            "\r\n",
            "tail-bytes\r\n--b--\r",
        )
        .as_bytes()
        .to_vec();

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", &body) {
            // Then the pending payload survives the EOF decision
            assert_eq!(upload.filename, "cr.bin");
            assert_eq!(data, b"tail-bytes");
        }
    }

    #[test]
    fn should_flush_pending_payload_when_part_delimiter_ends_with_lone_cr_at_eof() {
        // Given a body whose file part is followed by a part delimiter that
        // only a lone CR terminates at EOF (the tail part is truncated and
        // dropped, but the file payload must survive)
        let body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"part.bin\"\r\n",
            "\r\n",
            "file-bytes\r\n--b\r",
        )
        .as_bytes()
        .to_vec();

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", &body) {
            // Then the file bytes are flushed before the truncated tail part
            // is dropped
            assert_eq!(upload.filename, "part.bin");
            assert_eq!(data, b"file-bytes");
        }
    }

    #[test]
    fn should_reject_transport_padding_beyond_the_cap() {
        // Given a body whose close delimiter carries more transport padding
        // than the cap (an unbounded padding run would pin the scan buffer
        // and be rescanned on every push — the F-T10-2 DoS vector)
        let mut body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"pad.bin\"\r\n",
            "\r\n",
            "payload\r\n--b--",
        )
        .as_bytes()
        .to_vec();
        body.extend(std::iter::repeat_n(b' ', TRANSPORT_PADDING_LIMIT + 1));

        // When it is parsed
        let error = parse_chunked("b", &body, usize::MAX).unwrap_err();

        // Then the request is rejected rather than buffered
        let message = error.to_string();
        assert!(message.contains("padding"), "error: {message}");

        // And likewise when the padding rides a part delimiter
        let mut body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"pad.bin\"\r\n",
            "\r\n",
            "payload\r\n--b",
        )
        .as_bytes()
        .to_vec();
        body.extend(std::iter::repeat_n(b' ', TRANSPORT_PADDING_LIMIT + 1));
        body.extend(b"\r\n");
        let error = parse_chunked("b", &body, usize::MAX).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("padding"), "error: {message}");
    }

    #[test]
    fn should_accept_transport_padding_at_exactly_the_cap() {
        // Given a body padding the close delimiter with exactly the capped
        // number of LWSP bytes (RFC 2046-legal)
        let mut body: Vec<u8> = concat!(
            "--b\r\n",
            "Content-Disposition: form-data; name=\"file\"; filename=\"cap.bin\"\r\n",
            "\r\n",
            "payload\r\n--b--",
        )
        .as_bytes()
        .to_vec();
        body.extend(std::iter::repeat_n(b' ', TRANSPORT_PADDING_LIMIT));

        // When it is parsed
        let (upload, data) = parse_chunked("b", &body, usize::MAX).expect("parse");

        // Then the padded close is accepted with the payload intact
        assert_eq!(upload.filename, "cap.bin");
        assert_eq!(data, b"payload");
    }

    #[test]
    fn should_tolerate_transport_padding_and_epilogue_around_delimiters() {
        // Given a body using RFC 2046 transport padding after the part
        // delimiter and an epilogue after the close delimiter
        let body = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"padded.bin\"\r\n\r\npayload\r\n--b \t\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\npadded field\r\n--b--  \r\nepilogue junk bytes";

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", body.as_bytes()) {
            // Then the padded delimiters are recognized and the epilogue
            // ignored
            assert_eq!(upload.filename, "padded.bin");
            assert_eq!(data, b"payload");
            assert_eq!(
                upload.fields.get("note").map(String::as_str),
                Some("padded field")
            );
        }
    }

    #[test]
    fn should_accept_a_file_part_followed_by_fields() {
        // Given the file part first and the fields after (any order is
        // tolerated)
        let body = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"first.bin\"\r\n\r\nthe payload\r\n--b\r\nContent-Disposition: form-data; name=\"trailing\"\r\n\r\nvalue\r\n--b--\r\n";

        // When it is parsed at every cadence
        for (upload, data) in parse_all_cadences("b", body.as_bytes()) {
            // Then both the file and the after-the-file field are captured
            assert_eq!(upload.filename, "first.bin");
            assert_eq!(data, b"the payload");
            assert_eq!(
                upload.fields.get("trailing").map(String::as_str),
                Some("value")
            );
        }
    }

    #[test]
    fn should_return_inline_disposition_for_image_media_type() {
        let value = build_content_disposition("cat.png", "image/png");
        assert_eq!(value.to_str().unwrap(), "inline; filename=\"cat.png\"");
    }

    #[test]
    fn should_return_attachment_disposition_for_other_media_type() {
        let value = build_content_disposition("doc.pdf", "application/pdf");
        assert_eq!(value.to_str().unwrap(), "attachment; filename=\"doc.pdf\"");
    }

    #[test]
    fn should_drop_filename_when_name_has_unsafe_characters() {
        let value = build_content_disposition("a\"; rm -rf", "text/plain");
        assert_eq!(value.to_str().unwrap(), "attachment");
    }

    #[test]
    fn should_parse_normal_suffix_and_open_ranges() {
        // Given a 16-byte file
        let size = 16;

        // When each range spelling is parsed
        // Then the exact slice is computed (TS parseRange semantics)
        assert_eq!(parse_byte_range("bytes=2-5", size), Some((2, 5)));
        assert_eq!(parse_byte_range("bytes=2-", size), Some((2, 15)));
        assert_eq!(parse_byte_range("bytes=-4", size), Some((12, 15)));
        assert_eq!(parse_byte_range("bytes=0-99", size), Some((0, 15)));
        // And the unsatisfiable/malformed spellings degrade to None
        assert_eq!(parse_byte_range("bytes=16-20", size), None);
        assert_eq!(parse_byte_range("bytes=-", size), None);
        assert_eq!(parse_byte_range("bytes=5-2", size), None);
        assert_eq!(parse_byte_range("items=1-2", size), None);
    }
}
