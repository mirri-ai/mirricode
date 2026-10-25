//! `GrepTool` — content search across files.
//!
//! Uses the [`ContentSearch`] abstraction (a ripgrep subprocess) to search
//! file contents by regex. Supports a glob filter, three output modes
//! (`content`, `files`, `count`), and returns a
//! structured JSON payload with match line numbers.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::{DynContentSearch, DynFileSystem, SearchParams};

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::{PathClassification, WorkspacePaths, tool_execution_error};

/// Maximum number of paths accepted in a single `path` array.
const MAX_PATHS: usize = 32;

/// Maximum number of matches returned per search (non-content modes).
const DEFAULT_HEAD_LIMIT: usize = 250;

/// Maximum number of matches returned per content-mode search (G-ADR-4:
/// tightened 250 → 30 so an unprompted call stays within one screenful of
/// blocks; page with `offset` when more is needed).
const DEFAULT_HEAD_LIMIT_CONTENT: usize = 30;

/// Soft byte budget for content-mode rendering (G-ADR-4). Truncation happens
/// only on a block boundary — a single oversized block still renders whole.
const BYTE_BUDGET: usize = 4096;

/// Fallback search root used when neither the model provides a `path`
/// nor the session injects a default root.
fn default_search_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Present a match path relative to the search root it belongs to.
///
/// With multiple search roots the match path is relativized against its
/// owning root (longest leading-component prefix, matching the search
/// layer's group attribution); falls back to the raw path when it lies
/// outside every root (e.g. files pulled in by a glob that escapes the
/// roots).
fn display_path(path: &str, roots: &[PathBuf]) -> String {
    let absolute = Path::new(path);
    let owning = roots.iter().filter(|root| {
        let mut root_components = root.components().peekable();
        if root_components.peek().is_none() {
            return false;
        }
        let mut path_components = absolute.components();
        for root_component in root_components.by_ref() {
            match path_components.next() {
                Some(path_component) if root_component == path_component => {}
                _ => return false,
            }
        }
        true
    });
    let Some(root) = owning.max_by_key(|root| root.components().count()) else {
        // `./`-prefixed leftovers (legacy rg cwd-relative records) surface
        // compact; anything else stays verbatim.
        return path
            .strip_prefix("./")
            .map(str::to_owned)
            .unwrap_or_else(|| path.to_string());
    };
    match absolute.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
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
        "Search file contents using regular expressions.\n\nUse Grep when \
         the task is to find unknown content or unknown file locations. Do \
         not use shell `grep` or `rg` directly; this tool applies workspace \
         path policy, output limits, and sensitive-file filtering.\n\nOne \
         call answers the whole question — combine these instead of a shell \
         pipeline:\n- Reading around a match: `output_mode: \"content\"` \
         with `-C` returns the match with line-numbered context lines, \
         grouped into blocks. This replaces `rg -n -C 3 <pattern> | head`.\n\
         - Narrowing to specific files: `path` accepts an array of file \
         paths (`[\"a.rs\",\"b.rs\"]`) or one directory.\n- Excluding noise: \
         `exclude_pattern` drops matching lines (e.g. tests, generated code) \
         — replaces `| rg -v`.\n- Keeping output small: `head_limit` (default \
         30 in content mode, plus a byte budget) caps results; the result \
         tells you the total so you can page with `offset`.\n\nWrite patterns \
         in ripgrep regex syntax (braces are special: escape `\\{` for a \
         literal `{`).\n\nHidden files are searched by default; \
         `include_ignored: true` also searches gitignored files. Sensitive \
         files (`.env` etc.) are always skipped."
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
                        "anyOf": [
                            {
                                "type": "string",
                                "description": "Directory or file to search in. Defaults to the working directory. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                            },
                            {
                                "type": "array",
                                "items": { "type": "string" },
                                "maxItems": 32,
                                "description": "Up to 32 directories or files to search in a single invocation, searched together in one pass. Each path is validated against the workspace sandbox (outside paths require user authorization); every path must exist. When you need to filter many candidates, use the Glob tool to narrow the list first."
                            }
                        ]
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
                        "enum": ["content", "files", "count"],
                        "description": "Shape of the result. Pass exactly ONE of these string VALUES as output_mode — these are not standalone arguments. 'content' shows matching lines (honors -A/-B/-C/-n/head_limit); 'files' shows only paths of files with a match, most-recently-modified first (honors head_limit); 'count' shows per-file match counts as 'path:count' lines preceded by an aggregate total. Example: {\"output_mode\":\"count\"}. Defaults to 'files'."
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
                    "exclude_pattern": {
                        "type": "string",
                        "description": "Regular expression (ripgrep syntax) excluding matching lines from results. A match whose line text matches this pattern is dropped, along with its context lines; files whose matches are all excluded are dropped from files/count results."
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 10000,
                        "default": 30,
                        "description": "Limit output to the first N entries after offset. Defaults to 30 in content mode, 250 otherwise. Pass 0 for unlimited."
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
        let output_mode = input
            .get("output_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("files")
            .to_string();
        // `path` accepts a single string or an array of up to 32 strings
        // (schema `anyOf`); the explicit length re-check mirrors the
        // jsonschema `maxItems` because resolve_execution never sees the
        // validation layer.
        let path_args: Vec<String> = match input.get("path") {
            None => Vec::new(),
            Some(serde_json::Value::String(path)) => vec![path.clone()],
            Some(serde_json::Value::Array(items)) => {
                let mut paths = Vec::with_capacity(items.len());
                for item in items {
                    let Some(path) = item.as_str() else {
                        return Ok(tool_execution_error(
                            "Grep 'path' array entries must be strings",
                        ));
                    };
                    paths.push(path.to_string());
                }
                paths
            }
            Some(_) => {
                return Ok(tool_execution_error(
                    "Grep 'path' must be a string or an array of strings",
                ));
            }
        };
        if path_args.len() > MAX_PATHS {
            return Ok(tool_execution_error(format!(
                "Grep accepts at most {MAX_PATHS} paths per call (got {}). Use the glob \
                 tool to narrow the candidate list first, then pass the selected paths \
                 (up to {MAX_PATHS}) to Grep.",
                path_args.len()
            )));
        }
        // Classify every explicit path against the workspace sandbox before
        // any I/O (ADR-2, audit P0-2; plan 2026-10-10-sandbox-path-permission
        // D1). Inside and Outside both build the plan over the canonical
        // path (Outside enters the permission chain instead of being refused
        // here); any escape shape refuses the whole call. The canonical path
        // is used for the plan and the search, so the permission policies and
        // the file access agree on the target. When the model omits `path`,
        // the search anchors at the workspace root (TS parity: the TS tools
        // anchor at the session cwd).
        let mut paths: Vec<PathBuf> = Vec::with_capacity(path_args.len());
        for path_arg in &path_args {
            match crate::tools::classify_tool_path(
                self.workspace_paths.as_deref(),
                &self.fs,
                path_arg,
            ) {
                PathClassification::Inside(canonical) | PathClassification::Outside(canonical) => {
                    paths.push(canonical);
                }
                PathClassification::Escape { reason } => {
                    return Ok(tool_execution_error(crate::tools::escape_tool_error(
                        "Grep", path_arg, &reason,
                    )));
                }
            }
        }
        if !paths.is_empty() {
            for path in &paths {
                if !block_on_fs(self.fs.exists(path)).unwrap_or(false) {
                    return Ok(tool_execution_error(format!(
                        "Grep path does not exist: {}",
                        path.display()
                    )));
                }
            }
        }
        let paths = if paths.is_empty() {
            match self.workspace_paths.as_ref() {
                Some(workspace) => vec![workspace.root().to_path_buf()],
                None => Vec::new(),
            }
        } else {
            paths
        };
        let glob = input
            .get("glob")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let file_type = input
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
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
        // head_limit: 0 means unlimited (usize::MAX). Defaults are
        // mode-dependent: 30 for content (G-ADR-4), 250 otherwise.
        let head_limit_raw = input.get("head_limit").and_then(serde_json::Value::as_u64);
        let head_limit = match head_limit_raw {
            Some(0) => usize::MAX,
            Some(v) => v as usize,
            None => {
                if output_mode == "content" {
                    DEFAULT_HEAD_LIMIT_CONTENT
                } else {
                    DEFAULT_HEAD_LIMIT
                }
            }
        };
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
        let exclude_pattern = input
            .get("exclude_pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let plan = ExecutionPlan {
            accesses: paths
                .iter()
                .map(|path| ToolAccess {
                    kind: AccessKind::Search,
                    path: Some(path.to_string_lossy().into_owned()),
                    operation: AccessOperation::Read,
                })
                .collect(),
            display: ToolInputDisplay::Search {
                query: pattern.clone(),
                scope: (!paths.is_empty()).then(|| {
                    paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                }),
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
                    paths: if paths.is_empty() {
                        vec![default_search_root()]
                    } else {
                        paths
                    },
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
                    exclude_pattern,
                };
                Box::pin(search_contents(search, Some(fs), params))
            }),
        })
    }
}

/// Block on an injected filesystem probe without an async context.
///
/// Same shape as the probe helper in `path_security.rs`: a dedicated thread
/// drives the future on the shared blocking runtime (zero-IO contract —
/// the probe goes through the injected `DynFileSystem`, never `std::fs`).
fn block_on_fs<T: Send>(future: impl Future<Output = T> + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(move || crate::hook_engine::shared_blocking_runtime().block_on(future))
            .join()
            .expect("filesystem probe thread panicked")
    })
}

async fn search_contents(
    search: Arc<dyn DynContentSearch>,
    fs: Option<Arc<dyn DynFileSystem>>,
    params: SearchParams,
) -> ToolResult {
    let output_mode = params.output_mode.clone();
    let head_limit = params.head_limit;
    let search_roots: Vec<PathBuf> = params.paths.clone();
    match search.search(&params).await {
        Ok(result) => {
            let truncated = result.truncated;
            let total_matches = result.total_matches;
            let truncated_by_size = result.truncated_by_size;

            // Sensitive files (secrets, keys, credentials) never surface in
            // Grep results. mirri-os already filters at parse time; this is a
            // second layer guarding the rendered output.
            let filtered: Vec<&mirri_os::SearchMatch> = result
                .matches
                .iter()
                .filter(|m| !crate::tools::path_security::is_sensitive_file(Path::new(&m.path)))
                .collect();

            let output = if output_mode == "content" {
                // Content mode: block rendering (G-ADR-3) — matches cluster
                // into merged blocks with a line-range header.
                render_content_blocks(&filtered, &params, &search_roots, truncated, total_matches)
            } else if output_mode == "files" {
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
                    .map(|p| display_path(p, &search_roots))
                    .collect();
                RenderedContent::Text(if paths.is_empty() {
                    "No matches found".to_string()
                } else {
                    paths.join("\n")
                })
            } else {
                // Count mode: per-file counts (first-appearance order)
                // preceded by the spec's aggregate header. The provider
                // carries each file's occurrence count (`rg --count-matches`)
                // in `line_number`.
                let mut counts: Vec<(String, u64)> = Vec::new();
                for m in &filtered {
                    let path = display_path(&m.path, &search_roots);
                    if let Some((_, count)) = counts.iter_mut().find(|(p, _)| *p == path) {
                        *count += m.line_number;
                    } else {
                        counts.push((path, m.line_number));
                    }
                }
                RenderedContent::Text(if counts.is_empty() {
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
                })
            };

            // Truncation notes (design §3.3): two distinguishable templates.
            // In content mode the entry granularity is the rendering unit
            // (match rows + block headers); other modes keep the note
            // template over the match count.
            let note = truncated.then(|| {
                if truncated_by_size {
                    // US-G4 clause 3: the 10 MiB hard stdout cap fired —
                    // matches the legacy truncation-notice wording so callers
                    // (and snapshots) see one consistent template.
                    format!(
                        "[stdout truncated at {} bytes; incomplete trailing line omitted]",
                        mirri_os::MAX_OUTPUT_BYTES_NOTICE
                    )
                } else if head_limit == usize::MAX {
                    "Grep results truncated (output size limit reached)".to_string()
                } else {
                    format!("Grep results truncated at {head_limit} matches")
                }
            });
            if let RenderedContent::Blocks {
                text,
                footer,
                truncated: byte_truncated,
            } = output
            {
                let mut body = text;
                if let Some(footer) = footer {
                    body.push('\n');
                    body.push_str(&footer);
                }
                return ToolResult {
                    output: ToolOutput::text(if body.is_empty() {
                        // Excluded-to-empty and genuinely-empty share one
                        // wording (US-G3 clause 2).
                        "No non-sensitive matches found".to_string()
                    } else {
                        body
                    }),
                    is_error: false,
                    note,
                    stop_turn: false,
                    truncated: truncated || byte_truncated,
                    injection: None,
                };
            }
            let RenderedContent::Text(text) = output else {
                unreachable!("content mode is handled above");
            };
            ToolResult {
                output: ToolOutput::text(text),
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

/// Rendered output of a Grep invocation.
enum RenderedContent {
    /// Plain text (files / count modes).
    Text(String),
    /// Content-mode blocks: the block text, an optional truncation footer,
    /// and whether a display-side budget (entry count or bytes) truncated
    /// the output (complements the search layer's `truncated` flag).
    Blocks {
        text: String,
        footer: Option<String>,
        truncated: bool,
    },
}

/// A rendered content block: one file's clustered match/context lines.
struct ContentBlock {
    /// The file's display path (already relativized to its search root).
    display: String,
    /// Rendered rows in line order, with their file line number and match
    /// flag. Line numbers are kept here (not just in the header) so
    /// `-n:false` can drop the prefix at the last moment while the header
    /// range stays intact.
    rows: Vec<(u64, String, bool)>,
}

/// Cluster content matches into blocks and render them (G-ADR-3).
///
/// Pipeline: group by path (stable, preserving the search layer's root-group
/// order) → sort each group's rows by line number → merge windows that
/// overlap or touch (`start <= prev_end + 1`) → render each cluster as
/// `display:start-end` header plus `  {line}│{text}` rows with ` ← match`
/// suffixes.
///
/// Display-side caps (G-ADR-4), applied over the search layer's own
/// pagination:
///
/// - entry cap: at most `DEFAULT_HEAD_LIMIT_CONTENT` rows in aggregate;
/// - byte cap: at most [`BYTE_BUDGET`] bytes of block text; truncation
///   happens only at a block boundary (a single oversized block still
///   renders whole).
///
/// The byte cap does not apply when `head_limit` is unlimited (`0` —
/// US-G4 clause 3).
fn render_content_blocks(
    matches: &[&mirri_os::SearchMatch],
    params: &SearchParams,
    search_roots: &[PathBuf],
    search_truncated: bool,
    total_matches: usize,
) -> RenderedContent {
    // Group by path, preserving the incoming order (the search layer already
    // ordered content matches by root group and file). An order-preserving
    // map keeps multi-root output grouped by the `paths` array order.
    let mut groups: Vec<(String, Vec<&mirri_os::SearchMatch>)> = Vec::new();
    for m in matches {
        match groups.iter_mut().find(|(path, _)| path == &m.path) {
            Some((_, rows)) => rows.push(m),
            None => groups.push((m.path.clone(), vec![m])),
        }
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();
    for (path, mut rows) in groups {
        rows.sort_by_key(|m| m.line_number);
        let display = display_path(&path, search_roots);
        // Merge rows whose windows overlap or touch. Each row expands the
        // cluster to at least its own line; adjacent windows (gap 0) merge
        // too — `start <= prev_end + 1`.
        let mut cluster_rows: Vec<(u64, String, bool)> = Vec::new();
        let mut cluster_end: Option<u64> = None;
        for m in &rows {
            let inline = cluster_end.is_some_and(|end| {
                m.line_number <= end.saturating_add(1 + u64::from(context_total(params)))
            });
            if !inline && !cluster_rows.is_empty() {
                blocks.push(ContentBlock {
                    display: display.clone(),
                    rows: std::mem::take(&mut cluster_rows),
                });
            }
            // A multiline match payload (containing '\n') renders as one row
            // per physical line; the line number is the match's start line.
            let is_match = m.is_match;
            let start = m.line_number;
            for (index, text) in m.line.split('\n').enumerate() {
                cluster_rows.push((
                    start + index as u64,
                    text.to_string(),
                    is_match && index == 0,
                ));
            }
            cluster_end = Some(cluster_end.map_or(start, |end: u64| {
                end.max(start + m.line.split('\n').count() as u64 - 1)
            }));
        }
        if !cluster_rows.is_empty() {
            blocks.push(ContentBlock {
                display,
                rows: cluster_rows,
            });
        }
    }

    // Render with the display-side caps. Both caps are skipped entirely when
    // the caller asked for unlimited output (`head_limit: 0`).
    let unlimited = params.head_limit == usize::MAX;
    let mut text = String::new();
    let mut rendered_rows = 0usize;
    // Matches (not rows) rendered so far — the footer's entry count and
    // offset advice must follow the search layer's pagination granularity
    // (offset/head_limit count matches; context rows ride along for free).
    let mut rendered_matches = 0usize;
    let mut truncated = false;
    let mut byte_budget_hit = false;
    for block in &blocks {
        let block_text = render_block(block, params);
        let block_bytes = block_text.len();
        if !unlimited
            && !text.is_empty()
            && (rendered_rows + block.rows.len() > DEFAULT_HEAD_LIMIT_CONTENT
                || text.len() + block_bytes > BYTE_BUDGET)
        {
            truncated = true;
            byte_budget_hit = text.len() + block_bytes > BYTE_BUDGET
                && rendered_rows + block.rows.len() <= DEFAULT_HEAD_LIMIT_CONTENT;
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&block_text);
        rendered_rows += block.rows.len();
        rendered_matches += block
            .rows
            .iter()
            .filter(|(_, _, is_match)| *is_match)
            .count();
    }

    // The search layer's own pagination (head_limit/offset over match
    // counts) truncates before this renderer runs — surface it with the
    // entry-count footer so the caller knows the page is short.
    if search_truncated && !unlimited {
        truncated = true;
    }

    let footer = truncated.then(|| {
        // Matches rendered so far; total comes from the search layer's
        // match-count pagination (`total_matches`), covering matches beyond
        // this page. Both footer figures follow the match granularity — the
        // suggested `offset` resumes the search layer's pagination exactly
        // after the matches shown here (context rows are not paginated).
        let total_rows: usize = blocks
            .iter()
            .flat_map(|b| b.rows.iter())
            .filter(|(_, _, is_match)| *is_match)
            .count()
            .max(total_matches);
        if byte_budget_hit {
            format!(
                "Results truncated at {rendered_matches} of {total_rows} entries \
                 (byte budget reached). Use offset={replacement} to see more.",
                replacement = params.offset + rendered_matches
            )
        } else {
            format!(
                "Results truncated to {rendered_matches} entries (total: {total_rows}). \
                 Use offset={replacement} to see more.",
                replacement = params.offset + rendered_matches
            )
        }
    });

    RenderedContent::Blocks {
        text,
        footer,
        truncated,
    }
}

/// The combined context window a match expands to (mirrors `build_args`:
/// `-C` wins over `-A`/`-B`).
fn context_total(params: &SearchParams) -> u32 {
    if params.context > 0 {
        params.context
    } else {
        params.context_before.max(params.context_after)
    }
}

/// Render one block: `display:start-end` header plus `  {line}│{text}` rows.
///
/// `-n:false` drops the per-line number prefix; the header range always
/// keeps the numbers (the match stays addressable). Match rows carry the
/// ` ← match` marker.
fn render_block(block: &ContentBlock, params: &SearchParams) -> String {
    let start = block.rows.first().map(|(line, _, _)| *line).unwrap_or(0);
    let end = block.rows.last().map(|(line, _, _)| *line).unwrap_or(0);
    let mut out = format!("{}:{}-{}\n", block.display, start, end);
    let mut body = Vec::with_capacity(block.rows.len());
    for (line, text, is_match) in &block.rows {
        let mut row = String::new();
        if params.line_numbers {
            row.push_str(&format!("  {line}│"));
        } else {
            row.push_str("  ");
        }
        row.push_str(text);
        if *is_match {
            row.push_str(" ← match");
        }
        body.push(row);
    }
    out.push_str(&body.join("\n"));
    out
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

    /// Build `SearchParams` with GrepTool defaults and the given pattern /
    /// path / output_mode. The head_limit default is mode-dependent: 30
    /// for content, 250 otherwise — mirror `resolve_execution`.
    fn grep_params(pattern: &str, path: PathBuf, output_mode: &str) -> SearchParams {
        SearchParams {
            pattern: pattern.to_string(),
            paths: vec![path],
            output_mode: output_mode.to_string(),
            head_limit: if output_mode == "content" {
                DEFAULT_HEAD_LIMIT_CONTENT
            } else {
                DEFAULT_HEAD_LIMIT
            },
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
        // Content mode renders a block: range header + numbered match row.
        assert_eq!(text, "a.txt:2-2\n  2│beta ← match");
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
            grep_params("fn main", tmp.path().to_path_buf(), "files"),
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
        // files mode,
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
            grep_params("fn main", tmp.path().to_path_buf(), "files"),
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
            grep_params("x", tmp.path().to_path_buf(), "count"),
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

        let mut params = grep_params("fn main", tmp.path().to_path_buf(), "files");
        params.glob = Some("*.rs".to_string());
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert_eq!(paths[0], "a.rs");
    }

    #[tokio::test]
    async fn should_default_to_files_when_output_mode_is_omitted() {
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
        // files mode: one path per line.
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
        // No matches renders the shared empty-result wording.
        assert_eq!(output_text(&result), "No non-sensitive matches found");
    }

    // ------------------------------------------------------------------ //
    //  New tests — the 10 additional parameters                          //
    // ------------------------------------------------------------------ //

    #[tokio::test]
    async fn should_filter_by_type_when_type_parameter_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn target() {}\n").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "fn target() {}\n").unwrap();

        let mut params = grep_params("target", tmp.path().to_path_buf(), "files");
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
        assert_eq!(output_text(&result), "a.txt:1-1\n  1│Hello World ← match");
    }

    #[tokio::test]
    async fn should_omit_line_numbers_when_line_numbers_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\n").unwrap();

        let mut params = grep_params("beta", tmp.path().to_path_buf(), "content");
        params.line_numbers = false;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // -n:false drops the per-row prefix; the block header keeps the
        // range.
        assert_eq!(output_text(&result), "a.txt:2-2\n  beta ← match");
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
        assert_eq!(output_text(&result), "a.txt:2-2\n  2│beta ← match");
    }

    #[tokio::test]
    async fn should_show_after_context_lines_when_context_after_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "line1\nmatch\nline3\nline4\n").unwrap();

        let mut params = grep_params("match", tmp.path().to_path_buf(), "content");
        params.context_after = 1;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // With -A 1, the match line plus one context line render inside one
        // block spanning lines 2-3.
        let text = output_text(&result);
        assert!(
            text.contains("a.txt:2-3"),
            "expected block header in {text}"
        );
        assert!(
            text.contains("2│match ← match"),
            "expected match row in {text}"
        );
        assert!(
            text.contains("3│line3"),
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
            text.contains("a.txt:2-3"),
            "expected block header in {text}"
        );
        assert!(
            text.contains("3│match ← match"),
            "expected match row in {text}"
        );
        assert!(
            text.contains("2│line2"),
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
        // -C 1 gives one line before and one after, merged into one block
        // spanning lines 2-4.
        assert!(
            text.contains("a.txt:2-4"),
            "expected block header in {text}"
        );
        assert!(
            text.contains("2│line2"),
            "expected before-context in {text}"
        );
        assert!(
            text.contains("3│match ← match"),
            "expected match row in {text}"
        );
        assert!(text.contains("4│line4"), "expected after-context in {text}");
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
        // 2 matches collected → 2 match rows (plus their block headers).
        assert_eq!(text.matches("← match").count(), 2);
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
        // 5 total, skip 2 → 3 match rows returned, not truncated.
        assert_eq!(output_text(&result).matches("← match").count(), 3);
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
        // 10 total, skip 3, take 4 → 4 match rows returned, truncated
        assert_eq!(output_text(&result).matches("← match").count(), 4);
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
        // The single multiline match renders as one block; the payload
        // spans the two file lines, rendered per physical line with the
        // match marker on the start line.
        let text = output_text(&result);
        assert!(text.contains("a.txt:1-2"), "block header missing: {text}");
        assert!(
            text.contains("1│start ← match"),
            "match start row missing: {text}"
        );
        assert!(text.contains("2│end"), "second payload row missing: {text}");
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
        let params_no_ignore = grep_params("secret_term", tmp.path().to_path_buf(), "files");
        let result = search_contents(local_search(), None, params_no_ignore).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let paths: Vec<&str> = text.lines().collect();
        assert!(
            !paths.iter().any(|p| p.ends_with("ignored.txt")),
            "ignored.txt should be excluded without include_ignored, got {paths:?}"
        );

        // With include_ignored: both should match
        let mut params_ignore = grep_params("secret_term", tmp.path().to_path_buf(), "files");
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
        let mut params = grep_params("secret_term", tmp.path().to_path_buf(), "files");
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
        // -n false drops the row prefix, the block header keeps the range.
        assert_eq!(text, "a.txt:1-1\n  Hello World ← match");
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
        assert_eq!(text.matches("← match").count(), 3);
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
        // type=rust should filter out a.txt — only a.rs blocks render.
        assert!(!text.contains("a.txt"), "only .rs paths expected: {text}");
        assert!(text.contains("a.rs:2-3"), "block header expected: {text}");
        // -A 1 should include the context row "line3"
        assert!(
            text.contains("3│line3"),
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
    /// sandbox-path-permission D5), When the `path` descriptions are
    /// inspected, Then they state the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = GrepTool::new(local_search(), None, local_fs());
        let any_of = tool.parameters()["properties"]["path"]["anyOf"]
            .as_array()
            .expect("path uses anyOf");
        for branch in any_of {
            let description = branch["description"].as_str().expect("description present");
            assert!(description.contains("workspace sandbox"), "{description}");
            assert!(description.contains("user authorization"), "{description}");
        }
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
            "output_mode": "files"
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
                        is_match: true,
                    }],
                    total_matches: 1,
                    truncated: false,
                    truncated_by_size: false,
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
            is_match: true,
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
    async fn should_render_content_mode_as_blocks_when_searching() {
        // Given a file with a match, When content-mode grep runs,
        // Then the output is block text (not JSON): a `path:start-end`
        // header plus numbered match rows carrying the ` ← match` marker.
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
            text, "a.txt:2-2\n  2│beta ← match",
            "content mode must be block text, got: {text}"
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
        // -n:false drops the per-row prefix; the block header keeps the
        // range.
        assert_eq!(output_text(&result), "a.txt:1-1\n  beta ← match");
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
            grep_params("x", tmp.path().to_path_buf(), "count"),
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
            grep_params("x", tmp.path().to_path_buf(), "count"),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.starts_with("Found 1 occurrence across 1 file."),
            "singular header missing: {text}"
        );
    }

    // ------------------------------------------------------------------ //
    //  Task 5 Step 1 — schema + multi-path parsing + existence            //
    // ------------------------------------------------------------------ //

    #[test]
    fn should_accept_path_arrays_and_exclude_pattern_in_schema() {
        let tool = GrepTool::new(local_search(), None, local_fs());
        let props = tool.parameters()["properties"].as_object().unwrap();
        // anyOf is a schema keyword, not a type value — assert its presence
        // and the array branch's maxItems.
        let any_of = props["path"]["anyOf"].as_array().expect("path uses anyOf");
        assert_eq!(any_of[0]["type"], "string");
        assert_eq!(any_of[1]["type"], "array");
        assert_eq!(any_of[1]["maxItems"], 32);
        assert_eq!(props["exclude_pattern"]["type"], "string");
        assert_eq!(props["head_limit"]["default"], 30);
    }

    #[tokio::test]
    async fn should_reject_when_path_array_exceeds_thirty_two_entries() {
        let paths: Vec<String> = (0..33).map(|i| format!("/tmp/f{i}.txt")).collect();
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(serde_json::json!({"pattern":"x","path":paths}))
            .unwrap();
        // maxItems lives at the jsonschema validation layer and never reaches
        // resolve_execution — the tool layer re-checks the length explicitly.
        let ToolExecution::Error(err) = execution else {
            panic!("expected error")
        };
        assert!(
            err.message.contains("32"),
            "error must state the 32-path limit: {}",
            err.message
        );
        assert!(
            err.message.contains("glob"),
            "error must point to the glob tool for narrowing: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn should_name_the_missing_element_when_a_path_in_the_array_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("exists");
        std::fs::create_dir_all(&existing).unwrap();
        let missing = tmp.path().join("does-not-exist");
        let execution = GrepTool::new(local_search(), None, local_fs())
            .resolve_execution(serde_json::json!({
                "pattern": "x",
                "path": [existing.to_string_lossy(), missing.to_string_lossy()]
            }))
            .unwrap();
        let ToolExecution::Error(err) = execution else {
            panic!("expected error")
        };
        assert!(
            err.message.contains("does-not-exist"),
            "error must name the missing element: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn should_refuse_the_whole_call_when_any_array_path_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let inside = root.join("inside");
        std::fs::create_dir_all(&inside).unwrap();

        let tool = GrepTool::new(
            local_search(),
            Some(Arc::new(WorkspacePaths::new(root))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "path": [inside.to_string_lossy(), "../escape"],
            }))
            .unwrap();
        let ToolExecution::Error(error) = execution else {
            panic!("expected an escape refusal for the whole call");
        };
        assert!(
            error.message.contains("escapes the workspace sandbox"),
            "refusal uses the escape wording: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn should_generate_one_access_per_path_in_multi_path_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let tool = GrepTool::new(
            local_search(),
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "needle",
                "path": [first.to_string_lossy(), second.to_string_lossy()],
            }))
            .unwrap();
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected a runnable execution");
        };
        assert_eq!(
            plan.accesses.len(),
            2,
            "multi-path mode must generate one access per path"
        );
    }

    // ------------------------------------------------------------------ //
    //  Task 5 Step 3 — block rendering + byte budget                     //
    // ------------------------------------------------------------------ //

    #[tokio::test]
    async fn should_render_content_as_blocks_with_merged_context() {
        let tmp = tempfile::tempdir().unwrap();
        // Lines 3 and 4 match; -C 1 → windows [2,4] ∪ [3,5] overlap → one
        // merged block [2,5].
        std::fs::write(tmp.path().join("a.txt"), "l1\nl2\nneedle\nneedle\nl5\nl6\n").unwrap();
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.context = 1;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("a.txt:2-5"),
            "block header must span the merged window: {text}"
        );
        assert!(text.contains("← match"), "match marker missing: {text}");
        assert_eq!(
            text.matches("a.txt:").count(),
            1,
            "overlapping windows merge into one block: {text}"
        );
        // The block renders all four window lines with line-number
        // prefixes, and both match lines carry the marker.
        assert!(text.contains("2│l2"), "context line 2 missing: {text}");
        assert!(
            text.contains("3│needle ← match"),
            "match line 3 missing: {text}"
        );
        assert!(
            text.contains("4│needle ← match"),
            "match line 4 missing: {text}"
        );
        assert!(text.contains("5│l5"), "context line 5 missing: {text}");
    }

    #[tokio::test]
    async fn should_render_single_match_as_block_without_context() {
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
        assert!(
            text.contains("a.txt:2-2"),
            "single-match block header spans one line: {text}"
        );
        assert!(text.contains("2│beta ← match"), "match row missing: {text}");
    }

    #[tokio::test]
    async fn should_keep_block_header_line_numbers_when_line_numbers_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let mut params = grep_params("beta", tmp.path().to_path_buf(), "content");
        params.line_numbers = false;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // -n:false drops the per-line prefix but the block header keeps the
        // line range so the match stays addressable.
        assert!(
            text.contains("a.txt:2-2"),
            "block header keeps range: {text}"
        );
        assert!(text.contains("beta ← match"), "match row missing: {text}");
        assert!(
            !text.contains("│"),
            "per-line prefixes must be dropped: {text}"
        );
    }

    #[tokio::test]
    async fn should_truncate_at_block_boundary_when_byte_budget_is_reached() {
        let tmp = tempfile::tempdir().unwrap();
        // 40 blocks of ~500-byte matched lines separated by filler lines,
        // -C 0. head_limit 40 keeps the count cap away so the byte budget
        // fires first.
        let mut body: Vec<String> = Vec::new();
        for i in 0..40 {
            body.push(format!("{} needle-{}", "x".repeat(490), i));
            body.push(format!("filler-{i}"));
        }
        std::fs::write(tmp.path().join("a.txt"), body.join("\n") + "\n").unwrap();
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.head_limit = 40;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("(byte budget reached)"),
            "byte-budget footer missing: {text}"
        );
        assert!(
            text.len() < BYTE_BUDGET + 600,
            "output must stop at a block boundary after the budget ({} bytes)",
            text.len()
        );
        // Truncation must be reported in the note as well.
        assert!(
            result.truncated,
            "byte-budget truncation must flag truncated"
        );
    }

    #[tokio::test]
    async fn should_skip_byte_budget_when_head_limit_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        // 40 matches of ~500-byte lines, head_limit 0 → unlimited → the
        // byte budget does not apply (US-G4 clause 3).
        let body: Vec<String> = (0..40)
            .map(|i| format!("{} needle-{}", "x".repeat(490), i))
            .collect();
        std::fs::write(tmp.path().join("a.txt"), body.join("\n") + "\n").unwrap();
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.head_limit = usize::MAX;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            !text.contains("(byte budget reached)"),
            "byte budget must not apply when head_limit is unlimited: {text}"
        );
        assert!(
            text.matches("← match").count() == 40,
            "all 40 matches expected, got {}",
            text.matches("← match").count()
        );
    }

    #[tokio::test]
    async fn should_report_entry_truncation_with_total_when_head_limit_is_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let body: Vec<String> = (0..5).map(|i| format!("needle-{i}")).collect();
        std::fs::write(tmp.path().join("a.txt"), body.join("\n") + "\n").unwrap();
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.head_limit = 2;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("Results truncated to 2 entries (total: 5)."),
            "entry-count footer missing: {text}"
        );
        assert!(
            text.contains("Use offset=") || text.contains("Use offset=…"),
            "offset guidance missing: {text}"
        );
    }

    #[tokio::test]
    async fn should_report_offset_advice_on_the_match_granularity_when_context_lines_are_rendered()
    {
        let tmp = tempfile::tempdir().unwrap();
        // 3 hits separated by blank filler lines, -C 1 — each hit renders
        // with 1 context row before it, so 2 hits render 4 rows while the
        // offset pagination counts only matches (search-layer head_limit=2).
        let body = "needle1\nfiller\nneedle2\nfiller\nneedle3\n";
        std::fs::write(tmp.path().join("a.txt"), body).unwrap();
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.context = 1;
        params.head_limit = 2;
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        // The footer must suggest offset=2 (matches rendered), not offset=4
        // (rows incl. context) — the search layer paginates by match count.
        assert!(
            text.contains("Use offset=2 to see more."),
            "match-granularity offset advice missing: {text}"
        );
        assert!(
            !text.contains("Use offset=4"),
            "row-granularity offset advice must not appear: {text}"
        );
    }

    /// A [`mirri_os::DynContentSearch`] double that returns a fixed match
    /// with the hard-cap `truncated_by_size` flag set (US-G4 clause 3) — no
    /// ripgrep subprocess is spawned, no 10 MiB of output is materialized.
    struct HardCapSearch(mirri_os::SearchMatch);

    impl mirri_os::DynContentSearch for HardCapSearch {
        fn search<'a>(&'a self, _params: &'a SearchParams) -> mirri_os::DynSearchFuture<'a> {
            Box::pin(async move {
                Ok(mirri_os::SearchResult {
                    matches: vec![mirri_os::SearchMatch {
                        path: self.0.path.clone(),
                        line_number: self.0.line_number,
                        line: self.0.line.clone(),
                        column: self.0.column,
                        is_match: true,
                    }],
                    total_matches: 1,
                    truncated: true,
                    truncated_by_size: true,
                })
            })
        }
    }

    /// Given head_limit 0 (unlimited) and a search provider that hit the
    /// 10 MiB hard stdout cap, When a content search renders, Then the
    /// output carries the hard-cap notice instead of silently truncating.
    #[tokio::test]
    async fn should_render_the_hard_cap_notice_when_unlimited_search_hits_the_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("big.txt"), "needle\n").unwrap();

        let fake = HardCapSearch(mirri_os::SearchMatch {
            path: tmp.path().join("big.txt").to_string_lossy().into_owned(),
            line_number: 1,
            line: "needle".to_string(),
            column: 0,
            is_match: true,
        });
        let mut params = grep_params("needle", tmp.path().to_path_buf(), "content");
        params.head_limit = usize::MAX;
        let result = search_contents(Arc::new(fake), None, params).await;

        assert!(!result.is_error);
        assert!(result.truncated, "hard-cap truncation must flag truncated");
        assert_eq!(
            result.note.as_deref(),
            Some("[stdout truncated at 10485760 bytes; incomplete trailing line omitted]"),
            "hard-cap notice missing, note was: {:?}",
            result.note
        );
    }

    #[tokio::test]
    async fn should_return_no_sensitive_empty_message_when_all_matches_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
        let mut params = grep_params("alpha", tmp.path().to_path_buf(), "content");
        params.exclude_pattern = Some("alpha".to_string());
        let result = search_contents(local_search(), None, params).await;
        assert!(!result.is_error);
        // Every match excluded → the same wording as an empty search
        // (US-G3 clause 2; no second empty-result semantics).
        assert_eq!(output_text(&result), "No non-sensitive matches found");
    }
}
