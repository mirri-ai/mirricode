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
const DEFAULT_N_LINES: u64 = 2000;

/// Maximum line length: longer lines are truncated with a `... (truncated)`
/// suffix so a single pathological line cannot blow up the output.
const MAX_LINE_LENGTH: usize = 2000;

/// Maximum number of lines returned per call, regardless of `n_lines`.
const MAX_LINES: u64 = 1000;

/// Maximum file size Read accepts for a whole-file read (100 KiB, the spec
/// limit — the rendered-window cap, applied here to the input only when the
/// model did not explicitly narrow the request). Explicit `line_offset` /
/// `n_lines` requests stream through files of any size.
const MAX_BYTES: u64 = 100 * 1024;

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
         Use 'line_offset' (1-based, default 1; negative values read from the \
         end, e.g. -100 reads the last 100 lines) and 'n_lines' (default 2000, \
         capped at 1000 per call) to page through large files. Whole-file \
         reads of files larger than 100 KB are refused — re-read those with \
         'line_offset'/'n_lines'. Refuses directories, binary files (NUL \
         bytes), non-UTF-8 content, and image/video files (use ReadMediaFile \
         for those). Lines longer than 2000 characters are truncated."
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
                        "description": "Maximum number of lines to show. Defaults to 2000."
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
        Ok(stat) if stat.size > MAX_BYTES && !explicit_range => {
            return ToolResult::error(format!(
                "Read refused '{path}': file is {} bytes, which exceeds the \
                 whole-file read limit of {MAX_BYTES} bytes (100 KB). Re-read \
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
    let any_line_truncated = shown
        .iter()
        .any(|line| line.chars().count() > MAX_LINE_LENGTH);

    let mut output = String::new();
    for (index, line) in shown.iter().enumerate() {
        output.push_str(&format!(
            "{}\t{}\n",
            sink.start() + index + 1,
            truncate_line(line)
        ));
    }

    let truncated = sink.end() < sink.total() || any_line_truncated;
    let note = truncated.then(|| {
        let mut note = format!(
            "Read truncated: showing lines {}-{} of {}; re-read with \
             'line_offset' to continue",
            sink.start() + 1,
            sink.end(),
            sink.total()
        );
        if any_line_truncated {
            note.push_str(&format!(
                "; some lines exceed {MAX_LINE_LENGTH} characters and were truncated"
            ));
        }
        note
    });

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
    async fn should_set_truncated_flag_and_note_when_file_is_longer_than_requested_range() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "d.txt", "1\n2\n3\n4\n5\n");
        let result =
            read_file_range(local_fs(), path.to_string_lossy().into_owned(), 1, 2, false).await;
        assert!(!result.is_error);
        assert!(result.truncated);
        let note = result.note.clone().expect("truncation note");
        assert!(note.contains("lines 1-2 of 5"), "note was: {note}");
        assert_eq!(output_text(&result), "1\t1\n2\t2\n");
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-alias".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-canonical".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
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
        // And the truncation note still reports the exact file-wide total
        assert!(result.truncated);
        let note = result.note.clone().expect("truncation note");
        assert!(note.contains("lines 101-103 of 300"), "note was: {note}");
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
        assert_eq!(output.lines().count(), 1000);
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
}
