//! `GrepTool` — content search across files.
//!
//! Uses the [`ContentSearch`] abstraction (a ripgrep subprocess) to search
//! file contents by regex. Supports a glob filter, three output modes
//! (`content`, `files_with_matches`, `count_matches`), and returns a
//! structured JSON payload with match line numbers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::{DynContentSearch, DynFileSystem, SearchParams};

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::{PathClassification, WorkspacePaths, tool_execution_error};

/// Maximum number of matches returned per search.
const DEFAULT_HEAD_LIMIT: usize = 250;

/// Fallback search root used when neither the model provides a `path`
/// nor the session injects a default root.
fn default_search_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Present a match path relative to the search root.
///
/// Keeps model-facing results compact and portable; falls back to the raw
/// path when it lies outside the root (e.g. files pulled in by a glob that
/// escapes the root).
fn display_path(path: &str, root: &Path) -> String {
    let root_prefix = format!("{}/", root.to_string_lossy());
    path.strip_prefix(root_prefix.as_str())
        .map(str::to_owned)
        .or_else(|| path.strip_prefix("./").map(str::to_owned))
        .unwrap_or_else(|| path.to_string())
}

/// `Grep` — search file contents by regex.
pub struct GrepTool {
    /// The content-search provider every search goes through (zero-IO T13a).
    search: Arc<dyn DynContentSearch>,
    /// The session's workspace sandbox this tool may search inside; `None`
    /// disables path validation (legacy hosts / unit tests, ADR-2).
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider used by workspace path validation (zero-IO
    /// T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl GrepTool {
    /// Create the tool with the injected search provider (the engine default
    /// is the local ripgrep one) and the session's workspace sandbox. The
    /// sandbox root doubles as the default search root when the model omits
    /// `path` (TS parity: the TS tools anchor at the session cwd); `None`
    /// keeps the unguarded behavior (search root falls back to the process
    /// cwd).
    pub fn new(
        search: Arc<dyn DynContentSearch>,
        workspace_paths: Option<Arc<WorkspacePaths>>,
        fs: Arc<dyn DynFileSystem>,
    ) -> Self {
        Self {
            search,
            workspace_paths,
            fs,
        }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex pattern. 'output_mode' is one of \
         'content' (matching lines with line numbers), \
         'files_with_matches' (default; matching file paths), or \
         'count_matches' (per-file match counts). 'glob' filters which files \
         are searched (e.g. '*.rs'); a pattern containing a '/' (e.g. \
         'src/**/*.rs') is matched against each file's path relative to the \
         search root. 'type' filters by ripgrep file type \
         (e.g. 'ts', 'rust'). '-i' enables case-insensitive search. '-n' \
         controls line numbers in content mode (default true). '-A', '-B', \
         '-C' set after/before/combined context lines. 'head_limit' caps the \
         number of results (default 250, 0 for unlimited). 'offset' skips \
         leading results for pagination. 'multiline' enables multi-line \
         pattern matching. 'include_ignored' searches gitignored files. \
         'path' defaults to the working directory."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in. Defaults to the working directory. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob filter limiting which files are searched, e.g. '*.rs'. A pattern without a '/' matches the file name at any depth; a pattern containing a '/' (e.g. 'src/**/*.rs') is matched against each file's path relative to the search root."
                    },
                    "type": {
                        "type": "string",
                        "description": "Optional ripgrep file type filter, such as 'ts' or 'rust'. Prefer this over 'glob' when filtering by language."
                    },
                    "output_mode": {
                        "type": "string",
                        "enum": ["content", "files_with_matches", "count_matches"],
                        "description": "Result shape. Defaults to 'files_with_matches'."
                    },
                    "-i": {
                        "type": "boolean",
                        "description": "Perform a case-insensitive search. Defaults to false."
                    },
                    "-n": {
                        "type": "boolean",
                        "description": "Prefix each matching line with its line number. Applies only when output_mode is 'content'. Defaults to true."
                    },
                    "-A": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of lines to show after each match. Applies only when output_mode is 'content'."
                    },
                    "-B": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of lines to show before each match. Applies only when output_mode is 'content'."
                    },
                    "-C": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Number of lines to show before and after each match. Takes precedence over -A and -B. Applies only when output_mode is 'content'."
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 10000,
                        "description": "Limit output to the first N entries after offset. Defaults to 250. Pass 0 for unlimited."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100000,
                        "description": "Number of leading entries to skip before applying head_limit. Defaults to 0."
                    },
                    "multiline": {
                        "type": "boolean",
                        "description": "Enable multiline matching, where the pattern can span line boundaries. Defaults to false."
                    },
                    "include_ignored": {
                        "type": "boolean",
                        "description": "Also search files excluded by ignore files such as .gitignore. Sensitive files remain filtered out. Defaults to false."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            })
        })
    }

    fn capabilities(&self) -> &[String] {
        static CAPABILITIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(|| vec!["code.explore".to_string()])
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(pattern) = input.get("pattern").and_then(serde_json::Value::as_str) else {
            return Ok(tool_execution_error(
                "Grep requires a string 'pattern' argument",
            ));
        };
        if pattern.is_empty() {
            return Ok(tool_execution_error(
                "Grep requires a non-empty 'pattern' argument",
            ));
        }
        let pattern = pattern.to_string();
        let path_arg = input
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        // Classify the explicit `path` against the workspace sandbox before
        // any I/O (ADR-2, audit P0-2; plan 2026-10-10-sandbox-path-permission
        // D1). Inside and Outside both build the plan over the canonical
        // path (Outside enters the permission chain instead of being refused
        // here); only escape shapes are refused. The canonical path is used
        // for the plan and the search, so the permission policies and the
        // file access agree on the target. When the model omits `path`, the
        // search anchors at the workspace root (TS parity: the TS tools
        // anchor at the session cwd).
        let path = match path_arg {
            Some(path) => match crate::tools::classify_tool_path(
                self.workspace_paths.as_deref(),
                &self.fs,
                &path,
            ) {
                PathClassification::Inside(canonical) | PathClassification::Outside(canonical) => {
                    Some(canonical)
                }
                PathClassification::Escape { reason } => {
                    return Ok(tool_execution_error(crate::tools::escape_tool_error(
                        "Grep", &path, &reason,
                    )));
                }
            },
            None => self
                .workspace_paths
                .as_ref()
                .map(|workspace| workspace.root().to_path_buf()),
        };
        let glob = input
            .get("glob")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let file_type = input
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let output_mode = input
            .get("output_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("files_with_matches")
            .to_string();
        let case_insensitive = input
            .get("-i")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let line_numbers = input
            .get("-n")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let context_after = input
            .get("-A")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(0);
        let context_before = input
            .get("-B")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(0);
        let context = input
            .get("-C")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(0);
        // head_limit: 0 means unlimited (usize::MAX). Default 250.
        let head_limit = input
            .get("head_limit")
            .and_then(serde_json::Value::as_u64)
            .map(|v| if v == 0 { usize::MAX } else { v as usize })
            .unwrap_or(DEFAULT_HEAD_LIMIT);
        let offset = input
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(0);
        let multiline = input
            .get("multiline")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let include_ignored = input
            .get("include_ignored")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                kind: AccessKind::Search,
                path: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                operation: AccessOperation::Read,
            }],
            display: ToolInputDisplay::Search {
                query: pattern.clone(),
                scope: path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            },
            approval_rule: "Grep".to_string(),
            stop_batch_after_this: false,
        };

        let search = Arc::clone(&self.search);
        let fs = Arc::clone(&self.fs);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                let params = SearchParams {
                    pattern,
                    path: path.unwrap_or_else(default_search_root),
                    glob,
                    file_type,
                    case_insensitive,
                    line_numbers,
                    multiline,
                    output_mode,
                    context_before,
                    context_after,
                    context,
                    head_limit,
                    offset,
                    include_ignored,
                };
                Box::pin(search_contents(search, Some(fs), params))
            }),
        })
    }
}

async fn search_contents(
    search: Arc<dyn DynContentSearch>,
    fs: Option<Arc<dyn DynFileSystem>>,
    params: SearchParams,
) -> ToolResult {
    let search_path = params.path.clone();
    let output_mode = params.output_mode.clone();
    let head_limit = params.head_limit;
    match search.search(&params).await {
        Ok(result) => {
            let truncated = result.truncated;

            // Sensitive files (secrets, keys, credentials) never surface in
            // Grep results. mirri-os already filters at parse time; this is a
            // second layer guarding the rendered output.
            let filtered: Vec<&mirri_os::SearchMatch> = result
                .matches
                .iter()
                .filter(|m| !crate::tools::path_security::is_sensitive_file(Path::new(&m.path)))
                .collect();

            let output = if output_mode == "content" {
                // Content mode: `${displayPath}:${lineNo}:${payload}` text
                // lines; `-n:false` drops the line number (spec).
                let lines: Vec<String> = filtered
                    .iter()
                    .map(|m| {
                        let line_prefix = if params.line_numbers {
                            format!("{}:", m.line_number)
                        } else {
                            String::new()
                        };
                        format!(
                            "{}:{}{}",
                            display_path(&m.path, &search_path),
                            line_prefix,
                            m.line
                        )
                    })
                    .collect();
                lines.join("\n")
            } else if output_mode == "files_with_matches" {
                // Files mode: plain text, one path per line, most-recently
                // modified first (mtime descending — spec). Sorting runs on
                // the absolute match paths so stat hits the right file; the
                // displayed form is relativized after ordering. stat failures
                // keep the path but sort it last (mtime = 0).
                let deduped_abs: Vec<&str> = {
                    let mut seen = std::collections::HashSet::new();
                    filtered
                        .iter()
                        .map(|m| m.path.as_str())
                        .filter(|p| seen.insert((*p).to_string()))
                        .collect()
                };
                let paths: Vec<String> = if let Some(fs) = fs {
                    sort_by_mtime(fs, deduped_abs).await
                } else {
                    deduped_abs.into_iter().map(str::to_owned).collect()
                };
                let paths: Vec<String> = paths
                    .iter()
                    .map(|p| display_path(p, &search_path))
                    .collect();
                if paths.is_empty() {
                    "No matches found".to_string()
                } else {
                    paths.join("\n")
                }
            } else {
                // Count mode: per-file counts (first-appearance order)
                // preceded by the spec's aggregate header. The provider
                // carries each file's occurrence count (`rg --count-matches`)
                // in `line_number`.
                let mut counts: Vec<(String, u64)> = Vec::new();
                for m in &filtered {
                    let path = display_path(&m.path, &search_path);
                    if let Some((_, count)) = counts.iter_mut().find(|(p, _)| *p == path) {
                        *count += m.line_number;
                    } else {
                        counts.push((path, m.line_number));
                    }
                }
                if counts.is_empty() {
                    "No matches found".to_string()
                } else {
                    let total: u64 = counts.iter().map(|(_, count)| count).sum();
                    let files = counts.len();
                    let occurrence = if total == 1 {
                        "occurrence"
                    } else {
                        "occurrences"
                    };
                    let file_word = if files == 1 { "file" } else { "files" };
                    let mut lines = vec![format!(
                        "Found {total} {occurrence} across {files} {file_word}."
                    )];
                    lines.extend(counts.iter().map(|(path, count)| format!("{path}:{count}")));
                    lines.join("\n")
                }
            };

            let note = truncated.then(|| {
                if head_limit == usize::MAX {
                    "Grep results truncated (output size limit reached)".to_string()
                } else {
                    format!("Grep results truncated at {head_limit} matches")
                }
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
        Err(error) => ToolResult::error(format!("Grep failed: {error}")),
    }
}

/// Sort file paths by last-modification time, most recent first (spec).
///
/// Consults the filesystem via `fs::stat`; a path whose stat fails keeps the
/// path but sorts last (mtime = 0), matching the TypeScript fallback. Relative
/// paths are resolved against `current_dir` so the stat targets the right
/// file. A stable sort keeps the earlier-displayed order among equal mtimes.
async fn sort_by_mtime(fs: Arc<dyn DynFileSystem>, paths: Vec<&str>) -> Vec<String> {
    let mut with_mtime: Vec<(String, u64)> = Vec::with_capacity(paths.len());
    for path in paths {
        // Paths are absolute match paths from the search provider.
        let mtime = fs
            .stat(Path::new(path))
            .await
            .map(|stat| stat.mtime)
            .unwrap_or(0);
        with_mtime.push((path.to_string(), mtime));
    }
    with_mtime.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    with_mtime.into_iter().map(|(path, _)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::tool::ExecutableToolContext;

    /// The engine-default local ripgrep provider, as the registered tool
    /// receives it.
    fn local_search() -> Arc<dyn DynContentSearch> {
        Arc::new(mirri_os::RipgrepSearch::new())
    }

    /// The engine-default local filesystem provider, as the registered tool
    /// receives it (used by workspace path validation).
    fn local_fs() -> Arc<dyn DynFileSystem> {
        Arc::new(mirri_os::LocalFs::new())
    }

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    /// Build `SearchParams` with GrepTool defaults (head_limit = 250, etc.)
    /// and the given pattern / path / output_mode.
    fn grep_params(pattern: &str, path: PathBuf, output_mode: &str) -> SearchParams {
        SearchParams {
            pattern: pattern.to_string(),
            path,
            output_mode: output_mode.to_string(),
            head_limit: DEFAULT_HEAD_LIMIT,
            ..Default::default()
        }
    }

    /// Set a file's modification time to a fixed epoch seconds value.
    fn set_mtime(path: &Path, epoch_secs: i64) {
        use std::fs::FileTimes;
        let file = std::fs::File::open(path).unwrap();
        let times = FileTimes::new().set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64),
        );
        file.set_times(times).expect("set mtime");
    }

    // ------------------------------------------------------------------ //
    //  Existing tests — updated for the new `SearchParams` signature     //
    // ------------------------------------------------------------------ //

    #[tokio::test]
    async fn should_return_match_with_line_number_when_searching_in_content_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "no match here\n").unwrap();

        let result = search_contents(
            local_search(),
            None,
            grep_params("beta", tmp.path().to_path_buf(), "content"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text, "a.txt:2:beta");
    }

    #[tokio::test]
    async fn should_return_only_paths_when_searching_in_files_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn helper() {}\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "fn main() {}\n").unwrap();

        let result = search_contents(
            local_search(),
            None,
            grep_params("fn main", tmp.path().to_path_buf(), "files_with_matches"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let mut paths: Vec<&str> = text.lines().collect();
        // Plain-text files mode already sorts paths, but sort for safety.
        paths.sort();
        assert_eq!(paths, vec!["a.rs", "c.txt"]);
    }

    #[tokio::test]
    async fn should_order_files_most_recently_modified_first_when_searching_files_mode() {
        // Given two matching files with different modification times,
        // When the filesystem is consulted (Some(fs)) and grep runs in
        // files_with_matches mode,
        // Then the most recently modified file appears first (mtime desc).
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old.rs");
        let fresh = tmp.path().join("fresh.rs");
        std::fs::write(&old, "fn main() {}\n").unwrap();
        std::fs::write(&fresh, "fn main() {}\n").unwrap();

        set_mtime(&old, 1_000_000);
        set_mtime(&fresh, 2_000_000);

        let result = search_contents(
            local_search(),
            Some(local_fs()),
            grep_params("fn main", tmp.path().to_path_buf(), "files_with_matches"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(
            paths,
            vec!["fresh.rs", "old.rs"],
            "files must be most-recently-modified first: {text}"
        );
    }

    #[tokio::test]
    async fn should_return_per_file_counts_when_searching_in_count_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x x x\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x\n").unwrap();

        let result = search_contents(
            local_search(),
            None,
            grep_params("x", tmp.path().to_path_buf(), "count_matches"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // The spec header comes first, then one `path:count` line per file.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Found 4 occurrences across 2 files.");
        assert!(lines.contains(&"a.txt:3"), "{lines:?}");
        assert!(lines.contains(&"b.txt:1"), "{lines:?}");
    }

    #[tokio::test]
    async fn should_search_only_matching_files_when_a_glob_filter_is_provided() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "fn main() {}\n").unwrap();

        let mut params = grep_params("fn main", tmp.path().to_path_buf(), "files_with_matches");
        params.glob = Some("*.rs".to_string());
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert_eq!(paths[0], "a.rs");
    }

    #[tokio::test]
    async fn should_default_to_files_with_matches_when_output_mode_is_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "gamma\n").unwrap();

        let input = serde_json::json!({
            "pattern": "beta",
            "path": tmp.path().to_string_lossy()
        });
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(input)
            .unwrap();
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected Runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // files_with_matches mode: one path per line.
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(paths, vec!["a.txt"]);
    }

    #[tokio::test]
    async fn should_return_empty_result_without_error_when_searching_with_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing\n").unwrap();
        let result = search_contents(
            local_search(),
            None,
            grep_params("zzz_nope", tmp.path().to_path_buf(), "content"),
        )
        .await;
        assert!(!result.is_error, "zero matches is not an error");
        // No matches renders zero content-mode lines.
        assert_eq!(output_text(&result), "");
    }

    // ------------------------------------------------------------------ //
    //  New tests — the 10 additional parameters                          //
    // ------------------------------------------------------------------ //

    #[tokio::test]
    async fn should_filter_by_type_when_type_parameter_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn target() {}\n").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "fn target() {}\n").unwrap();

        let mut params = grep_params("target", tmp.path().to_path_buf(), "files_with_matches");
        params.file_type = Some("rust".to_string());
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(paths, vec!["a.rs"]);
    }

    #[tokio::test]
    async fn should_match_different_case_when_case_insensitive_is_true() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "Hello World\n").unwrap();

        let mut params = grep_params("hello", tmp.path().to_path_buf(), "content");
        params.case_insensitive = true;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "a.txt:1:Hello World");
    }

    #[tokio::test]
    async fn should_omit_line_numbers_when_line_numbers_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();

        let mut params = grep_params("beta", tmp.path().to_path_buf(), "content");
        params.line_numbers = false;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "a.txt:beta");
    }

    #[tokio::test]
    async fn should_include_line_numbers_by_default_in_content_mode() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();

        let result = search_contents(
            local_search(),
            None,
            grep_params("beta", tmp.path().to_path_buf(), "content"),
        )
        .await;
        assert!(!result.is_error);
        // line_number should be 2 (the second line) by default
        assert_eq!(output_text(&result), "a.txt:2:beta");
    }

    #[tokio::test]
    async fn should_show_after_context_lines_when_context_after_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "line1\nmatch\nline3\nline4\n").unwrap();

        let mut params = grep_params("match", tmp.path().to_path_buf(), "content");
        params.context_after = 1;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // With -A 1, the match line plus one context line render as
        // path:line:payload text rows.
        let text = output_text(&result);
        assert!(
            text.contains("a.txt:2:match"),
            "expected match row in {text}"
        );
        assert!(
            text.contains("a.txt:3:line3"),
            "expected after-context row in {text}"
        );
    }

    #[tokio::test]
    async fn should_show_before_context_lines_when_context_before_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "line1\nline2\nmatch\nline4\n").unwrap();

        let mut params = grep_params("match", tmp.path().to_path_buf(), "content");
        params.context_before = 1;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("a.txt:3:match"),
            "expected match row in {text}"
        );
        assert!(
            text.contains("a.txt:2:line2"),
            "expected before-context row in {text}"
        );
    }

    #[tokio::test]
    async fn should_show_combined_context_when_context_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.txt"),
            "line1\nline2\nmatch\nline4\nline5\n",
        )
        .unwrap();

        let mut params = grep_params("match", tmp.path().to_path_buf(), "content");
        params.context = 1;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // -C 1 gives one line before and one after
        assert!(
            text.contains("a.txt:2:line2"),
            "expected before-context in {text}"
        );
        assert!(
            text.contains("a.txt:3:match"),
            "expected match row in {text}"
        );
        assert!(
            text.contains("a.txt:4:line4"),
            "expected after-context in {text}"
        );
    }

    #[tokio::test]
    async fn should_truncate_results_when_head_limit_is_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "target\n").unwrap();
        }

        let mut params = grep_params("target", tmp.path().to_path_buf(), "content");
        params.head_limit = 2;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text.lines().count(), 2);
        assert!(
            result.truncated,
            "head_limit must mark the result truncated"
        );
        assert!(result.note.is_some(), "truncation note expected");
    }

    #[tokio::test]
    async fn should_skip_first_n_matches_when_offset_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        // 5 matches across 5 files — one match per file
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "target\n").unwrap();
        }

        let mut params = grep_params("target", tmp.path().to_path_buf(), "content");
        params.offset = 2;
        params.head_limit = 10; // high enough to not truncate
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // 5 total, skip 2 → 3 rows returned, not truncated.
        assert_eq!(output_text(&result).lines().count(), 3);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn should_apply_offset_and_head_limit_together() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "target\n").unwrap();
        }

        let mut params = grep_params("target", tmp.path().to_path_buf(), "content");
        params.offset = 3;
        params.head_limit = 4;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // 10 total, skip 3, take 4 → 4 rows returned, truncated
        assert_eq!(output_text(&result).lines().count(), 4);
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn should_match_across_line_boundaries_when_multiline_is_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "start\nend\n").unwrap();

        // Without multiline, "start\\nend" won't match because rg treats each
        // line separately. With multiline + dotall, it should match.
        let mut params = grep_params("start[\\s\\S]*end", tmp.path().to_path_buf(), "content");
        params.multiline = true;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // The single multiline match renders as one `path:line:payload`
        // row; the payload itself spans the two file lines.
        let text = output_text(&result);
        assert!(text.starts_with("a.txt:1:"), "row missing: {text}");
        assert!(text.contains("start") && text.contains("end"), "{text}");
    }

    #[tokio::test]
    async fn should_search_gitignored_files_when_include_ignored_is_true() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "secret_term\n").unwrap();
        std::fs::write(tmp.path().join("normal.txt"), "secret_term\n").unwrap();
        // Create a .gitignore that excludes ignored.txt
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        // Initialize a git repo so ripgrep respects .gitignore
        std::process::Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output()
            .ok();

        // Without include_ignored: only normal.txt should match
        let params_no_ignore = grep_params(
            "secret_term",
            tmp.path().to_path_buf(),
            "files_with_matches",
        );
        let result = search_contents(local_search(), None, params_no_ignore).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert!(
            !paths.iter().any(|p| p.ends_with("ignored.txt")),
            "ignored.txt should be excluded without include_ignored, got {paths:?}"
        );

        // With include_ignored: both should match
        let mut params_ignore = grep_params(
            "secret_term",
            tmp.path().to_path_buf(),
            "files_with_matches",
        );
        params_ignore.include_ignored = true;
        let result = search_contents(local_search(), None, params_ignore).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert!(
            paths.iter().any(|p| p.ends_with("ignored.txt")),
            "ignored.txt should be included with include_ignored, got {paths:?}"
        );
    }

    #[tokio::test]
    async fn should_filter_sensitive_files_from_grep_results() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("normal.txt"), "secret_term\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "secret_term\n").unwrap();
        std::fs::write(tmp.path().join("key.pem"), "secret_term\n").unwrap();
        // `.env.example` is a known non-sensitive variant and must survive.
        std::fs::write(tmp.path().join(".env.example"), "secret_term\n").unwrap();

        // include_ignored forces --hidden so the dotfiles are actually searched
        // and the sensitive-filter layers have something to remove.
        let mut params = grep_params(
            "secret_term",
            tmp.path().to_path_buf(),
            "files_with_matches",
        );
        params.include_ignored = true;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert!(
            paths.contains(&"normal.txt"),
            "normal.txt should match: {paths:?}"
        );
        assert!(
            paths.contains(&".env.example"),
            ".env.example is not sensitive and should match: {paths:?}"
        );
        assert!(
            !paths.contains(&".env"),
            ".env must be filtered from results: {paths:?}"
        );
        assert!(
            !paths.contains(&"key.pem"),
            "key material must be filtered from results: {paths:?}"
        );
    }

    #[tokio::test]
    async fn should_parse_all_new_parameters_from_input_in_resolve_execution() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "Hello World\n").unwrap();

        let input = serde_json::json!({
            "pattern": "hello",
            "path": tmp.path().to_string_lossy(),
            "output_mode": "content",
            "-i": true,
            "-n": false
        });
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(input)
            .unwrap();
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected Runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // case-insensitive should find "Hello" with pattern "hello";
        // -n false drops the line number.
        assert_eq!(text, "a.txt:Hello World");
    }

    #[tokio::test]
    async fn should_parse_head_limit_zero_as_unlimited_in_resolve_execution() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "target\n").unwrap();
        }

        let input = serde_json::json!({
            "pattern": "target",
            "path": tmp.path().to_string_lossy(),
            "output_mode": "content",
            "head_limit": 0
        });
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(input)
            .unwrap();
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected Runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // head_limit 0 → unlimited → all 3 matches returned, not truncated
        assert_eq!(text.lines().count(), 3);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn should_parse_type_and_context_params_from_input_in_resolve_execution() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "line1\nmatch\nline3\n").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "match\n").unwrap();

        let input = serde_json::json!({
            "pattern": "match",
            "path": tmp.path().to_string_lossy(),
            "output_mode": "content",
            "type": "rust",
            "-A": 1
        });
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(input)
            .unwrap();
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected Runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // type=rust should filter out a.txt
        assert!(
            text.lines().all(|l| l.starts_with("a.rs:")),
            "only .rs paths expected: {text}"
        );
        // -A 1 should include the context row "line3"
        assert!(
            text.contains("a.rs:3:line3"),
            "expected after-context row in {text}"
        );
    }

    #[test]
    fn should_cap_head_limit_and_offset_in_schema() {
        // Given the Grep tool's parameter schema
        let tool = GrepTool::new(local_search(), None, local_fs());
        let schema = tool.parameters();

        // Then head_limit and offset carry explicit upper bounds so callers
        // cannot request unbounded pagination.
        assert_eq!(
            schema["properties"]["head_limit"]["maximum"], 10000,
            "head_limit should be capped at 10000 in the schema"
        );
        assert_eq!(
            schema["properties"]["offset"]["maximum"], 100000,
            "offset should be capped at 100000 in the schema"
        );
    }

    /// Given the Grep tool's parameter schema (Task 5, plan 2026-10-10-
    /// sandbox-path-permission D5), When the `path` description is
    /// inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = GrepTool::new(local_search(), None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }
    // -------------------------------------------------------------------------
    // T-13a-4: injected search provider (zero-IO injection)
    // -------------------------------------------------------------------------

    /// Given the Grep tool with the session's workdir injected as its
    /// default root, When no `path` is given, Then the search hits the
    /// session workdir — not the process cwd (audit P0-1).
    #[tokio::test]
    async fn should_search_the_session_workdir_when_no_path_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("session-workdir-needle.txt"),
            "needle-rust-v3\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "pattern": "needle-rust-v3",
            "output_mode": "files_with_matches"
        });
        let execution = GrepTool::new(
            local_search(),
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        )
        .resolve_execution(input)
        .unwrap();
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected Runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(
            paths,
            vec!["session-workdir-needle.txt"],
            "expected the session workdir to be searched, got: {text}"
        );
    }

    /// A [`mirri_os::DynContentSearch`] double returning a fixed match
    /// (T-13a-4) — no ripgrep subprocess is spawned.
    struct FixedSearch(mirri_os::SearchMatch);

    impl mirri_os::DynContentSearch for FixedSearch {
        fn search<'a>(&'a self, _params: &'a SearchParams) -> mirri_os::DynSearchFuture<'a> {
            Box::pin(async move {
                Ok(mirri_os::SearchResult {
                    matches: vec![mirri_os::SearchMatch {
                        path: self.0.path.clone(),
                        line_number: self.0.line_number,
                        line: self.0.line.clone(),
                        column: self.0.column,
                    }],
                    total_matches: 1,
                    truncated: false,
                })
            })
        }
    }

    /// Given the Grep tool with an injected fake provider, When a content
    /// search executes, Then the fake's fixed match is returned — the
    /// search went through the injected provider, not the local ripgrep
    /// one (T-13a-4).
    #[tokio::test]
    async fn should_return_the_injected_providers_matches_when_grepping() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.txt"), "real content\n").unwrap();

        let fake = FixedSearch(mirri_os::SearchMatch {
            path: tmp.path().join("fake.rs").to_string_lossy().into_owned(),
            line_number: 7,
            line: "fake needle".to_string(),
            column: 3,
        });
        let tool = GrepTool::new(Arc::new(fake), None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "output_mode": "content"
            }))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let result = execute(crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        })
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("fake.rs") && text.contains("fake needle"),
            "the fake provider's match must surface: {text}"
        );
        assert!(
            !text.contains("real.txt"),
            "the local provider must not have run: {text}"
        );
    }

    /// Given a workspace root and a clean absolute path outside it, When
    /// Grep is called with that path, Then the plan is built over the
    /// canonical outside path and the search proceeds — the tool no longer
    /// refuses outside paths, the permission chain decides (plan
    /// 2026-10-10-sandbox-path-permission, D1).
    #[tokio::test]
    async fn should_search_an_outside_directory_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "needle\n").unwrap();

        let tool = GrepTool::new(
            local_search(),
            Some(Arc::new(WorkspacePaths::new(root))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "path": outside.to_string_lossy(),
            }))
            .expect("resolve");

        let ToolExecution::Runnable { plan, execute } = execution else {
            panic!("expected a runnable execution for an outside absolute path");
        };
        let access = plan.accesses.first().expect("one search access");
        assert_eq!(
            access.path.as_deref(),
            Some(outside.canonicalize().unwrap().to_str().unwrap()),
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
        assert!(!result.is_error, "grep failed: {}", output_text(&result));
        assert!(
            output_text(&result).contains("secret.txt"),
            "the outside directory must be searchable: {}",
            output_text(&result)
        );
    }

    /// Given a workspace root, When Grep is called with a relative path that
    /// lexically escapes the root (`..`), Then it is refused with the escape
    /// wording — a relative path can never declare outside intent.
    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_grepping_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();

        let tool = GrepTool::new(
            local_search(),
            Some(Arc::new(WorkspacePaths::new(root))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "path": "../escape",
            }))
            .expect("resolve");

        let ToolExecution::Error(error) = execution else {
            panic!("expected an escape refusal, got a runnable execution");
        };
        assert!(
            error.message.contains("escapes the workspace sandbox"),
            "refusal uses the new escape wording: {}",
            error.message
        );
    }

    /// Given an additional dir registered on the sandbox, When Grep targets
    /// it, Then the search proceeds normally — registered dirs are part of
    /// the allowed search surface (ADR-2).
    #[tokio::test]
    async fn should_allow_grep_inside_registered_additional_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let additional = tmp.path().join("additional");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&additional).unwrap();
        std::fs::write(additional.join("extra.txt"), "needle\n").unwrap();
        let workspace = WorkspacePaths::new(root);
        workspace.add_additional(additional.clone());

        let tool = GrepTool::new(local_search(), Some(Arc::new(workspace)), local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "path": additional.to_string_lossy(),
            }))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        let ctx = ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "test".to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result = execute(ctx).await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(
            paths,
            vec!["extra.txt"],
            "the registered additional dir must be searchable: {text}"
        );
    }

    // ------------------------------------------------------------------ //
    //  Output contract (A3 1.5 T8b)                                      //
    // ------------------------------------------------------------------ //

    #[tokio::test]
    async fn should_render_content_mode_as_path_line_text_when_searching() {
        // Given a file with a match, When content-mode grep runs,
        // Then the output is `<path>:<lineNo>:<payload>` text lines (not JSON).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let result = search_contents(
            local_search(),
            None,
            grep_params("beta", tmp.path().to_path_buf(), "content"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(
            text, "a.txt:2:beta",
            "content mode must be path:line:payload text, got: {text}"
        );
        assert!(!text.trim_start().starts_with('{'), "must not be JSON");
    }

    #[tokio::test]
    async fn should_omit_line_number_in_content_mode_when_line_numbers_is_false() {
        // Given content mode with -n:false, When grep runs,
        // Then the payload line carries no line number.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "beta\n").unwrap();
        let mut params = grep_params("beta", tmp.path().to_path_buf(), "content");
        params.line_numbers = false;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "a.txt:beta");
    }

    #[tokio::test]
    async fn should_render_count_mode_as_per_file_counts_with_total_header_when_searching() {
        // Given two files with 3 and 1 matches, When count-mode grep runs,
        // Then the output has a `Found N occurrences across M files.` header
        // and per-file `path:count` lines.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x x x\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x\n").unwrap();
        let result = search_contents(
            local_search(),
            None,
            grep_params("x", tmp.path().to_path_buf(), "count_matches"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Found 4 occurrences across 2 files.")),
            "header missing: {text}"
        );
        assert!(lines.contains(&"a.txt:3"), "per-file count missing: {text}");
        assert!(lines.contains(&"b.txt:1"), "per-file count missing: {text}");
    }

    #[tokio::test]
    async fn should_use_singular_header_words_when_count_mode_finds_one_match_in_one_file() {
        // Given one file with a single match, When count-mode grep runs,
        // Then the header uses the singular forms ('1 occurrence across 1 file').
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x\n").unwrap();
        let result = search_contents(
            local_search(),
            None,
            grep_params("x", tmp.path().to_path_buf(), "count_matches"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.starts_with("Found 1 occurrence across 1 file."),
            "singular header missing: {text}"
        );
    }
}
