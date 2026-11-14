//! `ReadTool` — read a file's contents with line numbers.
//!
//! Mirrors the TypeScript `ReadTool`: reads a text file, renders the
//! requested line range with 1-based line numbers, refuses binary files
//! (NUL bytes) and non-UTF-8 content up front, and signals truncation on the
//! `note` side channel plus the `truncated` flag so the loop engine can
//! surface it without polluting the visible output.

use std::path::Path;
use std::sync::Arc;

use mirri_os::DynFileSystem;

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::tool_execution_error;
use crate::tools::{PathClassification, WorkspacePaths};

/// Default number of lines to show when the model does not specify `n_lines`.
/// Equal to [`MAX_LINES`]: the per-call cap is the default, so a whole-file
/// read returns the first 1000 lines and reports itself truncated.
const DEFAULT_N_LINES: u64 = MAX_LINES;

/// Maximum line length: longer lines are truncated with a `... (truncated)`
/// suffix so a single pathological line cannot blow up the output.
const MAX_LINE_LENGTH: usize = 2000;

/// Maximum number of lines returned per call, regardless of `n_lines`.
const MAX_LINES: u64 = 1000;

/// Maximum file size Read accepts for a whole-file read (100 KiB, the spec
/// limit — the rendered-window cap, applied here to the input only when the
/// model did not explicitly narrow the request). Explicit `line_offset` /
/// `n_lines` requests stream through files of any size.
const MAX_FILE_BYTES: u64 = 100 * 1024;

/// Stream chunk size for range reads: keeps memory bounded by the requested
/// line window instead of the file size while paging through large files.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Image/video extensions that Read redirects to `ReadMediaFile`.
const IMAGE_VIDEO_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "mp4", "webm",
];

/// True when `path`'s extension marks it as an image or video file.
fn is_image_or_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| IMAGE_VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// `Read` — read a text file with line numbers.
///
/// When constructed with a workspace sandbox ([`ReadTool::new`]), every path
/// is validated against the workspace root or any registered additional
/// dir, and paths escaping all of them (traversal, absolute outside paths,
/// symlinks) are refused before any I/O (issue 15D).
pub struct ReadTool {
    /// Workspace sandbox this tool may read inside; `None` disables
    /// validation.
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider every read goes through (zero-IO T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl ReadTool {
    /// Create a read tool confined to the `workspace_paths` sandbox, or
    /// `None` to disable workspace path validation. `fs` is the injected
    /// filesystem provider (the engine default is the local one).
    pub fn new(workspace_paths: Option<Arc<WorkspacePaths>>, fs: Arc<dyn DynFileSystem>) -> Self {
        Self {
            workspace_paths,
            fs,
        }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a text file and return its content with 1-based line numbers. \
         This is the tool for any known file, including one line range of it — \
         do not shell out to `cat` / `head` / `tail` / `sed -n 'A,Bp'` for that. \
         Use 'line_offset' (1-based, default 1; negative values read from the \
         end, e.g. -100 reads the last 100 lines) and 'n_lines' (default 1000, \
         which is also the per-call maximum) to page through large files. A \
         whole-file read of a file longer than 1000 lines returns the first \
         1000 lines and says so; re-read with 'line_offset' to continue. \
         Whole-file reads of files larger than 100 KB are refused — re-read \
         those with 'line_offset'/'n_lines'. Refuses directories, binary files \
         (NUL bytes), non-UTF-8 content, and image/video files (use \
         ReadMediaFile for those). Lines longer than 2000 characters are \
         truncated."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to read. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "line_offset": {
                        "type": "integer",
                        "description": "1-based line number of the first line to show. \
                                        Defaults to 1. Negative values read from the end \
                                        of the file: -N starts at the Nth line from the \
                                        end (i.e. the last N lines)."
                    },
                    "n_lines": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of lines to show. Defaults to 1000, which is also the per-call maximum; larger values are capped."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        })
    }

    fn capabilities(&self) -> &[String] {
        static CAPABILITIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(|| vec!["code.read".to_string()])
    }

    /// ADR-015 (stage-2 D-M2): the read-only capability category —
    /// observation only, no side effects the ReadOnly posture must gate.
    fn readonly(&self) -> bool {
        true
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
            return Ok(tool_execution_error(
                "Read requires a string 'path' argument",
            ));
        };
        let path = path.to_string();
        // Classify against the workspace sandbox before any I/O (issue
        // 15D, plan 2026-10-10-sandbox-path-permission D1). Inside and
        // Outside both build the plan over the canonical path (Outside
        // enters the permission chain instead of being refused here);
        // only escape shapes are refused. The canonical path is used for
        // the plan and the read, so the permission policies and the file
        // access agree on the target.
        let file_path = match crate::tools::classify_tool_path(
            self.workspace_paths.as_deref(),
            &self.fs,
            &path,
        ) {
            PathClassification::Inside(canonical) | PathClassification::Outside(canonical) => {
                canonical
            }
            PathClassification::Escape { reason } => {
                return Ok(tool_execution_error(crate::tools::escape_tool_error(
                    "Read", &path, &reason,
                )));
            }
        };
        let path = file_path.to_string_lossy().into_owned();
        // `line_offset` is 1-based and may be negative (tail read). `offset`
        // is accepted as a backward-compatible serde alias for `line_offset`.
        // The arg-converter layer also renames `offset` → `line_offset`
        // upstream; the fallback here covers direct (test) invocations.
        let line_offset = input
            .get("line_offset")
            .or_else(|| input.get("offset"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1);
        let n_lines = input
            .get("n_lines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_N_LINES);
        // The model explicitly narrowed the request when it named a range
        // parameter; whole-file reads without one stay behind the size gate
        // so an accidental full read of a huge file is refused fast instead
        // of streaming through it.
        let explicit_range = input.get("n_lines").is_some()
            || input.get("line_offset").is_some()
            || input.get("offset").is_some();

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                kind: AccessKind::File,
                path: Some(path.clone()),
                operation: AccessOperation::Read,
            }],
            display: ToolInputDisplay::FileIo {
                path: path.clone(),
                // Lowercase: the protocol `file_io.operation` enum only
                // accepts `read|write|edit|glob|grep`.
                operation: "read".to_string(),
            },
            approval_rule: "Read".to_string(),
            stop_batch_after_this: false,
        };

        let fs = Arc::clone(&self.fs);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(read_file_range(
                    fs,
                    path,
                    line_offset,
                    n_lines,
                    explicit_range,
                ))
            }),
        })
    }
}

/// Truncate a line longer than [`MAX_LINE_LENGTH`] characters, appending a
/// marker so the model knows content was cut. Character-based (not byte-based)
/// so UTF-8 text is never split mid-codepoint.
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_LINE_LENGTH).collect();
    format!("{head}... (truncated)")
}

async fn read_file_range(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    line_offset: i64,
    n_lines: u64,
    explicit_range: bool,
) -> ToolResult {
    let file_path = Path::new(&path);

    // Refuse directories up front with a precise message.
    match fs.stat(file_path).await {
        Ok(stat) if stat.is_dir => {
            return ToolResult::error(format!("Read failed: '{path}' is a directory"));
        }
        // Empty files are valid — return an empty range.
        Ok(stat) if stat.size == 0 => {
            return ToolResult::success("");
        }
        // Image/video files must go through ReadMediaFile, not the text path.
        Ok(_) if is_image_or_video(file_path) => {
            return ToolResult::error(format!(
                "Read refused '{path}': this is an image/video file. \
                 Use the ReadMediaFile tool to view it."
            ));
        }
        // Whole-file reads of oversized files are refused fast — an explicit
        // range request pages through instead, so the guidance names the
        // paging parameters.
        Ok(stat) if stat.size > MAX_FILE_BYTES && !explicit_range => {
            return ToolResult::error(format!(
                "Read refused '{path}': file is {} bytes, which exceeds the \
                 whole-file read limit of {MAX_FILE_BYTES} bytes (100 KB). Re-read \
                 with 'line_offset' or 'n_lines' to page through the file.",
                stat.size
            ));
        }
        Err(error) => {
            return ToolResult::error(format!("Read failed to stat '{path}': {error}"));
        }
        _ => {}
    }

    // Stream the file in chunks, assembling complete lines as they arrive.
    // The whole file is still traversed (the exact line total feeds the
    // truncation note and tail offsets, and NUL/UTF-8 checks keep the
    // whole-file binary-refusal semantics), but memory stays bounded by the
    // requested window: a forward read keeps at most `cap` lines, a tail
    // read at most `|line_offset|` lines.
    let mut sink = LineSink::new(line_offset, n_lines);
    let mut carry: Vec<u8> = Vec::new();

    let mut offset: u64 = 0;
    loop {
        let chunk = match fs
            .read_bytes_range(file_path, offset, READ_CHUNK_BYTES)
            .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                return ToolResult::error(format!("Read failed to read '{path}': {error}"));
            }
        };
        if chunk.is_empty() {
            break;
        }
        // NUL bytes mark binary content anywhere in the file.
        if chunk.contains(&0) {
            return ToolResult::error(format!(
                "Read refused '{path}': file contains NUL bytes (binary content)"
            ));
        }
        carry.extend_from_slice(&chunk);
        // '\n' never appears inside a multi-byte UTF-8 sequence, so cutting
        // on it is safe; each assembled line is validated individually. A
        // cursor scan plus one trailing drain keeps the per-chunk cost linear
        // (draining per line would shift the remaining bytes on every line).
        let mut cursor = 0usize;
        while let Some(rel) = carry[cursor..].iter().position(|b| *b == b'\n') {
            let end = cursor + rel;
            let line = match std::str::from_utf8(&carry[cursor..end]) {
                Ok(line) => line,
                Err(_) => {
                    return ToolResult::error(format!(
                        "Read refused '{path}': content is not valid UTF-8 (binary content)"
                    ));
                }
            };
            sink.push(line);
            cursor = end + 1;
        }
        carry.drain(..cursor);
        offset += chunk.len() as u64;
        if chunk.len() < READ_CHUNK_BYTES {
            break;
        }
    }
    // A trailing partial line (no final newline) counts as a line only when
    // non-empty — a trailing newline must not produce a phantom line.
    if !carry.is_empty() {
        let line = match std::str::from_utf8(&carry) {
            Ok(line) => line,
            Err(_) => {
                return ToolResult::error(format!(
                    "Read refused '{path}': content is not valid UTF-8 (binary content)"
                ));
            }
        };
        sink.push(line);
    }

    let shown: Vec<&str> = sink.shown();

    // Render line by line, stopping before the shared context budget is
    // exceeded. Accounting is done on the *rendered* line (line number, tab,
    // content, newline), because that is what actually reaches the model —
    // estimating from the raw line length would drift on every line.
    //
    // Both read directions emit at least one line when `shown` is non-empty:
    // `truncate_line` caps a line at MAX_LINE_LENGTH (2000) characters, so a
    // rendered line is at most ~2020 bytes — far under the 50,000-byte
    // budget. The "always keep one line" guard keeps that invariant explicit
    // rather than implied, so a future change to MAX_LINE_LENGTH cannot
    // silently produce an empty window.
    //
    // The trimming direction follows the read direction:
    //   * forward read — fill from the head, stop at the budget, deliver the
    //     `[start, start+m]` prefix.
    //   * tail read — fill from the *back*, stop at the budget, deliver the
    //     `[total-k, total]` suffix. A tail read means "the end of the
    //     file", so the window must keep ending at the last line; trimming
    //     the head is what makes an over-budget tail fit without moving away
    //     from EOF. Skipping the budget entirely for tails instead would let
    //     a wide-line file push ~2 MB into the downstream truncator, which
    //     replaces the whole result with a 2000-character preview — the exact
    //     failure this budget exists to prevent.
    let budget = crate::tool_result_truncation::MAX_BYTES;
    let (first_index, last_index, output, emitted_line_truncated) = if line_offset < 0 {
        let mut output = String::new();
        let mut first = shown.len();
        let mut line_clipped = false;
        for (index, line) in shown.iter().enumerate().rev() {
            let line_no = sink.start() + index + 1;
            let clipped = line.chars().count() > MAX_LINE_LENGTH;
            let rendered = format!("{line_no}\t{}\n", truncate_line(line));
            if first < shown.len() && output.len() + rendered.len() > budget {
                break;
            }
            output.insert_str(0, &rendered);
            first = index;
            line_clipped |= clipped;
        }
        (first, shown.len(), output, line_clipped)
    } else {
        let mut output = String::new();
        let mut emitted = 0usize;
        let mut line_clipped = false;
        for (index, line) in shown.iter().enumerate() {
            let line_no = sink.start() + index + 1;
            let clipped = line.chars().count() > MAX_LINE_LENGTH;
            let rendered = format!("{line_no}\t{}\n", truncate_line(line));
            if emitted > 0 && output.len() + rendered.len() > budget {
                break;
            }
            output.push_str(&rendered);
            emitted += 1;
            line_clipped |= clipped;
        }
        (0, emitted, output, line_clipped)
    };
    let emitted = last_index - first_index;
    // The budget stopped the window before the line cap / file end did.
    let byte_stopped = emitted < shown.len();
    // 1-based line numbers of the delivered window's ends.
    let first_line_no = sink.start() + first_index + 1;
    let last_line_no = sink.start() + last_index;
    // An empty window (empty file, or an offset past EOF) reports nothing
    // delivered rather than a nonsensical "lines 1-0" range.
    let range_clause = if emitted == 0 {
        format!("no lines of {}", sink.total())
    } else {
        format!("lines {first_line_no}-{last_line_no} of {}", sink.total())
    };

    // A read is truncated only when content the model asked for could not be
    // delivered: the per-call line cap cut the window short, the byte budget
    // stopped it, or a delivered line was clipped. A window the caller
    // explicitly narrowed and then fully received is complete — reporting it
    // as truncated pushes the model to re-read what it already has.
    //
    // The clipped-line flag is computed over the *emitted* lines only: a line
    // the budget never delivered is not the model's problem yet.
    let any_line_truncated = emitted_line_truncated;
    let truncated =
        (sink.end() < sink.total() && n_lines >= MAX_LINES) || any_line_truncated || byte_stopped;
    let mut note = if truncated {
        // The note states what was delivered and how to cover the rest; it
        // does not construct the next read. Anchoring a continuation to the
        // delivered window broke differently by read mode: a forward offset
        // past the window walks away from a head-trimmed tail's undelivered
        // lines, and an offset just before the window turns one insight into
        // a page chain the model never asked for. Paging from line 1 always
        // addresses the whole file, so every truncation points there and the
        // model decides which span it still needs (SDR-007 / ADR-9).
        format!(
            "Read truncated: showing {range_clause}; re-read from 'line_offset': 1 to page through the file"
        )
    } else if last_line_no < sink.total() {
        format!("Read {range_clause}.")
    } else {
        String::new()
    };
    if any_line_truncated {
        if !note.is_empty() {
            note.push_str("; ");
        }
        note.push_str(&format!(
            "some lines exceed {MAX_LINE_LENGTH} characters and were truncated"
        ));
    }
    let note = (!note.is_empty()).then_some(note);

    ToolResult {
        output: ToolOutput::text(output),
        is_error: false,
        note,
        stop_turn: false,
        truncated,
        injection: None,
    }
}

/// Streaming line sink for one ranged read: counts every line while keeping
/// only what the request can return — the forward window (lines
/// `skip..skip+cap`) or the tail ring (the last `|line_offset|` lines) — so
/// memory stays bounded by the requested window instead of the file size.
struct LineSink {
    /// 1-based line offset; `>= 1` selects the forward window, `< 0` the
    /// tail ring.
    line_offset: i64,
    /// Window line cap: `n_lines` clamped by [`MAX_LINES`].
    cap: usize,
    /// Total lines fed so far.
    total: usize,
    /// Forward window: lines `[skip, skip+cap)`, 0-based, in arrival order.
    window: Vec<String>,
    /// Tail ring: the last `|line_offset|` lines seen.
    ring: std::collections::VecDeque<String>,
}

impl LineSink {
    fn new(line_offset: i64, n_lines: u64) -> Self {
        Self {
            line_offset,
            cap: n_lines.min(MAX_LINES) as usize,
            total: 0,
            window: Vec::new(),
            ring: std::collections::VecDeque::new(),
        }
    }

    /// Feed one assembled line (without its newline). Only the sink this
    /// read mode uses gets a clone, so a forward read never materializes the
    /// tail ring and vice versa.
    fn push(&mut self, line: &str) {
        self.total += 1;
        if self.line_offset >= 1 {
            let skip = (self.line_offset - 1) as usize;
            if self.total > skip && self.window.len() < self.cap {
                self.window.push(line.to_string());
            }
        } else if self.line_offset < 0 {
            self.ring.push_back(line.to_string());
            while self.ring.len() > self.line_offset.unsigned_abs() as usize {
                self.ring.pop_front();
            }
        }
        // line_offset == 0 is out of range (the canonical default is 1): the
        // requested slice is empty, so only the running total matters.
    }

    /// The 0-based start index of the requested range, mirroring the
    /// TypeScript `ReadTool`:
    ///   * line_offset >= 1  → forward read: start = line_offset - 1
    ///   * line_offset <  0   → tail read:    start = total + line_offset
    ///     (e.g. -1 reads the last line, -N reads the last N lines)
    ///   * line_offset == 0   → out of range: start = total (empty range)
    fn start(&self) -> usize {
        if self.line_offset >= 1 {
            ((self.line_offset - 1) as usize).min(self.total)
        } else {
            let s = self.total as i64 + self.line_offset;
            if s < 0 {
                0
            } else {
                (s as usize).min(self.total)
            }
        }
    }

    /// The 0-based exclusive end index of the requested range.
    fn end(&self) -> usize {
        (self.start() + self.cap).min(self.total)
    }

    fn total(&self) -> usize {
        self.total
    }

    /// The requested `[start, end)` slice, selected from whichever source
    /// this read mode used.
    fn shown(&self) -> Vec<&str> {
        let (start, end) = (self.start(), self.end());
        if self.line_offset >= 1 {
            let skip = (self.line_offset - 1) as usize;
            let start_in_window = start.saturating_sub(skip);
            let end_in_window = start_in_window + (end - start);
            self.window
                .get(start_in_window..end_in_window)
                .map(|slice| slice.iter().map(String::as_str).collect())
                .unwrap_or_default()
        } else {
            let ring_start = self.total - self.ring.len();
            let start_in_ring = start.saturating_sub(ring_start);
            let end_in_ring = start_in_ring + (end - start);
            self.ring
                .range(start_in_ring..end_in_ring)
                .map(|line| line.as_str())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine-default local filesystem, as the registered tool receives it.
    fn local_fs() -> Arc<dyn DynFileSystem> {
        Arc::new(mirri_os::LocalFs::new())
    }

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    fn write_fixture(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /// The 1-based line number of the first delivered line.
    fn first_delivered_line(text: &str) -> usize {
        text.lines()
            .next()
            .and_then(|line| line.split('\t').next()?.parse().ok())
            .expect("at least one delivered line")
    }

    #[tokio::test]
    async fn should_return_numbered_lines_when_reading_a_three_line_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "a.txt", "one\ntwo\nthree\n");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.truncated);
        assert_eq!(output_text(&result), "1\tone\n2\ttwo\n3\tthree\n");
    }

    #[tokio::test]
    async fn should_not_produce_a_phantom_line_when_reading_a_file_without_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "b.txt", "first\nsecond");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "1\tfirst\n2\tsecond\n");
    }

    #[tokio::test]
    async fn should_start_at_the_requested_line_when_reading_with_a_line_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "c.txt", "a\nb\nc\nd\ne\n");
        let result =
            read_file_range(local_fs(), path.to_string_lossy().into_owned(), 3, 2, false).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "3\tc\n4\td\n");
    }

    #[tokio::test]
    async fn should_not_flag_truncation_when_the_requested_window_is_fully_delivered() {
        // Given a five-line file and an explicit request for lines 1-2
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "d.txt", "1\n2\n3\n4\n5\n");
        let result =
            read_file_range(local_fs(), path.to_string_lossy().into_owned(), 1, 2, true).await;

        // Then the model gets exactly what it asked for and no truncation
        // signal, only a factual line-range note
        assert!(!result.is_error);
        assert!(
            !result.truncated,
            "a fully delivered window must not be flagged truncated"
        );
        let note = result.note.clone().expect("line-range note");
        assert_eq!(note, "Read lines 1-2 of 5.");
        assert_eq!(output_text(&result), "1\t1\n2\t2\n");
    }

    /// Given a file far longer than one call can return
    /// When a whole-file read is capped at 1000 lines
    /// Then the note states the delivered range and points at paging from
    /// line 1 — never at a line just past the window to auto-chain the next
    /// read (SDR-007 / ADR-9)
    #[tokio::test]
    async fn should_flag_truncation_when_the_requested_window_exceeds_the_per_call_cap() {
        // Given a 1200-line file and a request for more lines than one call
        // can return
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(
            tmp.path(),
            "cap.txt",
            &(1..=1200).map(|n| format!("{n}\n")).collect::<String>(),
        );
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            5000,
            true,
        )
        .await;

        // Then the result is capped at 1000 lines and says it was cut short,
        // states what it delivered, and leaves paging to the model
        assert!(!result.is_error);
        assert!(result.truncated);
        let note = result.note.clone().expect("truncation note");
        assert_eq!(
            note,
            "Read truncated: showing lines 1-1000 of 1200; re-read from 'line_offset': 1 to page through the file"
        );
        assert_eq!(output_text(&result).lines().count(), 1000);
    }

    #[tokio::test]
    async fn should_return_empty_output_when_line_offset_is_beyond_end_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "e.txt", "only\n");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            6,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "");
    }

    #[tokio::test]
    async fn should_succeed_with_empty_output_when_reading_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "f.txt", "");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "");
    }

    #[tokio::test]
    async fn should_return_error_result_when_reading_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let result = read_file_range(
            local_fs(),
            tmp.path().to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("is a directory"));
    }

    #[tokio::test]
    async fn should_refuse_when_reading_a_binary_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bin.dat");
        std::fs::write(&path, b"\x00\x01\x02\x03").unwrap();
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("NUL bytes"));
    }

    #[tokio::test]
    async fn should_return_error_result_when_reading_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.txt");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("failed"));
    }

    #[tokio::test]
    async fn should_read_an_outside_absolute_file_when_reading_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let tool = ReadTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": outside_file.to_string_lossy()
            }))
            .expect("resolve");

        // A clean absolute path outside the sandbox is no longer refused at
        // the tool layer: the plan is built over the canonical path and the
        // authorization flow (permission chain) decides.
        let ToolExecution::Runnable { plan, execute } = execution else {
            panic!("expected a runnable execution for an outside absolute path");
        };
        let access = plan.accesses.first().expect("one file access");
        assert_eq!(
            access.path.as_deref(),
            Some(outside_file.canonicalize().unwrap().to_str().unwrap()),
            "the canonical outside path must enter the accesses"
        );
        let result = execute(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        })
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "1\tsecret\n");
    }

    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_reading_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = ReadTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": "../secret.txt" }))
            .expect("resolve");

        let ToolExecution::Error(result) = execution else {
            panic!("expected an escape refusal, got a runnable execution");
        };
        assert!(
            result.message.contains("escapes the workspace sandbox"),
            "refusal uses the new escape wording: {}",
            result.message
        );
        assert!(
            result.message.contains("absolute path"),
            "refusal points at the absolute-path channel: {}",
            result.message
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_refuse_symlink_escape_when_reading_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "s3cret").unwrap();
        std::os::unix::fs::symlink(&secret, tmp.path().join("leak.txt")).unwrap();

        let tool = ReadTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": tmp.path().join("leak.txt").to_string_lossy()
            }))
            .expect("resolve");

        let ToolExecution::Error(result) = execution else {
            panic!("expected an escape refusal, got a runnable execution");
        };
        assert!(
            result.message.contains("escapes the workspace sandbox"),
            "refusal uses the new escape wording: {}",
            result.message
        );
        assert!(
            result.message.contains("symlink"),
            "refusal explains the symlink escape: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn should_read_file_inside_workspace_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "in.txt", "hello\n");

        let tool = ReadTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": path.to_string_lossy() }))
            .expect("resolve");

        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let result = execute(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        })
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "1\thello\n");
    }

    #[tokio::test]
    async fn should_resolve_relative_path_against_workspace_root_when_reading() {
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path(), "rel.txt", "rel\n");

        let tool = ReadTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": "rel.txt" }))
            .expect("resolve");

        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let result = execute(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        })
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "1\trel\n");
    }

    #[tokio::test]
    async fn should_start_at_line_one_when_line_offset_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "lo1.txt", "alpha\nbeta\ngamma\n");
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.truncated);
        assert_eq!(output_text(&result), "1\talpha\n2\tbeta\n3\tgamma\n");
    }

    #[tokio::test]
    async fn should_read_from_end_when_line_offset_is_negative() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "neg.txt", "a\nb\nc\nd\ne\n");
        // line_offset = -2 reads the last 2 lines (lines 4 and 5).
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -2,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        assert!(!result.truncated);
        assert_eq!(output_text(&result), "4\td\n5\te\n");

        // line_offset = -1 reads exactly the last line.
        let result_last = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -1,
            10,
            false,
        )
        .await;
        assert!(!result_last.is_error);
        assert_eq!(output_text(&result_last), "5\te\n");

        // A tail count larger than the file clamps to the whole file.
        let result_all = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -100,
            10,
            false,
        )
        .await;
        assert!(!result_all.is_error);
        assert_eq!(output_text(&result_all), "1\ta\n2\tb\n3\tc\n4\td\n5\te\n");
    }

    #[tokio::test]
    async fn should_accept_offset_as_alias_for_line_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "alias.txt", "a\nb\nc\nd\ne\n");
        let display = path.to_string_lossy().into_owned();

        // The legacy `offset` name must behave identically to `line_offset`.
        let via_alias = read_file_range(local_fs(), display.clone(), 2, 10, false).await;
        assert!(!via_alias.is_error);
        assert_eq!(output_text(&via_alias), "2\tb\n3\tc\n4\td\n5\te\n");

        // Verify the alias is honoured end-to-end through resolve_execution:
        // passing `offset` produces the same result as passing `line_offset`.
        let tool = ReadTool::new(None, local_fs());

        let alias_exec = tool
            .resolve_execution(serde_json::json!({
                "path": display,
                "offset": 2
            }))
            .expect("resolve alias");
        let ToolExecution::Runnable {
            execute: alias_run, ..
        } = alias_exec
        else {
            panic!("expected a runnable execution for the alias");
        };
        let alias_result = alias_run(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-alias".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        })
        .await;

        let canonical_exec = tool
            .resolve_execution(serde_json::json!({
                "path": display,
                "line_offset": 2
            }))
            .expect("resolve canonical");
        let ToolExecution::Runnable {
            execute: canonical_run,
            ..
        } = canonical_exec
        else {
            panic!("expected a runnable execution for the canonical name");
        };
        let canonical_result = canonical_run(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-canonical".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        })
        .await;

        assert_eq!(output_text(&alias_result), output_text(&canonical_result));
        assert_eq!(output_text(&alias_result), "2\tb\n3\tc\n4\td\n5\te\n");
    }

    #[tokio::test]
    async fn should_truncate_lines_longer_than_2000_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let long_line = "x".repeat(2500);
        let path = write_fixture(tmp.path(), "long.txt", &format!("{long_line}\nend\n"));

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let first = text.lines().next().unwrap();
        // 2000 chars head + "1\t" prefix + truncation marker.
        assert!(
            first.ends_with("... (truncated)"),
            "line should carry the truncation marker: …{first_tail}",
            first_tail = &first[first.len().saturating_sub(30)..]
        );
        let content = first.strip_prefix("1\t").unwrap();
        assert_eq!(content.chars().count(), 2000 + "... (truncated)".len());
        // The following line is untouched.
        assert!(
            text.contains("2\tend\n"),
            "later line must be intact: {text}"
        );
        assert!(
            result.truncated,
            "line truncation must set the truncated flag"
        );
    }

    #[tokio::test]
    async fn should_refuse_reading_beyond_100kb() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "big.txt", &"y".repeat(200_000));

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("100 KB"),
            "message should name the limit: {text}"
        );
        assert!(
            text.contains("exceeds"),
            "message should explain refusal: {text}"
        );
    }

    #[tokio::test]
    async fn should_redirect_to_read_media_when_reading_image_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Extension-based detection: the bytes are arbitrary, the extension is
        // what routes image/video files to ReadMediaFile.
        let path = write_fixture(tmp.path(), "screenshot.png", "not really an image");

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;
        assert!(result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("ReadMediaFile"),
            "message should redirect to ReadMediaFile: {text}"
        );
    }

    #[test]
    fn should_identify_image_and_video_extensions_when_checking_file_paths() {
        for name in [
            "a.png", "a.jpg", "a.jpeg", "a.gif", "a.webp", "a.bmp", "a.svg", "a.mp4", "a.webm",
        ] {
            assert!(is_image_or_video(Path::new(name)), "{name} should be media");
        }
        for name in ["a.txt", "a.rs", "a.PNG.TXT", "notes"] {
            assert!(
                !is_image_or_video(Path::new(name)),
                "{name} should not be media"
            );
        }
        // Extension matching is case-insensitive.
        assert!(is_image_or_video(Path::new("photo.JPG")));
    }

    /// Build an oversized text fixture: `lines` lines of exactly `width`
    /// bytes each (including the trailing newline). Every line is ASCII and
    /// starts with its 4-digit 1-based line number, so windowed output is
    /// easy to assert on and byte offsets are predictable.
    fn oversized_fixture(
        dir: &std::path::Path,
        name: &str,
        lines: usize,
        width: usize,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut content = String::new();
        for number in 1..=lines {
            let prefix = format!("{number:04}");
            content.push_str(&prefix);
            content.push_str(&"x".repeat(width - prefix.len() - 1));
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn should_page_through_a_file_larger_than_the_whole_file_limit_when_a_range_is_explicit()
    {
        // Given a 300-line, 500-byte-per-line file (150 KB > the 100 KB
        // whole-file limit), and an explicitly narrowed request
        let tmp = tempfile::tempdir().unwrap();
        let path = oversized_fixture(tmp.path(), "big.txt", 300, 500);
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            101,
            3,
            true,
        )
        .await;

        // Then the requested window is returned with correct numbering and
        // line content spanning several stream chunks
        assert!(!result.is_error, "output was: {}", output_text(&result));
        let output = output_text(&result);
        assert_eq!(output.lines().count(), 3);
        assert!(output.starts_with("101\t0101"), "output was: {output}");
        assert!(output.contains("\n102\t0102"), "output was: {output}");
        assert!(output.contains("\n103\t0103"), "output was: {output}");
        // And the note reports the exact file-wide total without claiming a
        // truncation the caller never suffered
        assert!(!result.truncated);
        let note = result.note.clone().expect("line-range note");
        assert_eq!(note, "Read lines 101-103 of 300.");
    }

    #[tokio::test]
    async fn should_return_tail_lines_from_a_file_larger_than_the_limit_when_a_range_is_explicit() {
        // Given the same oversized file with a tail request for the last
        // two lines
        let tmp = tempfile::tempdir().unwrap();
        let path = oversized_fixture(tmp.path(), "tail.txt", 300, 500);
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -2,
            10,
            true,
        )
        .await;

        // Then exactly the last two lines come back with absolute numbering
        assert!(!result.is_error, "output was: {}", output_text(&result));
        let output = output_text(&result);
        assert_eq!(output.lines().count(), 2);
        assert!(output.starts_with("299\t0299"), "output was: {output}");
        assert!(output.contains("\n300\t0300"), "output was: {output}");
        // And nothing is flagged truncated: the window reaches EOF
        assert!(!result.truncated);
        assert!(result.note.is_none());
    }

    #[tokio::test]
    async fn should_refuse_whole_file_reads_above_the_limit_with_paging_guidance() {
        // Given an oversized file and a whole-file read with no explicit
        // range parameters
        let tmp = tempfile::tempdir().unwrap();
        let path = oversized_fixture(tmp.path(), "whole.txt", 300, 500);
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            DEFAULT_N_LINES,
            false,
        )
        .await;

        // Then the read is refused and the message points at paging
        assert!(result.is_error);
        let output = output_text(&result);
        assert!(
            output.contains("exceeds the whole-file read limit"),
            "output was: {output}"
        );
        assert!(output.contains("n_lines"), "output was: {output}");
        assert!(output.contains("line_offset"), "output was: {output}");
    }

    #[tokio::test]
    async fn should_allow_whole_file_reads_up_to_the_spec_limit_of_102400_bytes() {
        // Given a file sized exactly at the spec limit (100 KiB = 102400
        // bytes: 2048 lines of 50 bytes)
        let tmp = tempfile::tempdir().unwrap();
        let path = oversized_fixture(tmp.path(), "edge.txt", 2048, 50);
        let stat = std::fs::metadata(&path).unwrap();
        assert_eq!(stat.len(), 102_400);
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            DEFAULT_N_LINES,
            false,
        )
        .await;

        // Then the whole-file read succeeds (the previous limit of 100000
        // bytes refused files in the 100000..=102399 range)
        assert!(!result.is_error, "output was: {}", output_text(&result));
        let output = output_text(&result);
        // The line cap (1000 lines) alone would emit 54,000 bytes (49 content
        // + 3 line-number + 1 tab bytes per line), which exceeds the shared
        // budget; the byte budget therefore stops the window at line 927
        // (49,950 bytes, the next line would overshoot). The range below pins
        // that: 920 rules out a regression that delivers only a handful of
        // lines, 1000 is the line cap this read must not reach.
        let rendered_lines = output.lines().count();
        assert!(
            (920..1000).contains(&rendered_lines),
            "expected the budget to stop the window between 920 and 999 lines, got {rendered_lines}"
        );
        assert!(
            output.len() <= crate::tool_result_truncation::MAX_BYTES,
            "output was {} bytes, over the shared budget",
            output.len()
        );
        // A budget-stopped window is a truncation and says where it stopped.
        assert!(
            result.truncated,
            "a budget-limited read must report itself truncated"
        );
        let note = result.note.as_deref().expect("truncation note");
        assert!(
            note.contains("showing lines 1-"),
            "the note must name the delivered window, got: {note}"
        );
        assert!(
            note.contains("re-read from 'line_offset': 1 to page through the file"),
            "the note must point at paging from line 1, got: {note}"
        );
        assert!(output.starts_with("1\t0001"), "output was: {output}");
    }

    #[tokio::test]
    async fn should_refuse_a_paged_read_when_the_large_file_contains_nul_bytes() {
        // Given an oversized file whose middle holds a NUL byte
        let tmp = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for number in 1..=300 {
            content.push_str(&format!("{number:04}"));
            content.push_str(&"x".repeat(495));
            content.push('\n');
        }
        let mut bytes = content.into_bytes();
        bytes[75_000] = 0;
        let path = tmp.path().join("nul.txt");
        std::fs::write(&path, bytes).unwrap();
        let result =
            read_file_range(local_fs(), path.to_string_lossy().into_owned(), 1, 10, true).await;

        // Then even the paged read is refused — binary detection covers the
        // whole file, not just the returned window
        assert!(result.is_error);
        assert!(output_text(&result).contains("NUL bytes"));
    }

    #[tokio::test]
    async fn should_refuse_a_paged_read_when_the_large_file_is_not_valid_utf8() {
        // Given an oversized file with an invalid UTF-8 sequence near the
        // end
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        for number in 1..=300 {
            let prefix = format!("{number:04}");
            bytes.extend_from_slice(prefix.as_bytes());
            bytes.extend(std::iter::repeat_n(b'x', 500 - prefix.len() - 1));
            bytes.push(b'\n');
        }
        bytes[149_000] = 0xFF;
        let path = tmp.path().join("bad-utf8.txt");
        std::fs::write(&path, bytes).unwrap();
        let result =
            read_file_range(local_fs(), path.to_string_lossy().into_owned(), 1, 10, true).await;

        // Then the paged read is refused for non-UTF-8 content
        assert!(result.is_error);
        assert!(output_text(&result).contains("not valid UTF-8"));
    }

    /// Given the Read tool's parameter schema (Task 5, plan 2026-10-10-
    /// sandbox-path-permission D5), When the `path` description is
    /// inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = ReadTool::new(None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }

    #[test]
    fn should_state_the_per_call_line_cap_consistently_in_the_description_and_schema() {
        let tool = ReadTool::new(None, local_fs());
        let description = tool.description();

        // The default and the cap are the same number, stated once.
        assert!(
            description.contains("default 1000"),
            "the description must state the real default: {description}"
        );
        assert!(
            !description.contains("default 2000"),
            "the default must not contradict the per-call cap: {description}"
        );
        // The line-range idiom is claimed as Read's own job.
        assert!(
            description.contains("sed -n"),
            "the description must name the shell idiom it replaces: {description}"
        );

        let n_lines = tool.parameters()["properties"]["n_lines"]["description"]
            .as_str()
            .expect("n_lines description present");
        assert!(
            n_lines.contains("1000") && !n_lines.contains("2000"),
            "the schema must state 1000, not 2000: {n_lines}"
        );
    }

    /// Given a file whose first 1000 lines are far larger than the context
    /// budget (1000 lines x ~2000 chars)
    /// When reading it with an explicit whole-file range
    /// Then the output stays within the shared byte budget, so the runner
    /// truncator never has to replace it with a preview
    #[tokio::test]
    async fn should_keep_output_within_the_shared_budget_when_the_file_is_huge() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=1000)
            .map(|n| format!("{n:04} {}\n", "y".repeat(1995)))
            .collect::<String>();
        let path = write_fixture(tmp.path(), "huge.txt", &content);

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            MAX_LINES,
            true,
        )
        .await;

        assert!(!result.is_error);
        let bytes = output_text(&result).len();
        assert!(
            bytes <= crate::tool_result_truncation::MAX_BYTES,
            "Read output was {bytes} bytes, over the {} byte shared budget",
            crate::tool_result_truncation::MAX_BYTES
        );
        assert!(
            result.truncated,
            "a budget-limited read must report itself truncated"
        );
        let note = result.note.as_deref().expect("truncation note");
        assert!(
            note.contains("line_offset"),
            "the note must give the paging offset, got: {note}"
        );
    }

    /// Given a huge file where the byte budget stops the window mid-file
    /// When reading it
    /// Then the note states the delivered range and points at paging from
    /// line 1, so the model decides which span it still needs rather than
    /// being walked forward one window at a time (SDR-007 / ADR-9)
    #[tokio::test]
    async fn should_point_every_truncation_at_paging_from_line_one() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=1000)
            .map(|n| format!("{n:04} {}\n", "y".repeat(1995)))
            .collect::<String>();
        let path = write_fixture(tmp.path(), "stop.txt", &content);

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            MAX_LINES,
            true,
        )
        .await;

        let delivered: Vec<usize> = output_text(&result)
            .lines()
            .filter_map(|line| line.split('\t').next()?.parse().ok())
            .collect();
        let first = *delivered.first().expect("at least one delivered line");
        let last = *delivered.last().expect("at least one delivered line");
        let note = result.note.as_deref().expect("truncation note");
        assert!(
            note.contains(&format!("showing lines {first}-{last} of 1000")),
            "the note must name the delivered window, got: {note}"
        );
        assert!(
            note.contains("re-read from 'line_offset': 1 to page through the file"),
            "the note must point at paging from line 1, got: {note}"
        );
        assert!(
            !note.contains(&format!("'line_offset': {}", last + 1)),
            "the note must not auto-chain past the delivered window, got: {note}"
        );
    }

    /// Given a small file that fits comfortably
    /// When reading it whole
    /// Then it is delivered complete and not flagged truncated
    #[tokio::test]
    async fn should_deliver_a_small_file_whole_within_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "small.txt", "one\ntwo\nthree\n");

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            10,
            false,
        )
        .await;

        assert!(!result.is_error);
        assert!(!result.truncated);
        assert_eq!(output_text(&result), "1\tone\n2\ttwo\n3\tthree\n");
    }

    /// Given an offset past the end of the file
    /// When reading it
    /// Then the result is empty and carries no malformed line range
    #[tokio::test]
    async fn should_report_no_line_range_when_the_offset_is_past_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "past.txt", "only\n");

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            6,
            10,
            false,
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(output_text(&result), "");
        if let Some(note) = result.note.as_deref() {
            assert!(
                !note.contains("lines 6-5") && !note.contains("lines 1-0"),
                "an empty window must not report an inverted range, got: {note}"
            );
        }
    }

    /// Given a file with a large tail
    /// When reading it with a negative offset asking for the last lines
    /// Then the delivered window still ends at the file's last line — the
    /// byte budget trims from the head, never from the tail — and the note
    /// states the delivered suffix and points at paging from line 1 rather
    /// than at the line just before the window (SDR-007 / ADR-9)
    #[tokio::test]
    async fn should_keep_the_window_at_the_file_tail_when_reading_with_a_negative_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=400)
            .map(|n| format!("{n:04} {}\n", "w".repeat(500)))
            .collect::<String>();
        let path = write_fixture(tmp.path(), "tail.txt", &content);

        // Ask for the last 100 lines; the rendered window is ~50 KB at most.
        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -100,
            100,
            true,
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let last_no: usize = text
            .lines()
            .last()
            .and_then(|line| line.split('\t').next()?.parse().ok())
            .expect("at least one delivered line");
        assert_eq!(
            last_no, 400,
            "a tail read must end at the file's last line, got {last_no}"
        );
        // Rendering all 100 lines would be ~50,505 bytes, just over the
        // 50,000-byte budget, so the head is trimmed to the 98-line suffix
        // (49,980 bytes) and reported. The tail itself never moves.
        assert!(
            result.truncated,
            "the budget trimmed the head, so the read is truncated"
        );
        assert!(
            text.starts_with("303\t0303"),
            "output should start at line 303, got: {}",
            &text[..40]
        );
        assert_eq!(text.lines().count(), 98);
        let note = result.note.as_deref().expect("truncation note");
        assert!(
            note.contains("showing lines 303-400 of 400"),
            "the note must name the delivered suffix, got: {note}"
        );
        assert!(
            note.contains("re-read from 'line_offset': 1 to page through the file"),
            "the note must point at paging from line 1, got: {note}"
        );
        // The note must not hand back a page chain: naming the line just
        // before the window is what made each re-read walk one window
        // further back until the model had paged the whole file.
        assert!(
            !note.contains("'line_offset': 302"),
            "the note must not anchor the next read just before the window, \
             got: {note}"
        );
    }

    /// Given a file far longer than one call can return
    /// When reading it with a tail offset larger than the per-call line cap
    /// Then the window keeps the cap's worth of lines at the head of the
    /// requested tail, reports truncation, and points at paging from line 1
    /// like every other truncation — the undelivered lines below the window
    /// are never handed to the model as the next offset (SDR-007 / ADR-9)
    #[tokio::test]
    async fn should_page_from_line_one_when_a_tail_read_hits_the_line_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(
            tmp.path(),
            "cap-tail.txt",
            &(1..=5000).map(|n| format!("{n}\n")).collect::<String>(),
        );

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -5000,
            MAX_LINES,
            true,
        )
        .await;

        assert!(!result.is_error);
        assert!(result.truncated);
        let text = output_text(&result);
        let delivered: Vec<usize> = text
            .lines()
            .filter_map(|line| line.split('\t').next()?.parse().ok())
            .collect();
        assert_eq!(delivered.first(), Some(&1));
        assert_eq!(delivered.last(), Some(&1000));
        let note = result.note.as_deref().expect("truncation note");
        assert_eq!(
            note,
            "Read truncated: showing lines 1-1000 of 5000; re-read from 'line_offset': 1 to page through the file"
        );
    }

    /// Given a file of 1000 very wide lines (1995 chars each) that cannot
    /// fit the byte budget as a whole tail
    /// When reading it with `line_offset = -1000`
    /// Then the byte budget trims the *head* of the tail window: the output
    /// fits, ends at the file's last line, reports truncation, and the note
    /// says which suffix was delivered and points at paging from line 1
    #[tokio::test]
    async fn should_trim_the_head_of_a_wide_tail_read_to_stay_within_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=1000)
            .map(|n| format!("{n:04} {}\n", "z".repeat(1995)))
            .collect::<String>();
        let path = write_fixture(tmp.path(), "wide-tail.txt", &content);

        let result = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -1000,
            DEFAULT_N_LINES,
            true,
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.len() <= crate::tool_result_truncation::MAX_BYTES,
            "tail read was {} bytes, over the {} byte shared budget",
            text.len(),
            crate::tool_result_truncation::MAX_BYTES
        );
        assert!(
            result.truncated,
            "a budget-trimmed tail read must report itself truncated"
        );
        let delivered: Vec<usize> = text
            .lines()
            .filter_map(|line| line.split('\t').next()?.parse().ok())
            .collect();
        let first = *delivered.first().expect("at least one delivered line");
        let last = *delivered.last().expect("at least one delivered line");
        // The window still ends at the file's last line — only its head was
        // trimmed. 24 wide lines (48,121 bytes) is what the budget admits: a
        // 25th line would push it to 50,125 bytes.
        assert_eq!(last, 1000, "the tail must not move away from EOF");
        assert_eq!(
            first, 977,
            "the budget must trim the head to the 977-1000 suffix"
        );
        assert_eq!(delivered.len(), 24);
        assert!(
            text.len() <= 49_000,
            "the trimmed tail should sit just under the budget, got {} bytes",
            text.len()
        );
        let note = result.note.as_deref().expect("truncation note");
        assert!(
            note.contains(&format!("lines {first}-{last} of 1000")),
            "the note must name the delivered suffix, got: {note}"
        );
        // Every truncation points at page-one paging: the note neither walks
        // past EOF nor anchors the next read just above the window.
        assert!(
            note.contains("re-read from 'line_offset': 1 to page through the file"),
            "the note must point at paging from line 1, got: {note}"
        );
        assert!(
            !note.contains(&format!("'line_offset': {}", last + 1)),
            "a tail read must not be told to continue past EOF, got: {note}"
        );
    }

    /// Given a file of 400 wide lines whose tail window is trimmed by the
    /// byte budget
    /// When reading the last 100 lines with a negative offset and following
    /// the note's guidance to page from line 1
    /// Then the second read starts at line 1 and reaches the lines above the
    /// first window by the model's own paging, not by a chained offset that
    /// only ever reaches the previous slice
    #[tokio::test]
    async fn should_reach_lines_above_the_window_when_following_a_tail_note() {
        let tmp = tempfile::tempdir().unwrap();
        let content = (1..=400)
            .map(|n| format!("{n:04} {}\n", "v".repeat(500)))
            .collect::<String>();
        let path = write_fixture(tmp.path(), "tail-advance.txt", &content);

        let first_read = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            -100,
            100,
            true,
        )
        .await;
        assert!(!first_read.is_error);
        assert!(first_read.truncated, "the head must be trimmed");
        let first_text = output_text(&first_read);
        let first_start = first_delivered_line(&first_text);
        let last_start = first_text.lines().last().map(first_delivered_line);
        assert_eq!(
            last_start,
            Some(400),
            "the tail window must end at the file's last line"
        );
        assert!(
            first_start > 1,
            "the fixture must force a head trim, got start {first_start}"
        );
        let note = first_read.note.as_deref().expect("truncation note");
        assert!(
            note.contains("re-read from 'line_offset': 1 to page through the file"),
            "the note must point at paging from line 1, got: {note}"
        );

        // Follow the note literally: page from line 1. The undelivered lines
        // above the tail window are reachable from there, and the model keeps
        // control of how far it pages.
        let second_read = read_file_range(
            local_fs(),
            path.to_string_lossy().into_owned(),
            1,
            100,
            true,
        )
        .await;
        assert!(!second_read.is_error);
        let second_start = first_delivered_line(&output_text(&second_read));
        assert_eq!(
            second_start, 1,
            "paging from line 1 must start at line 1, got {second_start}"
        );
        assert!(
            second_start < first_start,
            "paging from line 1 must reach lines above the delivered window \
             (start {first_start}), got {second_start}"
        );
    }
}
