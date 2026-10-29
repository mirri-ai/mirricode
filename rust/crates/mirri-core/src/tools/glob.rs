//! `GlobTool` — discover files by glob pattern.
//!
//! Uses the [`GlobMatch`] abstraction (a ripgrep-style walker that respects
//! `.gitignore`), returning matches relative to the search root, sorted by
//! modification time (newest first). An optional `filter` (substring or
//! regex) narrows the results before they are capped at `limit` matches
//! (default 200; `0` requests the full set).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::{PathClassification, WorkspacePaths, tool_execution_error};
use mirri_os::{DynFileSystem, DynGlobMatch, GlobParams};

/// Fallback search root used when neither the model provides a `path` nor
/// the tool carries a workspace sandbox (`None`, legacy hosts / unit tests):
/// the sandbox root doubles as the search root in the guarded shape (TS
/// parity: the TS tools anchor at the session cwd).
fn default_search_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `Glob` — find files by glob pattern.
pub struct GlobTool {
    /// The glob provider every search goes through (zero-IO T13a).
    glob: Arc<dyn DynGlobMatch>,
    /// The session's workspace sandbox this tool may search inside; `None`
    /// disables path validation (legacy hosts / unit tests, ADR-2).
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider used by workspace path validation (zero-IO
    /// T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl GlobTool {
    /// Create the tool with the injected glob provider (the engine default
    /// is the local one) and the session's workspace sandbox. The sandbox
    /// root doubles as the default search root when the model omits `path`
    /// (TS parity: the TS tools anchor at the session cwd); `None` keeps the
    /// unguarded behavior (search root falls back to the process cwd).
    pub fn new(
        glob: Arc<dyn DynGlobMatch>,
        workspace_paths: Option<Arc<WorkspacePaths>>,
        fs: Arc<dyn DynFileSystem>,
    ) -> Self {
        Self {
            glob,
            workspace_paths,
            fs,
        }
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by name pattern, sorted by modification time (most recent \
         first).\n\nRespects `.gitignore`, `.ignore`, and `.rgignore` by \
         default — set `include_ignored` to also match ignored files. \
         Sensitive files (`.env` etc.) are always filtered out. Matches are \
         files only; to find a directory, glob for a file inside it (e.g. \
         `**/fixtures/**`).\n\nNarrow results without a shell pipeline:\n- \
         `filter` narrows matched paths by substring or regex (`\"skill\"` or \
         `\"test-\\\\d+\"`), case-insensitive — replaces `rg --files | rg -i \
         'x'`.\n- `limit` controls how many paths are returned (default \
         200). When more match, the result says so and tells you the total, \
         so you can refine `pattern` or `filter` instead of paging blindly.\n\
         - Sorting is most-recently-modified first — ideal for \"the newest \
         X\": `{\"pattern\":\"*.log\",\"filter\":\"ws-\"}` finds the latest \
         ws-*.log without `ls -t | head`.\n\nGood patterns:\n- `*.ts` — \
         extension match at any depth (a bare pattern without `/` matches \
         recursively)\n- `src/**/*.ts` — recursive walk anchored under \
         src/\n- `*.{ts,tsx}` — brace expansion; `{src,test}/**/*.ts` \
         cartesian too\n\nAvoid recursing into dependency/build dirs even \
         with an anchor (`node_modules/**` can flood results): prefer \
         specific subpaths like `node_modules/react/src/**/*.js`."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match, e.g. '**/*.rs'. Multiple patterns may be comma-separated."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in. Defaults to the working directory. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "include_ignored": {
                        "type": "boolean",
                        "description": "When true, also match files excluded by ignore files such as .gitignore/.ignore/.rgignore. Sensitive files remain filtered out. Defaults to false."
                    },
                    "filter": {
                        "type": "string",
                        "description": "Optional pattern applied to the matched paths before limiting. Plain text matches as a case-insensitive substring; regex trigger characters (\\ + * ? ( ) [ ] { } | ^ $) switch it to a case-insensitive regex (falling back to substring if it fails to compile). A leading '!' negates the match. Applied before 'limit'."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of matches to return after filtering, newest first. Defaults to 200. Pass 0 to request the full result set (large result sets are truncated by an internal hard cap).",
                        "default": 200
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
                "Glob requires a string 'pattern' argument",
            ));
        };
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
                        "Glob", &path, &reason,
                    )));
                }
            },
            None => self
                .workspace_paths
                .as_ref()
                .map(|workspace| workspace.root().to_path_buf()),
        };
        let include_ignored = input
            .get("include_ignored")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let filter = input
            .get("filter")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let limit = match input.get("limit") {
            Some(value) => match value.as_u64() {
                Some(limit) => limit as usize,
                None => {
                    return Ok(tool_execution_error(
                        "Glob requires 'limit' to be a non-negative integer",
                    ));
                }
            },
            None => DEFAULT_GLOB_LIMIT,
        };

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
            approval_rule: "Glob".to_string(),
            stop_batch_after_this: false,
        };

        let glob = Arc::clone(&self.glob);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(glob_files(
                    glob,
                    pattern,
                    path,
                    include_ignored,
                    filter,
                    limit,
                ))
            }),
        })
    }
}

/// Default number of matches returned after filtering (US-GL3).
const DEFAULT_GLOB_LIMIT: usize = 200;

/// Characters that switch `filter` from plain substring to regex semantics.
/// `.` is deliberately absent: dots are ubiquitous in file paths and plain
/// substring matching covers them.
const REGEX_TRIGGER: &[char] = &[
    '\\', '+', '*', '?', '(', ')', '[', ']', '{', '}', '|', '^', '$',
];

/// Apply the user `filter` to the (sensitive-filtered) match list.
///
/// Plain text matches as a case-insensitive substring; a filter containing
/// any [`REGEX_TRIGGER`] character is compiled as a case-insensitive regex,
/// falling back to substring matching when compilation fails. A leading `!`
/// negates the match (keep the paths that do **not** match).
fn apply_filter(paths: &[String], filter: &str) -> Vec<String> {
    let (negated, pat) = filter
        .strip_prefix('!')
        .map(|p| (true, p))
        .unwrap_or((false, filter));
    let is_regex = pat.chars().any(|c| REGEX_TRIGGER.contains(&c));
    let hit: Box<dyn Fn(&str) -> bool> = if is_regex {
        match regex::Regex::new(&format!("(?i){pat}")) {
            Ok(re) => Box::new(move |p| re.is_match(p)),
            Err(_) => {
                let f = pat.to_lowercase();
                Box::new(move |p: &str| p.to_lowercase().contains(&f))
            }
        }
    } else {
        let f = pat.to_lowercase();
        Box::new(move |p: &str| p.to_lowercase().contains(&f))
    };
    paths
        .iter()
        .filter(|p| hit(p) != negated)
        .cloned()
        .collect()
}

async fn glob_files(
    glob: Arc<dyn DynGlobMatch>,
    pattern: String,
    path: Option<PathBuf>,
    include_ignored: bool,
    filter: Option<String>,
    limit: usize,
) -> ToolResult {
    let params = GlobParams {
        pattern,
        path: path.unwrap_or_else(default_search_root),
        include_ignored,
        // Tool-layer limit lands after filtering; the search layer collects
        // unbounded (guarded by its own hard caps).
        limit: 0,
    };
    match glob.glob(&params).await {
        Ok(result) => {
            let search_truncated = result.truncated;
            // Sensitive files (secrets, keys, credentials) never surface in
            // Glob results — the tool layer's second defense line on top of
            // the search layer's collection-time filter.
            let matches: Vec<String> = result
                .matches
                .iter()
                .filter(|m| !crate::tools::path_security::is_sensitive_file(Path::new(m)))
                .cloned()
                .collect();
            // Post-filter truncation pipeline: search layer full set →
            // sensitive second filter → user filter → limit (newest first).
            let filtered = match &filter {
                Some(filter) => apply_filter(&matches, filter),
                None => matches,
            };
            let after_filter = filtered.len();
            let total_matched = result.total_matched;
            let returned: Vec<String> = if limit == 0 {
                filtered
            } else {
                filtered.into_iter().take(limit).collect()
            };
            let n = returned.len();
            let truncated = n < after_filter || search_truncated;

            let mut lines: Vec<String> = Vec::new();
            if total_matched == 0 {
                lines.push("No matches found".to_string());
            } else {
                let header = if filter.is_some() {
                    format!(
                        "{n} matches (of {total_matched} matching before filtering, {after_filter} after filter) — most recent first"
                    )
                } else {
                    format!("{n} matches — most recent first")
                };
                lines.push(header);
                lines.extend(returned);
                if limit != 0 && n < after_filter {
                    lines.push(format!(
                        "[Results truncated at {n} of {after_filter} — refine pattern or filter, or pass limit]"
                    ));
                } else if limit == 0 && search_truncated {
                    lines.push("[Results truncated — limit: 0 with large result set; pass a smaller limit or refine pattern]".to_string());
                }
            }

            let output = lines.join("\n");
            let note = truncated
                .then(|| format!("Glob results truncated at {n} of {after_filter} matches"));
            ToolResult {
                output: ToolOutput::text(output),
                is_error: false,
                note,
                stop_turn: false,
                truncated,
                injection: None,
            }
        }
        Err(error) => ToolResult::error(format!("Glob failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine-default local glob provider, as the registered tool
    /// receives it.
    fn local_glob() -> Arc<dyn DynGlobMatch> {
        Arc::new(mirri_os::LocalGlob::new())
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

    /// Run `glob_files` with the local provider and no filter / unlimited
    /// limit (the pre-T4 default shape for the direct-pipeline tests).
    async fn run_glob_files(
        pattern: &str,
        path: Option<PathBuf>,
        include_ignored: bool,
    ) -> ToolResult {
        glob_files(
            local_glob(),
            pattern.to_string(),
            path,
            include_ignored,
            None,
            0,
        )
        .await
    }

    #[tokio::test]
    async fn should_return_all_matches_when_globbing_with_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "text\n").unwrap();

        let result = run_glob_files("*.rs", Some(tmp.path().to_path_buf()), false).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert_eq!(matches.len(), 3);
        assert!(matches.contains(&"a.rs"));
        assert!(matches.contains(&"b.rs"));
        assert!(
            matches.contains(&"2 matches — most recent first"),
            "count header must lead the list: {text}"
        );
    }

    #[tokio::test]
    async fn should_return_recursive_matches_when_globbing_with_a_nested_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("src/deep/lib.rs"), "pub fn f() {}\n").unwrap();

        let result = run_glob_files("**/*.rs", Some(tmp.path().to_path_buf()), false).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert!(matches.contains(&"src/main.rs"), "{matches:?}");
        assert!(matches.contains(&"src/deep/lib.rs"), "{matches:?}");
    }

    #[tokio::test]
    async fn should_return_empty_match_list_when_globbing_with_no_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "text\n").unwrap();
        let result = run_glob_files("*.rs", Some(tmp.path().to_path_buf()), false).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text, "No matches found");
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn should_return_error_result_when_globbing_with_an_invalid_pattern() {
        let result = run_glob_files("[", Some(PathBuf::from(".")), false).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn should_return_error_result_when_globbing_with_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does_not_exist");
        let result = run_glob_files("*.rs", Some(missing), false).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn should_include_ignored_files_when_include_ignored_is_true() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "secret\n").unwrap();
        std::fs::write(tmp.path().join("normal.txt"), "visible\n").unwrap();
        // Create a .gitignore that excludes ignored.txt.
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        // Initialize a git repo so the walker honours .gitignore.
        std::process::Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output()
            .ok();

        // Without include_ignored: only normal.txt should match.
        let result = run_glob_files("*.txt", Some(tmp.path().to_path_buf()), false).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("normal.txt"), "{text}");
        assert!(
            !text.contains("ignored.txt"),
            "ignored.txt should be excluded, got {text}"
        );

        // With include_ignored: both should match.
        let result = run_glob_files("*.txt", Some(tmp.path().to_path_buf()), true).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("normal.txt"), "{text}");
        assert!(
            text.contains("ignored.txt"),
            "ignored.txt should be included with include_ignored, got {text}"
        );
    }

    #[tokio::test]
    async fn should_filter_sensitive_files_from_glob_results() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "hello\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(tmp.path().join("server.pem"), "PRIVATE\n").unwrap();
        std::fs::create_dir_all(tmp.path().join(".ssh")).unwrap();
        std::fs::write(tmp.path().join(".ssh/id_rsa"), "KEY\n").unwrap();

        // `**` matches every file, including hidden ones; sensitive files must
        // still never appear.
        let result = run_glob_files("**", Some(tmp.path().to_path_buf()), true).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("notes.txt"), "{text}");
        assert!(!text.contains(".env"), "secrets must be filtered: {text}");
        assert!(
            !text.contains("server.pem"),
            "keys must be filtered: {text}"
        );
        assert!(
            !text.contains(".ssh"),
            "ssh material must be filtered: {text}"
        );
    }
    // -------------------------------------------------------------------------
    // T-13a-4: injected glob provider (zero-IO injection)
    // -------------------------------------------------------------------------

    /// A [`mirri_os::DynGlobMatch`] double returning a fixed result set
    /// (T-13a-4) — no filesystem is touched.
    struct FixedGlob(Vec<String>);

    impl mirri_os::DynGlobMatch for FixedGlob {
        fn glob<'a>(&'a self, _params: &'a mirri_os::GlobParams) -> mirri_os::DynGlobFuture<'a> {
            Box::pin(async move {
                Ok(mirri_os::GlobResult {
                    matches: self.0.clone(),
                    truncated: false,
                    // Fixture doubles are small; nothing is ever skipped.
                    total_matched: self.0.len(),
                })
            })
        }
    }

    /// Given the Glob tool with the session's workdir injected as its
    /// default root, When the pattern has no `path` anchor, Then the
    /// search runs in the session workdir — not the process cwd
    /// (audit P0-1).
    #[tokio::test]
    async fn should_search_the_session_workdir_when_pattern_has_no_path_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("session-workdir-a1.txt"), "x\n").unwrap();

        let tool = GlobTool::new(
            local_glob(),
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "session-workdir-*.txt"
            }))
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
        })
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert_eq!(
            matches,
            vec!["1 matches — most recent first", "session-workdir-a1.txt"],
            "expected the session workdir to be searched, got: {text}"
        );
    }

    /// Given the Glob tool with an injected fake provider, When a search
    /// executes, Then the fake's fixed matches are returned verbatim — the
    /// search went through the injected provider, not the local one
    /// (T-13a-4).
    #[tokio::test]
    async fn should_return_the_injected_providers_matches_when_globbing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real-file.txt"), "on disk\n").unwrap();

        let tool = GlobTool::new(
            Arc::new(FixedGlob(vec!["from-fake.txt".to_string()])),
            None,
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({ "pattern": "*.txt" }))
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
        })
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert_eq!(
            matches,
            vec!["1 matches — most recent first", "from-fake.txt"],
            "the injected provider's matches must be returned verbatim behind \
             the count header: {text}"
        );
    }

    // -------------------------------------------------------------------------
    // Workspace path guard (ADR-2, audit P0-2)
    // -------------------------------------------------------------------------

    /// Given a workspace root and a clean absolute path outside it, When
    /// Glob is called with that path, Then the plan is built over the
    /// canonical outside path and the search proceeds — the tool no longer
    /// refuses outside paths, the permission chain decides (plan
    /// 2026-10-10-sandbox-path-permission, D1). The audit scenario (a
    /// subagent Glob enumerating the engine's own storage) is now gated by
    /// that chain instead of the tool.
    #[tokio::test]
    async fn should_glob_an_outside_directory_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();

        let tool = GlobTool::new(
            local_glob(),
            Some(Arc::new(WorkspacePaths::new(root))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "**",
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
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        })
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        let text = output_text(&result);
        assert!(
            text.contains("secret.txt"),
            "the outside directory must be searchable: {text}"
        );
    }

    /// Given a workspace root, When Glob is called with a relative path that
    /// lexically escapes the root (`..`), Then it is refused with the escape
    /// wording — a relative path can never declare outside intent.
    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_globbing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();

        let tool = GlobTool::new(
            local_glob(),
            Some(Arc::new(WorkspacePaths::new(root))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "**",
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

    /// Given an additional dir registered on the sandbox, When Glob targets
    /// it, Then the search proceeds normally — registered dirs are part of
    /// the allowed search surface (ADR-2).
    #[tokio::test]
    async fn should_allow_glob_inside_registered_additional_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let additional = tmp.path().join("additional");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&additional).unwrap();
        std::fs::write(additional.join("extra.rs"), "fn extra() {}\n").unwrap();
        let workspace = WorkspacePaths::new(root);
        workspace.add_additional(additional.clone());

        let tool = GlobTool::new(local_glob(), Some(Arc::new(workspace)), local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "pattern": "*.rs",
                "path": additional.to_string_lossy(),
            }))
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
        })
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert_eq!(
            matches,
            vec!["1 matches — most recent first", "extra.rs"],
            "the registered additional dir must be searchable: {text}"
        );
    }

    /// Given the Glob tool's parameter schema (Task 5, plan 2026-10-10-
    /// sandbox-path-permission D5), When the `path` description is
    /// inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = GlobTool::new(local_glob(), None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }

    /// Given the Glob tool's parameter schema, When the properties are
    /// inspected, Then the deprecated `include_dirs` parameter is absent.
    #[test]
    fn should_not_expose_deprecated_include_dirs_in_schema() {
        let tool = GlobTool::new(local_glob(), None, local_fs());
        let props = tool.parameters()["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("include_dirs"),
            "include_dirs must be removed"
        );
    }

    // -------------------------------------------------------------------------
    // Task 4 (plan grep-glob-capability-upgrade): filter + post-filter limit
    // with count header
    // -------------------------------------------------------------------------

    /// Build the Glob tool anchored at `root` as its default search root (the
    /// session-workdir shape), so `path` can be omitted in test inputs.
    fn anchored_tool(root: &Path) -> GlobTool {
        GlobTool::new(
            local_glob(),
            Some(Arc::new(WorkspacePaths::new(root.to_path_buf()))),
            local_fs(),
        )
    }

    /// Resolve and execute the tool with `input`, returning the result.
    async fn run_glob(tool: &GlobTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution");
        };
        execute(crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        })
        .await
    }

    /// Given matches with mixed-case names, When a plain-text `filter` is
    /// passed, Then only case-insensitive substring hits are returned with
    /// the full count header (US-GL1/US-GL2).
    #[tokio::test]
    async fn should_filter_matches_by_case_insensitive_substring_when_filter_is_plain_text() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Skill-Registry.ts"), "x").unwrap();
        std::fs::write(tmp.path().join("plain.txt"), "x").unwrap();

        let result = run_glob(
            &anchored_tool(tmp.path()),
            serde_json::json!({ "pattern": "*", "filter": "skill" }),
        )
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        let text = output_text(&result);
        assert!(text.contains("Skill-Registry.ts"), "{text}");
        assert!(!text.contains("plain.txt"), "{text}");
        // Count header: full template whenever a filter is present — literal
        // "matches" with no pluralization.
        assert!(
            text.contains(
                "1 matches (of 2 matching before filtering, 1 after filter) — most recent first"
            ),
            "{text}"
        );
    }

    /// Given a filter containing regex trigger characters, When the results
    /// are rendered, Then the filter is applied as a case-insensitive regex
    /// (test-x.ts does not match `test-\d+`).
    #[tokio::test]
    async fn should_treat_filter_as_regex_when_it_contains_trigger_characters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test-1.ts"), "x").unwrap();
        std::fs::write(tmp.path().join("test-2.ts"), "x").unwrap();
        std::fs::write(tmp.path().join("test-x.ts"), "x").unwrap();

        let result = run_glob(
            &anchored_tool(tmp.path()),
            serde_json::json!({ "pattern": "*", "filter": "test-\\d+" }),
        )
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        let text = output_text(&result);
        assert!(text.contains("test-1.ts"), "{text}");
        assert!(text.contains("test-2.ts"), "{text}");
        assert!(!text.contains("test-x.ts"), "{text}");
        assert!(
            text.contains(
                "2 matches (of 3 matching before filtering, 2 after filter) — most recent first"
            ),
            "{text}"
        );
    }

    /// Given a filter with trigger characters that does not compile as a
    /// regex, When the results are rendered, Then the filter falls back to a
    /// case-insensitive substring match against the literal text.
    #[tokio::test]
    async fn should_fall_back_to_substring_when_filter_regex_fails_to_compile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("has-a((paren.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("other.txt"), "x").unwrap();

        let result = run_glob(
            &anchored_tool(tmp.path()),
            serde_json::json!({ "pattern": "*", "filter": "a((" }),
        )
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        let text = output_text(&result);
        assert!(text.contains("has-a((paren.txt"), "{text}");
        assert!(!text.contains("other.txt"), "{text}");
        assert!(
            text.contains(
                "1 matches (of 2 matching before filtering, 1 after filter) — most recent first"
            ),
            "{text}"
        );
    }

    /// Given more filter hits than `limit`, When the results are rendered,
    /// Then truncation happens after filtering: three skill files are
    /// returned (not zero — a pre-filter truncation could keep only plain
    /// files that the filter then drops) with the truncation tail line.
    #[tokio::test]
    async fn should_truncate_after_filter_when_limit_is_smaller_than_filter_results() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("skill-{i}.txt")), "x").unwrap();
            std::fs::write(tmp.path().join(format!("plain-{i}.txt")), "x").unwrap();
        }

        let result = run_glob(
            &anchored_tool(tmp.path()),
            serde_json::json!({ "pattern": "*", "filter": "skill", "limit": 3 }),
        )
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        assert!(result.truncated, "tool-level truncation must be reported");
        let text = output_text(&result);
        let skill_lines = text.lines().filter(|l| l.contains("skill-")).count();
        assert_eq!(skill_lines, 3, "exactly three filtered results: {text}");
        assert!(
            !text.contains("plain-"),
            "truncation must happen after filtering: {text}"
        );
        assert!(
            text.contains(
                "3 matches (of 10 matching before filtering, 5 after filter) — most recent first"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[Results truncated at 3 of 5 — refine pattern or filter, or pass limit]"
            ),
            "{text}"
        );
    }

    /// Given a filter prefixed with `!`, When the results are rendered, Then
    /// matching paths are excluded instead of kept.
    #[tokio::test]
    async fn should_exclude_matching_paths_when_filter_is_prefixed_with_an_exclamation_mark() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("drop-test.rs"), "x").unwrap();

        let result = run_glob(
            &anchored_tool(tmp.path()),
            serde_json::json!({ "pattern": "*", "filter": "!test" }),
        )
        .await;
        assert!(!result.is_error, "glob failed: {}", output_text(&result));
        let text = output_text(&result);
        assert!(text.contains("keep.rs"), "{text}");
        assert!(!text.contains("drop-test.rs"), "{text}");
        assert!(
            text.contains(
                "1 matches (of 2 matching before filtering, 1 after filter) — most recent first"
            ),
            "{text}"
        );
    }

    /// Given the Glob tool's parameter schema, When the new parameters are
    /// inspected, Then `filter` is a string and `limit` is an integer with a
    /// default of 200.
    #[test]
    fn should_expose_filter_and_limit_in_schema_with_default_limit_of_two_hundred() {
        let tool = GlobTool::new(local_glob(), None, local_fs());
        let props = tool.parameters()["properties"].as_object().unwrap();
        assert_eq!(props["filter"]["type"], "string");
        assert_eq!(props["limit"]["type"], "integer");
        assert_eq!(props["limit"]["default"], 200);
    }
}
