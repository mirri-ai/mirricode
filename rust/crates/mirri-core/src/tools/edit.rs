//! `EditTool` — targeted string replacement inside a file.
//!
//! Reads the file, replaces `old_string` with `new_string` (once by default,
//! every occurrence with `replace_all: true`), and writes it back. The match
//! is a literal byte-for-byte string comparison — `old_string` must match the
//! file content exactly, including line endings. Refuses to guess when the
//! string is absent or ambiguous (multiple occurrences without
//! `replace_all`).

use std::path::Path;
use std::sync::Arc;

use mirri_os::DynFileSystem;

use crate::error::EngineResult;
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolResult,
};
use crate::tools::tool_execution_error;
use crate::tools::{PathClassification, WorkspacePaths};

/// `Edit` — replace an exact string within a file.
///
/// When constructed with a workspace sandbox ([`EditTool::new`]), every path
/// is validated against the workspace root or any registered additional
/// dir, and paths escaping all of them (traversal, absolute outside paths,
/// symlinks) are refused before any I/O (issue 15D).
pub struct EditTool {
    /// Workspace sandbox this tool may edit inside; `None` disables
    /// validation.
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider every edit reads/writes through (zero-IO
    /// T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl EditTool {
    /// Create an edit tool confined to the `workspace_paths` sandbox, or
    /// `None` to disable workspace path validation. `fs` is the injected
    /// filesystem provider (the engine default is the local one).
    pub fn new(workspace_paths: Option<Arc<WorkspacePaths>>, fs: Arc<dyn DynFileSystem>) -> Self {
        Self {
            workspace_paths,
            fs,
        }
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace an exact substring in a file with new content. \
         'old_string' must match the file bytes exactly, including line endings \
         (CRLF files are matched with either \r\n or \n and keep CRLF after \
         editing). By default exactly one occurrence must exist; pass \
         'replace_all' to replace every occurrence. When 'old_string' equals \
         'new_string' the edit is a no-op that succeeds without writing."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to edit. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to find (line endings included)."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "When true, replace every occurrence. Defaults to false."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let (Some(path), Some(old_string), Some(new_string)) = (
            input.get("path").and_then(serde_json::Value::as_str),
            input.get("old_string").and_then(serde_json::Value::as_str),
            input.get("new_string").and_then(serde_json::Value::as_str),
        ) else {
            return Ok(tool_execution_error(
                "Edit requires string 'path', 'old_string', and 'new_string' arguments",
            ));
        };
        let path = path.to_string();
        let old_string = old_string.to_string();
        let new_string = new_string.to_string();

        // Classify against the workspace sandbox before any I/O (issue 15D,
        // plan 2026-10-10-sandbox-path-permission D1). Inside and Outside
        // both build the plan over the canonical path (Outside enters the
        // permission chain instead of being refused here); only escape
        // shapes are refused.
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
                    "Edit", &path, &reason,
                )));
            }
        };
        let path = file_path.to_string_lossy().into_owned();
        let replace_all = input
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                kind: AccessKind::File,
                path: Some(path.clone()),
                operation: AccessOperation::ReadWrite,
            }],
            display: ToolInputDisplay::Diff {
                path: path.clone(),
                before: old_string.clone(),
                after: new_string.clone(),
            },
            approval_rule: "Edit".to_string(),
            stop_batch_after_this: false,
        };

        let fs = Arc::clone(&self.fs);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(edit_file(fs, path, old_string, new_string, replace_all))
            }),
        })
    }
}

async fn edit_file(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolResult {
    // No-op edits succeed immediately without touching the file: the model
    // asked for no change, so there is nothing to verify or write.
    if old_string == new_string {
        return ToolResult::success(format!(
            "Edit skipped: 'old_string' and 'new_string' are identical; no \
             change was made to '{path}'"
        ));
    }

    let file_path = Path::new(&path);

    // Serialize same-path mutations process-wide (parallel agents / swarms
    // share this lock table) before reading, so two edits can never
    // interleave their read/write pairs.
    let _guard = match crate::tools::file_mutation_guard::FileMutationGuard::acquire(file_path)
        .await
    {
        Ok(guard) => guard,
        Err(message) => return ToolResult::error(format!("Edit failed for '{path}': {message}")),
    };

    let raw_text = match fs.read_text(file_path).await {
        Ok(text) => text,
        Err(error) => {
            return ToolResult::error(format!("Edit failed to read '{path}': {error}"));
        }
    };

    // CRLF awareness: when the file uses `\r\n` line endings, matching is done
    // on LF-normalized copies (so an `old_string` written with plain `\n`
    // matches the file, and vice versa), and the result is converted back to
    // CRLF before writing so the file's line-ending style is preserved.
    let uses_crlf = raw_text.contains("\r\n");
    let (text, old_string, new_string) = if uses_crlf {
        (
            raw_text.replace("\r\n", "\n"),
            old_string.replace("\r\n", "\n"),
            new_string.replace("\r\n", "\n"),
        )
    } else {
        (raw_text.clone(), old_string, new_string)
    };

    let occurrence_count = text.matches(old_string.as_str()).count();
    if occurrence_count == 0 {
        return ToolResult::error(format!(
            "Edit failed: 'old_string' was not found in '{path}'"
        ));
    }
    if occurrence_count > 1 && !replace_all {
        return ToolResult::error(format!(
            "Edit failed: 'old_string' occurs {occurrence_count} times in '{path}'; \
             pass 'replace_all': true to replace every occurrence"
        ));
    }

    let edited_lf = if replace_all {
        text.replace(old_string.as_str(), new_string.as_str())
    } else {
        text.replacen(old_string.as_str(), new_string.as_str(), 1)
    };
    let replaced_count = if replace_all { occurrence_count } else { 1 };

    let edited = if uses_crlf {
        // Every newline is currently LF (the text was normalized) — restore
        // the file's CRLF ending style.
        edited_lf.replace('\n', "\r\n")
    } else {
        edited_lf
    };

    // The file must still be byte-for-byte what we read — otherwise an
    // external writer (editor, git, another process) would be silently
    // clobbered by this edit's full-file rewrite.
    if let Err(message) =
        crate::tools::file_mutation_guard::verify_unchanged(fs.as_ref(), file_path, &raw_text).await
    {
        return ToolResult::error(format!("Edit failed for '{path}': {message}"));
    }

    match fs.write_text(file_path, &edited).await {
        Ok(()) => ToolResult::success(format!(
            "Edited '{path}': replaced {replaced_count} occurrence(s)"
        )),
        Err(error) => ToolResult::error(format!("Edit failed to write '{path}': {error}")),
    }
}

/// Test-only entry so `write.rs` tests can drive a real edit against a fake
/// filesystem without going through the full tool plumbing.
#[cfg(test)]
pub(crate) async fn edit_file_for_tests(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> ToolResult {
    edit_file(fs, path, old_string, new_string, replace_all).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;
    use std::sync::Mutex;

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

    #[tokio::test]
    async fn should_replace_once_when_editing_a_unique_string() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("edit.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "world".to_string(),
            "rust".to_string(),
            false,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust\n");
        assert!(output_text(&result).contains("replaced 1 occurrence"));
    }

    #[tokio::test]
    async fn should_refuse_edit_when_string_has_multiple_occurrences_without_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dup.txt");
        std::fs::write(&path, "a a a\n").unwrap();
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "a".to_string(),
            "b".to_string(),
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("occurs 3 times"));
        // The file must be untouched after a refused edit.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a a a\n");
    }

    #[tokio::test]
    async fn should_replace_every_occurrence_when_editing_with_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dup_all.txt");
        std::fs::write(&path, "a a a\n").unwrap();
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "a".to_string(),
            "b".to_string(),
            true,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b b b\n");
        assert!(output_text(&result).contains("replaced 3 occurrence"));
    }

    #[tokio::test]
    async fn should_return_error_result_when_editing_an_absent_string() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent.txt");
        std::fs::write(&path, "nothing here\n").unwrap();
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "missing".to_string(),
            "ghost".to_string(),
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("was not found"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nothing here\n");
    }

    #[tokio::test]
    async fn should_return_error_result_when_editing_a_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no_such_file.txt");
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "x".to_string(),
            "y".to_string(),
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("failed to read"));
    }

    #[tokio::test]
    async fn should_edit_an_outside_absolute_file_when_editing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "old").unwrap();

        let tool = EditTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "old_string": "old",
                "new_string": "pwned"
            }))
            .expect("resolve");

        // A clean absolute path outside the sandbox flows into the plan; the
        // permission chain (not the tool) decides whether it may run.
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
        })
        .await;
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "pwned");
    }

    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_editing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = EditTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": "../escape.txt",
                "old_string": "old",
                "new_string": "pwned"
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
    }

    #[tokio::test]
    async fn should_edit_file_inside_workspace_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("edit.txt");
        std::fs::write(&path, "hello world\n").unwrap();

        let tool = EditTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "old_string": "world",
                "new_string": "rust"
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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust\n");
    }

    #[tokio::test]
    async fn should_resolve_relative_path_against_workspace_root_when_editing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("rel.txt"), "x y\n").unwrap();

        let tool = EditTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": "rel.txt",
                "old_string": "x",
                "new_string": "z"
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
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("rel.txt")).unwrap(),
            "z y\n"
        );
    }

    #[tokio::test]
    async fn should_preserve_crlf_line_endings_when_editing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crlf.txt");
        std::fs::write(&path, "line1\r\nline2\r\nline3\r\n").unwrap();

        // The model passes an LF-terminated old_string; the CRLF file must
        // still match.
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "line2".to_string(),
            "CHANGED".to_string(),
            false,
        )
        .await;
        assert!(!result.is_error, "{:?}", output_text(&result));
        let content = std::fs::read(&path).unwrap();
        assert_eq!(
            content, b"line1\r\nCHANGED\r\nline3\r\n",
            "CRLF line endings must be preserved after editing"
        );
    }

    #[tokio::test]
    async fn should_preserve_crlf_when_old_string_uses_crlf_too() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crlf2.txt");
        std::fs::write(&path, "a\r\nb\r\nc\r\n").unwrap();

        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "a\r\nb".to_string(),
            "a\r\nX".to_string(),
            false,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"a\r\nX\r\nc\r\n",
            "CRLF-bearing old/new strings must work on CRLF files"
        );
    }

    #[tokio::test]
    async fn should_return_early_when_old_string_equals_new_string() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("noop.txt");
        std::fs::write(&path, "hello world\n").unwrap();

        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "world".to_string(),
            "world".to_string(),
            false,
        )
        .await;
        assert!(!result.is_error, "identical strings are not an error");
        let text = output_text(&result);
        assert!(
            text.contains("no change was made"),
            "message should explain the no-op: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello world\n",
            "file must not be rewritten"
        );
    }

    #[tokio::test]
    async fn should_succeed_without_reading_when_old_string_equals_new_string_and_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("never_created.txt");

        // old == new short-circuits before any read, so a missing file is fine.
        let result = edit_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "same".to_string(),
            "same".to_string(),
            true,
        )
        .await;
        assert!(!result.is_error);
    }

    // -------------------------------------------------------------------------
    // Concurrent-edit safety (file mutation guard)
    // -------------------------------------------------------------------------

    /// A fake filesystem that returns different content on every read, as
    /// if an external writer modified the file between the edit's read and
    /// its write.
    #[derive(Default)]
    struct ExternallyChangedFs {
        writes: Mutex<Vec<String>>,
        // First read: the file as the edit sees it. Second read (the
        // pre-write verification): an external writer already rewrote it.
        reads: std::sync::atomic::AtomicUsize,
    }

    impl mirri_os::DynFileSystem for ExternallyChangedFs {
        fn read_text<'a>(
            &'a self,
            _path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<String>> {
            Box::pin(async {
                let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok("hello world\n".to_string())
                } else {
                    Ok("EXTERNAL WRITE\n".to_string())
                }
            })
        }

        fn write_text<'a>(
            &'a self,
            _path: &'a Path,
            content: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                self.writes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(content.to_string());
                Ok(())
            })
        }
    }

    /// Given the file is modified by an external writer between the edit's
    /// read and its write, When the edit completes, Then it fails with a
    /// conflict error and writes nothing.
    #[tokio::test]
    async fn should_fail_without_writing_when_the_file_changes_externally_during_the_edit() {
        let fake = Arc::new(ExternallyChangedFs::default());
        let result = edit_file(
            Arc::clone(&fake) as Arc<dyn DynFileSystem>,
            "victim.txt".to_string(),
            "world".to_string(),
            "rust".to_string(),
            false,
        )
        .await;
        assert!(result.is_error, "an externally changed file must conflict");
        let text = output_text(&result);
        assert!(
            text.contains("modified by another writer"),
            "error must explain the conflict, was: {text}"
        );
        let writes = fake.writes.lock().unwrap_or_else(|e| e.into_inner());
        assert!(writes.is_empty(), "a conflicted edit must not write");
    }

    /// A fake filesystem that yields once inside every read and write,
    /// forcing concurrently-joined read-modify-write cycles to interleave on
    /// the single-threaded test runtime.
    #[derive(Default)]
    struct YieldingFs {
        content: Mutex<String>,
    }

    impl mirri_os::DynFileSystem for YieldingFs {
        fn read_text<'a>(
            &'a self,
            _path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<String>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(self
                    .content
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone())
            })
        }

        fn write_text<'a>(
            &'a self,
            _path: &'a Path,
            content: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                *self.content.lock().unwrap_or_else(|e| e.into_inner()) = content.to_string();
                Ok(())
            })
        }
    }

    /// Given two concurrent edits of the same file touching different
    /// regions, When both complete, Then both replacements survive — the
    /// second editor must re-read the post-first-edit content instead of
    /// silently clobbering the first edit's write.
    #[tokio::test]
    async fn should_preserve_both_edits_when_two_edits_run_concurrently_on_the_same_file() {
        let fs = Arc::new(YieldingFs::default());
        *fs.content.lock().unwrap_or_else(|e| e.into_inner()) = "a b c\n".to_string();
        let fs1: Arc<dyn DynFileSystem> = Arc::clone(&fs) as Arc<dyn DynFileSystem>;
        let fs2: Arc<dyn DynFileSystem> = Arc::clone(&fs) as Arc<dyn DynFileSystem>;
        let (r1, r2) = tokio::join!(
            edit_file(
                fs1,
                "shared.txt".to_string(),
                "a".to_string(),
                "x".to_string(),
                false
            ),
            edit_file(
                fs2,
                "shared.txt".to_string(),
                "c".to_string(),
                "z".to_string(),
                false
            ),
        );
        assert!(!r1.is_error, "first edit failed: {}", output_text(&r1));
        assert!(!r2.is_error, "second edit failed: {}", output_text(&r2));
        assert_eq!(
            *fs.content.lock().unwrap_or_else(|e| e.into_inner()),
            "x b z\n",
            "both edits must survive a concurrent same-file edit"
        );
    }

    /// Given the Edit tool's parameter schema (Task 5, plan 2026-10-10-
    /// sandbox-path-permission D5), When the `path` description is
    /// inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = EditTool::new(None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }
}
