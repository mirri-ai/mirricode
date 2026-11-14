//! Tool result truncation — model-context truncation for oversized tool results.
//!
//! When a tool produces text output exceeding [`MAX_BYTES`], the full text is
//! persisted to disk and the model-visible output is replaced with a bounded
//! preview containing metadata and a recovery path. This prevents oversized
//! tool results from consuming disproportionate context window space while
//! preserving the complete output for follow-up retrieval via `Read`.
//!
//! Ported from the TypeScript `toolResultTruncation` domain
//! (`agent-core-v2/src/agent/toolResultTruncation/toolResultTruncationService.ts`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::SyncFs;
use uuid::Uuid;

use crate::tool::{ToolOutput, ToolResult};

/// Maximum UTF-8 **bytes** of tool output text allowed inline in model context.
///
/// Outputs at or below this size pass through unchanged. Outputs strictly
/// larger than this are persisted to disk and replaced with a preview.
///
/// The unit is bytes, not characters: the budget models context-window
/// consumption, which is byte-shaped. A CJK character costs three bytes, so
/// character-based accounting would over-admit Chinese text by 3x. Tool
/// output that claims a "characters" figure must therefore say characters
/// only where it really counts characters (see [`PREVIEW_CHARS`]).
pub const MAX_BYTES: usize = 50_000;

/// Number of **characters** of preview text included in the truncated
/// replacement shown to the model.
///
/// Character-based on purpose: this is a readability truncation, and slicing
/// mid-codepoint would produce invalid UTF-8.
pub const PREVIEW_CHARS: usize = 2_000;

/// Persists oversized tool result text to disk and replaces the inline output
/// with a bounded, recoverable preview.
///
/// The truncator is stateless beyond its `home_dir` configuration — each call
/// to [`truncate_for_model`](Self::truncate_for_model) is independent and
/// produces a unique file path via a UUID suffix.
#[derive(Clone)]
pub struct ToolResultTruncator {
    /// Root directory for persisted tool result files.
    /// Files are written under `{home_dir}/tool-results/`.
    pub home_dir: PathBuf,
    /// Filesystem provider backing truncation persistence (zero-IO T13c;
    /// the engine default is `LocalSyncFs`).
    fs: Arc<dyn SyncFs>,
}

impl std::fmt::Debug for ToolResultTruncator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolResultTruncator")
            .field("home_dir", &self.home_dir)
            .finish_non_exhaustive()
    }
}

impl ToolResultTruncator {
    /// Create a new truncator that persists files under
    /// `{home_dir}/tool-results/`.
    ///
    /// Filesystem access goes through the host-local `LocalSyncFs`; inject a
    /// double with [`ToolResultTruncator::with_fs`] instead.
    pub fn new(home_dir: impl Into<PathBuf>) -> Self {
        Self::with_fs(home_dir, Arc::new(mirri_os::LocalSyncFs::new()))
    }

    /// Create a new truncator whose persistence goes through `fs`.
    pub fn with_fs(home_dir: impl Into<PathBuf>, fs: Arc<dyn SyncFs>) -> Self {
        Self {
            home_dir: home_dir.into(),
            fs,
        }
    }

    /// Truncate an oversized tool result for model consumption.
    ///
    /// # Rules
    ///
    /// - Only [`ToolOutput::Text`] is considered for truncation;
    ///   [`ToolOutput::Parts`] is returned unchanged regardless of size.
    /// - If the body byte length is `<= MAX_BYTES`, the result is returned
    ///   unchanged.
    /// - The `truncated` flag does **not** exempt a result: sizing is decided
    ///   by the real byte count alone.
    /// - Otherwise, the full text is saved to
    ///   `{home_dir}/tool-results/{safe_stem}-{uuid}.txt` and the output is
    ///   replaced with a preview containing metadata, a recovery path, and the
    ///   first [`PREVIEW_CHARS`] characters. `truncated` is set to `true`.
    /// - The producer's `note`, if any, does not survive alongside the
    ///   preview: its text moves into the preview as a
    ///   `note_before_truncation` field and `note` becomes `None`.
    ///
    /// If the file write fails (e.g. disk full, permission denied), the
    /// original result is returned unchanged rather than losing the output.
    pub fn truncate_for_model(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        mut result: ToolResult,
    ) -> ToolResult {
        let text = match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => return result,
        };

        // Size is the only exemption. A producer's `truncated` flag is a
        // description of what it did, not a permission slip: an over-budget
        // result must be paged down regardless of how the producer labelled
        // itself, or a producer that truncates only its *line count* (Read
        // caps at 1000 lines x 2000 chars = ~2 MB) would push the whole thing
        // into the context window.
        if text.len() <= MAX_BYTES {
            return result;
        }

        let stem = safe_file_stem(tool_name, tool_call_id);
        let uuid = Uuid::new_v4();
        let filename = format!("{stem}-{uuid}.txt");

        let tool_results_dir = self.home_dir.join("tool-results");
        let output_path = tool_results_dir.join(&filename);

        // Best-effort persistence: if directory creation or the write fails,
        // return the original result rather than losing the output entirely.
        if self.fs.mkdir(&tool_results_dir, true).is_err() {
            return result;
        }
        if self.fs.write_text(&output_path, &text).is_err() {
            return result;
        }

        let preview = render_preview(
            tool_name,
            tool_call_id,
            &text,
            &output_path,
            result.note.as_deref(),
        );

        result.output = ToolOutput::Text(preview);
        // The note describes the output it shipped with. Once that output is
        // replaced by a preview the two would contradict each other (a Read
        // note claiming "lines 1-100 of 1000" above a 3-line preview), so the
        // preview becomes the single authority on what was delivered. The
        // note's facts survive inside the preview as `note_before_truncation`
        // rather than being dropped. Tool-agnostic: any producer's note — Read
        // line ranges, grep cutoffs — takes the same path.
        result.note = None;
        result.truncated = true;
        result
    }
}

/// Render the truncated preview string shown to the model.
///
/// The preview contains:
/// - A truncation notice naming the threshold.
/// - Metadata fields: `tool_name`, `tool_call_id`, `output_size_chars`,
///   `output_size_bytes`, `output_path`.
/// - `note_before_truncation` carrying the producer's original note, when it
///   had one (see [`ToolResultTruncator::truncate_for_model`]).
/// - A `next_step` instruction directing the model to use `Read` with the
///   `output_path` for full content.
/// - The first [`PREVIEW_CHARS`] characters of the original text under a
///   `[preview]` header.
fn render_preview(
    tool_name: &str,
    tool_call_id: &str,
    text: &str,
    output_path: &Path,
    note: Option<&str>,
) -> String {
    let output_size_chars = text.chars().count();
    let output_size_bytes = text.len();
    let preview_text: String = text.chars().take(PREVIEW_CHARS).collect();

    let mut lines = vec![
        format!("Tool output exceeded {MAX_BYTES} bytes; showing a preview only."),
        format!("tool_name: {tool_name}"),
        format!("tool_call_id: {tool_call_id}"),
        format!("output_size_chars: {output_size_chars}"),
        format!("output_size_bytes: {output_size_bytes}"),
        format!("output_path: {}", output_path.display()),
    ];
    if let Some(note) = note {
        lines.push(format!("note_before_truncation: {note}"));
    }
    lines.extend([
        "next_step: Use Read with output_path to page through the full output.".to_string(),
        String::new(),
        "[preview]".to_string(),
        preview_text,
    ]);
    lines.join("\n")
}

/// Sanitize a tool name and call ID into a safe file stem.
///
/// Replaces any run of characters outside `[a-zA-Z0-9._-]` with a single
/// `_`, trims leading and trailing underscores, and truncates to 80
/// characters. Falls back to `"tool-result"` if the result is empty or
/// reduces to only the separator dash between name and call id.
pub fn safe_file_stem(tool_name: &str, tool_call_id: &str) -> String {
    let combined = format!("{tool_name}-{tool_call_id}");

    // Replace runs of invalid characters with a single underscore, preserving
    // valid characters (alphanumeric, dot, dash, underscore) as-is.
    let mut sanitized = String::with_capacity(combined.len());
    let mut in_invalid_run = false;
    for ch in combined.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            sanitized.push(ch);
            in_invalid_run = false;
        } else if !in_invalid_run {
            sanitized.push('_');
            in_invalid_run = true;
        }
    }

    // Trim leading and trailing underscores.
    let trimmed = sanitized.trim_matches('_');
    // A stem that is empty — or contains only the separator dash from the
    // `{tool_name}-{tool_call_id}` join — carries no usable label (an
    // all-special-characters name/call-id pair reduces to just the dash);
    // fall back to the default stem so the persisted filename stays
    // meaningful.
    if trimmed.is_empty() || trimmed.chars().all(|ch| ch == '-') {
        return "tool-result".to_string();
    }

    // Truncate to 80 characters (by Unicode scalar value, matching the TS
    // `slice(0, 80)` behavior for BMP text).
    let truncated: String = trimmed.chars().take(80).collect();
    if truncated.is_empty() {
        "tool-result".to_string()
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolOutput, ToolResult};

    /// Helper: create a `ToolResult` with the given text output.
    fn text_result(text: &str) -> ToolResult {
        ToolResult {
            output: ToolOutput::Text(text.to_string()),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        }
    }

    /// Helper: extract the `output_path` value from a rendered preview string.
    fn extract_output_path(rendered: &str) -> &str {
        for line in rendered.lines() {
            if let Some(rest) = line.strip_prefix("output_path: ") {
                return rest;
            }
        }
        panic!("expected rendered output to include output_path, got:\n{rendered}");
    }

    #[test]
    fn should_return_unchanged_when_text_under_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let result = text_result("small output");

        let truncated = truncator.truncate_for_model("Read", "call_1", result);

        match &truncated.output {
            ToolOutput::Text(text) => assert_eq!(text, "small output"),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
        assert!(!truncated.truncated);
    }

    #[test]
    fn should_return_unchanged_when_text_exactly_at_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = "x".repeat(MAX_BYTES);
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Read", "call_1", result);

        assert!(!truncated.truncated);
        match &truncated.output {
            ToolOutput::Text(t) => assert_eq!(t.len(), MAX_BYTES),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    #[test]
    fn should_truncate_when_text_exceeds_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = "x".repeat(MAX_BYTES + 1);
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Bash", "call_2", result);

        assert!(truncated.truncated);
        match &truncated.output {
            ToolOutput::Text(rendered) => {
                assert!(rendered.contains("Tool output exceeded 50000 bytes"));
                assert!(rendered.contains("tool_name: Bash"));
                assert!(rendered.contains("tool_call_id: call_2"));
                assert!(rendered.contains("output_size_chars:"));
                assert!(rendered.contains("output_size_bytes:"));
                assert!(rendered.contains("output_path:"));
                assert!(rendered.contains("next_step:"));
                assert!(rendered.contains("[preview]"));
            }
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    #[test]
    fn should_return_unchanged_when_output_is_parts() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let parts = vec![crate::message::ContentPart::Text {
            text: "x".repeat(MAX_BYTES + 1),
        }];
        let result = ToolResult {
            output: ToolOutput::Parts(parts),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        };

        let truncated = truncator.truncate_for_model("Read", "call_4", result);

        assert!(!truncated.truncated);
        assert!(matches!(truncated.output, ToolOutput::Parts(_)));
    }

    #[test]
    fn should_persist_full_text_to_disk_when_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = format!("{}tail survives on disk", "x".repeat(MAX_BYTES + 1));
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Lookup", "call_5", result);

        assert!(truncated.truncated);
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let output_path = extract_output_path(rendered);
        let persisted = std::fs::read_to_string(output_path).unwrap();
        assert_eq!(persisted, text);
    }

    #[test]
    fn should_preserve_is_error_flag_when_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = "x".repeat(MAX_BYTES + 1);
        let result = ToolResult {
            output: ToolOutput::Text(text),
            is_error: true,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        };

        let truncated = truncator.truncate_for_model("Bash", "call_6", result);

        assert!(truncated.is_error);
        assert!(truncated.truncated);
    }

    #[test]
    fn should_use_unique_filenames_when_truncating_repeated_call_ids() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());

        let text_a = format!("{}first", "a".repeat(MAX_BYTES + 1));
        let text_b = format!("{}second", "b".repeat(MAX_BYTES + 1));

        let result_a = truncator.truncate_for_model("Lookup", "call_repeat", text_result(&text_a));
        let result_b = truncator.truncate_for_model("Lookup", "call_repeat", text_result(&text_b));

        let rendered_a = match &result_a.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let rendered_b = match &result_b.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };

        let path_a = extract_output_path(rendered_a);
        let path_b = extract_output_path(rendered_b);
        assert_ne!(path_a, path_b, "repeated calls must produce unique files");

        let persisted_a = std::fs::read_to_string(path_a).unwrap();
        let persisted_b = std::fs::read_to_string(path_b).unwrap();
        assert!(persisted_a.ends_with("first"));
        assert!(persisted_b.ends_with("second"));
    }

    #[test]
    fn should_sanitize_tool_name_and_call_id_when_building_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = "x".repeat(MAX_BYTES + 1);
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Lookup Tool", "call:lookup", result);

        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let output_path = extract_output_path(rendered);
        assert!(
            output_path.contains("Lookup_Tool-call_lookup"),
            "expected sanitized stem in path, got: {output_path}"
        );
    }

    #[test]
    fn should_include_preview_text_when_truncating_oversized_output() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let prefix = "PREVIEW_CONTENT_HERE";
        let text = format!("{prefix}{}", "x".repeat(MAX_BYTES + 1));
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Read", "call_7", result);

        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        // The preview should contain the first PREVIEW_CHARS chars of the text,
        // which starts with our prefix.
        assert!(rendered.contains(prefix));
        // The preview should not contain text beyond PREVIEW_CHARS.
        // The full text has MAX_BYTES + 1 + prefix.len() bytes, so the tail
        // 'x's beyond PREVIEW_CHARS should not appear in the preview section.
        let preview_section = rendered.split("[preview]\n").nth(1).unwrap_or("");
        assert!(preview_section.starts_with(prefix));
        assert!(preview_section.len() <= PREVIEW_CHARS + prefix.len());
    }

    #[test]
    fn should_report_correct_char_and_byte_counts_when_text_is_multibyte() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        // Use multi-byte characters to verify char vs byte distinction.
        let text = "é".repeat(MAX_BYTES + 1); // é is 2 bytes in UTF-8
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Read", "call_8", result);

        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        assert!(rendered.contains(&format!("output_size_chars: {}", MAX_BYTES + 1)));
        assert!(rendered.contains(&format!("output_size_bytes: {}", (MAX_BYTES + 1) * 2)));
    }

    #[test]
    fn should_include_first_preview_chars_when_text_has_unicode() {
        // Given an oversized result whose text is made of multi-byte characters
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = "é".repeat(MAX_BYTES + 1); // é is 2 bytes in UTF-8
        let result = text_result(&text);

        // When truncating for the model
        let truncated = truncator.truncate_for_model("Read", "call_unicode", result);

        // Then the preview keeps exactly PREVIEW_CHARS characters — slicing
        // must be char-based, not byte-based (byte slicing would halve it)
        assert!(truncated.truncated);
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let preview_section = rendered.split("[preview]\n").nth(1).unwrap_or("");
        assert_eq!(preview_section.chars().count(), PREVIEW_CHARS);
        assert!(preview_section.chars().all(|c| c == 'é'));
    }

    #[test]
    fn should_handle_newlines_in_preview_correctly_when_text_has_many_lines() {
        // Given an oversized result whose text is many newline-terminated lines
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let line = "line-of-tool-output\n";
        let text = line.repeat((MAX_BYTES / line.len()) + 5);
        let result = text_result(&text);

        // When truncating for the model
        let truncated = truncator.truncate_for_model("Bash", "call_lines", result);

        // Then the preview keeps exactly PREVIEW_CHARS chars with newlines
        // intact inside it
        assert!(truncated.truncated);
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let preview_section = rendered.split("[preview]\n").nth(1).unwrap_or("");
        assert_eq!(preview_section.chars().count(), PREVIEW_CHARS);
        assert!(
            preview_section.contains('\n'),
            "preview must preserve the newlines of the original text"
        );
    }

    #[test]
    fn should_use_default_stem_when_tool_name_and_call_id_are_all_special_chars() {
        // Given tool name and call id made entirely of characters outside
        // the safe filename alphabet
        // When building a safe file stem from them
        // Then the default fallback stem is used
        assert_eq!(safe_file_stem("!!!", "@@@"), "tool-result");
        assert_eq!(safe_file_stem("日本語", "漢字"), "tool-result");
    }

    #[test]
    fn should_truncate_stem_to_80_chars_when_combined_name_exceeds_limit() {
        // Given a tool name whose combined label with the call id is far
        // longer than the 80-character stem limit
        let long_tool_name = "tool-".repeat(30);
        let call_id = "call-12345";

        // When building a safe file stem from them
        let stem = safe_file_stem(&long_tool_name, call_id);

        // Then the stem is truncated to exactly 80 characters and still
        // carries the label's leading content
        assert_eq!(stem.chars().count(), 80);
        assert!(
            stem.starts_with("tool-tool"),
            "stem must keep prefix, got: {stem}"
        );
    }

    // -----------------------------------------------------------------------
    // T-13c1-3 — in-memory SyncFs double (zero-IO T13c)
    // -----------------------------------------------------------------------

    /// Recording [`SyncFs`] double: nothing is written to any real
    /// filesystem — the mkdir / write_text calls are captured so tests can
    /// assert the truncation layer went through the injected provider with
    /// the exact paths.
    struct RecordingFs {
        mkdir_calls: std::sync::Mutex<Vec<PathBuf>>,
        write_text_calls: std::sync::Mutex<Vec<(PathBuf, String)>>,
    }

    impl RecordingFs {
        fn new() -> Self {
            Self {
                mkdir_calls: std::sync::Mutex::new(Vec::new()),
                write_text_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn mkdir_calls(&self) -> Vec<PathBuf> {
            self.mkdir_calls.lock().unwrap().clone()
        }

        fn write_text_calls(&self) -> Vec<(PathBuf, String)> {
            self.write_text_calls.lock().unwrap().clone()
        }
    }

    impl SyncFs for RecordingFs {
        fn mkdir(&self, path: &Path, _recursive: bool) -> anyhow::Result<()> {
            self.mkdir_calls.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }

        fn write_text(&self, path: &Path, content: &str) -> anyhow::Result<()> {
            self.write_text_calls
                .lock()
                .unwrap()
                .push((path.to_path_buf(), content.to_string()));
            Ok(())
        }
    }

    #[test]
    fn should_persist_truncated_output_through_the_injected_sync_fs_double() {
        // Given a truncator whose filesystem is a recording in-memory double
        let home = PathBuf::from("/virtual/home");
        let fs = std::sync::Arc::new(RecordingFs::new());
        let truncator = ToolResultTruncator::with_fs(
            home.clone(),
            std::sync::Arc::clone(&fs) as std::sync::Arc<dyn SyncFs>,
        );

        // When an oversized result is truncated for the model
        let text = format!("{}tail survives the cut", "x".repeat(MAX_BYTES + 1));
        let truncated = truncator.truncate_for_model("Lookup", "call_1", text_result(&text));

        // Then the tool-results directory is created through the double
        assert_eq!(fs.mkdir_calls(), vec![home.join("tool-results")]);

        // And the full text is written exactly once, under the expected
        // directory, with the sanitized stem and a unique uuid filename
        let writes = fs.write_text_calls();
        assert_eq!(writes.len(), 1, "truncation must persist exactly once");
        let (path, persisted) = &writes[0];
        assert_eq!(persisted, &text, "the full text must be persisted");
        assert_eq!(
            path.parent(),
            Some(home.join("tool-results").as_path()),
            "the persisted file must live under tool-results"
        );
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("Lookup-call_1-") && name.ends_with(".txt"),
            "unexpected filename: {name}"
        );

        // And the preview rendered for the model points at exactly that path
        assert!(truncated.truncated);
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        assert!(rendered.contains(&format!("output_path: {}", path.display())));
    }

    #[test]
    fn should_not_persist_when_write_fails_through_the_double() {
        // Given a truncator whose injected filesystem always fails the write
        struct FailingWriteFs;
        impl SyncFs for FailingWriteFs {
            fn mkdir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
                Ok(())
            }
            fn write_text(&self, _path: &Path, _content: &str) -> anyhow::Result<()> {
                Err(anyhow::anyhow!("disk full"))
            }
        }
        let truncator = ToolResultTruncator::with_fs(
            PathBuf::from("/virtual/home"),
            std::sync::Arc::new(FailingWriteFs) as std::sync::Arc<dyn SyncFs>,
        );

        // When an oversized result is truncated
        let text = "x".repeat(MAX_BYTES + 1);
        let truncated = truncator.truncate_for_model("Read", "call_2", text_result(&text));

        // Then the original output is returned unchanged rather than lost
        assert!(!truncated.truncated);
        match &truncated.output {
            ToolOutput::Text(t) => assert_eq!(t, &text),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    /// Given a result whose text is 20,000 Chinese characters (60,000 bytes)
    /// When truncating for the model with a 50,000-byte budget
    /// Then it IS truncated, because bytes — not characters — measure cost
    #[test]
    fn should_measure_budget_in_bytes_when_text_is_multibyte() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        // 20,000 CJK chars = 60,000 bytes: past the 50,000-byte budget,
        // even though the character count is well under it.
        let text = "中".repeat(20_000);
        let result = text_result(&text);

        let truncated = truncator.truncate_for_model("Read", "call_cjk", result);

        assert!(
            truncated.truncated,
            "60,000 bytes must exceed a 50,000-byte budget regardless of character count"
        );
    }

    /// Given a result that self-reports truncation but is still far over the
    /// byte budget (Read caps at 1000 lines x 2000 chars = ~2 MB)
    /// When truncating for the model
    /// Then it IS truncated, because the producer's flag is not a permission
    /// slip
    #[test]
    fn should_truncate_an_over_budget_result_even_when_it_self_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let result = ToolResult {
            output: ToolOutput::Text("x".repeat(MAX_BYTES + 1)),
            is_error: false,
            note: Some("Read truncated: showing lines 1-1000 of 1200".to_string()),
            stop_turn: false,
            truncated: true,
            injection: None,
        };

        let out = truncator.truncate_for_model("Read", "call_self", result);

        assert!(out.truncated);
        match &out.output {
            ToolOutput::Text(t) => assert!(
                t.contains("Tool output exceeded"),
                "an over-budget result must be paged down even when it claims \
                 to be truncated already"
            ),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    /// Given an over-budget result that self-reports truncation
    /// When truncating for the model
    /// Then the full text is still persisted, so the model can page to it
    #[test]
    fn should_persist_the_full_text_when_an_over_budget_result_self_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let text = format!("{}tail survives", "x".repeat(MAX_BYTES + 1));
        let result = ToolResult {
            output: ToolOutput::Text(text.clone()),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: true,
            injection: None,
        };

        let out = truncator.truncate_for_model("Read", "call_self_disk", result);

        let rendered = match &out.output {
            ToolOutput::Text(t) => t.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        let persisted_path = rendered
            .lines()
            .find_map(|line| line.strip_prefix("output_path: "))
            .expect("preview must name the persisted file");
        let persisted = std::fs::read_to_string(persisted_path).unwrap();
        assert_eq!(
            persisted, text,
            "the full over-budget text must still be recoverable from disk"
        );
    }

    // -----------------------------------------------------------------------
    // Note rewriting: the preview is the single authority on what was sent
    // -----------------------------------------------------------------------

    /// Given a result whose note says the output was cut to lines 1-100 of
    /// 1000, and whose text exceeds the byte budget
    /// When truncating for the model
    /// Then `note` is cleared and the original note survives inside the
    /// preview as `note_before_truncation`, so the model sees one consistent
    /// account of what it received rather than a note contradicting the
    /// preview.
    #[test]
    fn should_move_the_note_into_the_preview_and_clear_it_when_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let original_note = "Read lines 1-100 of 1000.";
        let result = ToolResult {
            output: ToolOutput::Text("x".repeat(MAX_BYTES + 1)),
            is_error: false,
            note: Some(original_note.to_string()),
            stop_turn: false,
            truncated: false,
            injection: None,
        };

        let truncated = truncator.truncate_for_model("Read", "call_note", result);

        assert!(
            truncated.note.is_none(),
            "the producer's note must not survive alongside the preview"
        );
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        assert!(
            rendered.contains(&format!("note_before_truncation: {original_note}")),
            "the original note must be preserved inside the preview, got:\n{rendered}"
        );
    }

    /// Given a result with no note whose text exceeds the byte budget
    /// When truncating for the model
    /// Then the preview carries no `note_before_truncation` line at all —
    /// not an empty one — and `note` stays `None`.
    #[test]
    fn should_omit_the_note_line_when_truncating_a_result_without_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let result = text_result(&"x".repeat(MAX_BYTES + 1));

        let truncated = truncator.truncate_for_model("Bash", "call_no_note", result);

        assert!(truncated.note.is_none());
        let rendered = match &truncated.output {
            ToolOutput::Text(t) => t,
            ToolOutput::Parts(_) => panic!("expected text output"),
        };
        assert!(
            !rendered.contains("note_before_truncation:"),
            "a note-less result must not gain an empty note line, got:\n{rendered}"
        );
    }

    /// Given a result with a note whose text is within the byte budget
    /// When truncating for the model
    /// Then the result passes through with its note intact — the rewrite only
    /// applies to results whose output is actually replaced.
    #[test]
    fn should_keep_the_note_when_the_text_is_under_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let truncator = ToolResultTruncator::new(dir.path());
        let original_note = "Read lines 1-3 of 3.";
        let result = ToolResult {
            output: ToolOutput::Text("small output".to_string()),
            is_error: false,
            note: Some(original_note.to_string()),
            stop_turn: false,
            truncated: false,
            injection: None,
        };

        let passthrough = truncator.truncate_for_model("Read", "call_small_note", result);

        assert_eq!(passthrough.note.as_deref(), Some(original_note));
        assert!(!passthrough.truncated);
        match &passthrough.output {
            ToolOutput::Text(t) => assert_eq!(t, "small output"),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }
}
