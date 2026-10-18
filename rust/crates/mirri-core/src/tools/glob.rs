//! `GlobTool` — discover files by glob pattern.
//!
//! Uses the [`GlobMatch`] abstraction (a ripgrep-style walker that respects
//! `.gitignore`), returning matches relative to the search root, sorted by
//! modification time (newest first), capped at 100 matches.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::{DynFileSystem, DynGlobMatch, GlobParams};

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::{PathClassification, WorkspacePaths, tool_execution_error};

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
        "Find files by glob pattern (e.g. '**/*.rs'), sorted by modification \
         time, newest first. Results are paths relative to the search root \
         ('path' defaults to the working directory) and are capped at 100. \
         Respects .gitignore; set 'include_ignored' to also match ignored \
         files. Sensitive files (secrets, keys, credentials) are always \
         filtered out. Directories are never matched."
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
                    "include_dirs": {
                        "type": "boolean",
                        "description": "Deprecated and ignored. Directories are never matched."
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
        // Accepted for backward compatibility; deprecated and ignored.
        let _include_dirs = input
            .get("include_dirs")
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
            approval_rule: "Glob".to_string(),
            stop_batch_after_this: false,
        };

        let glob = Arc::clone(&self.glob);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(glob_files(glob, pattern, path, include_ignored))
            }),
        })
    }
}

/// Maximum number of matches returned by Glob, matching the TS engine.
const MAX_GLOB_MATCHES: usize = 100;

async fn glob_files(
    glob: Arc<dyn DynGlobMatch>,
    pattern: String,
    path: Option<PathBuf>,
    include_ignored: bool,
) -> ToolResult {
    let params = GlobParams {
        pattern,
        path: path.unwrap_or_else(default_search_root),
        include_ignored,
    };
    match glob.glob(&params).await {
        Ok(result) => {
            let truncated = result.truncated;
            // Sensitive files (secrets, keys, credentials) never surface in
            // Glob results.
            let matches: Vec<String> = result
                .matches
                .iter()
                .filter(|m| !crate::tools::path_security::is_sensitive_file(Path::new(m)))
                .cloned()
                .collect();
            let mut lines: Vec<String> = Vec::new();

            if matches.is_empty() {
                lines.push("No matches found".to_string());
            } else {
                if truncated {
                    lines.push(format!(
                        "[Truncated at {MAX_GLOB_MATCHES} matches — use a more specific pattern]"
                    ));
                    lines.push(format!(
                        "Only the first {MAX_GLOB_MATCHES} matches are returned."
                    ));
                }
                lines.extend(matches.iter().cloned());
            }

            let output = lines.join("\n");
            let note = truncated.then(|| "Glob results truncated at 100 matches".to_string());
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

    #[tokio::test]
    async fn should_return_all_matches_when_globbing_with_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "text\n").unwrap();

        let result = glob_files(
            local_glob(),
            "*.rs".to_string(),
            Some(tmp.path().to_path_buf()),
            false,
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        let matches: Vec<&str> = text.lines().collect();
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&"a.rs"));
        assert!(matches.contains(&"b.rs"));
    }

    #[tokio::test]
    async fn should_return_recursive_matches_when_globbing_with_a_nested_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("src/deep/lib.rs"), "pub fn f() {}\n").unwrap();

        let result = glob_files(
            local_glob(),
            "**/*.rs".to_string(),
            Some(tmp.path().to_path_buf()),
            false,
        )
        .await;
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
        let result = glob_files(
            local_glob(),
            "*.rs".to_string(),
            Some(tmp.path().to_path_buf()),
            false,
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text, "No matches found");
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn should_return_error_result_when_globbing_with_an_invalid_pattern() {
        let result = glob_files(
            local_glob(),
            "[".to_string(),
            Some(PathBuf::from(".")),
            false,
        )
        .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn should_return_error_result_when_globbing_with_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does_not_exist");
        let result = glob_files(local_glob(), "*.rs".to_string(), Some(missing), false).await;
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
        let result = glob_files(
            local_glob(),
            "*.txt".to_string(),
            Some(tmp.path().to_path_buf()),
            false,
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("normal.txt"), "{text}");
        assert!(
            !text.contains("ignored.txt"),
            "ignored.txt should be excluded, got {text}"
        );

        // With include_ignored: both should match.
        let result = glob_files(
            local_glob(),
            "*.txt".to_string(),
            Some(tmp.path().to_path_buf()),
            true,
        )
        .await;
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
        let result = glob_files(
            local_glob(),
            "**".to_string(),
            Some(tmp.path().to_path_buf()),
            true,
        )
        .await;
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
            vec!["session-workdir-a1.txt"],
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
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        })
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text, "from-fake.txt");
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
            vec!["extra.rs"],
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
}
