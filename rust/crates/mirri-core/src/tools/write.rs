//! `WriteTool` — write (or overwrite) a file's contents.
//!
//! Uses the [`FileSystem`] abstraction, which creates parent directories as
//! needed. The file is written as UTF-8 text.

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

/// How [`WriteTool`] should write to the target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Replace the file's entire contents (the default).
    #[default]
    Overwrite,
    /// Append to the end of the file, creating it if it does not exist.
    Append,
}

impl WriteMode {
    /// Parse a mode string from tool input.
    ///
    /// Returns `Ok(Overwrite)` for `"overwrite"` or `None`, `Ok(Append)` for
    /// `"append"`, and an error string for any other value.
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("overwrite") => Ok(WriteMode::Overwrite),
            Some("append") => Ok(WriteMode::Append),
            Some(other) => Err(format!(
                "Write received invalid mode '{other}': expected 'overwrite' or 'append'"
            )),
        }
    }
}

/// `Write` — create or overwrite a text file.
///
/// When constructed with a workspace sandbox ([`WriteTool::new`]), every
/// path is validated against the workspace root or any registered
/// additional dir, and paths escaping all of them (traversal, absolute
/// outside paths, symlinks) are refused before any I/O (issue 15D).
pub struct WriteTool {
    /// Workspace sandbox this tool may write inside; `None` disables
    /// validation.
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider every write goes through (zero-IO T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl WriteTool {
    /// Create a write tool confined to the `workspace_paths` sandbox, or
    /// `None` to disable workspace path validation. `fs` is the injected
    /// filesystem provider (the engine default is the local one).
    pub fn new(workspace_paths: Option<Arc<WorkspacePaths>>, fs: Arc<dyn DynFileSystem>) -> Self {
        Self {
            workspace_paths,
            fs,
        }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write text content to a file, creating parent directories as needed. \
         By default overwrites the file if it already exists; set mode to 'append' \
         to add to the end instead. Returns the number of bytes written."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to write. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full text content to write."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "default": "overwrite",
                        "description": "Write mode: 'overwrite' replaces the file contents (default), 'append' adds to the end of the file, creating it if it does not exist."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let (Some(path), Some(content)) = (
            input.get("path").and_then(serde_json::Value::as_str),
            input.get("content").and_then(serde_json::Value::as_str),
        ) else {
            return Ok(tool_execution_error(
                "Write requires string 'path' and 'content' arguments",
            ));
        };
        let path = path.to_string();
        let content = content.to_string();

        let mode = match WriteMode::parse(input.get("mode").and_then(serde_json::Value::as_str)) {
            Ok(mode) => mode,
            Err(message) => return Ok(tool_execution_error(message)),
        };

        // Classify against the workspace sandbox before any I/O (issue
        // 15D, plan 2026-10-10-sandbox-path-permission D1) — including
        // existing targets and not-yet-created nested paths. Inside and
        // Outside both build the plan over the canonical path (Outside
        // enters the permission chain instead of being refused here);
        // only escape shapes are refused. The symlink probes inside
        // classification go through the injected filesystem provider.
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
                    "Write", &path, &reason,
                )));
            }
        };
        let path = file_path.to_string_lossy().into_owned();
        let fs = Arc::clone(&self.fs);

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                kind: AccessKind::File,
                path: Some(path.clone()),
                operation: AccessOperation::Write,
            }],
            display: ToolInputDisplay::FileIo {
                path: path.clone(),
                // Lowercase: the protocol `file_io.operation` enum only
                // accepts `read|write|edit|glob|grep`.
                operation: "write".to_string(),
            },
            approval_rule: "Write".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| Box::pin(write_file(fs, path, content, mode))),
        })
    }
}

async fn write_file(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    content: String,
    mode: WriteMode,
) -> ToolResult {
    // Serialize same-path mutations process-wide so a Write can never
    // interleave with a concurrent Edit's read/write pair (or another
    // Write). Blind overwrite is Write's contract, so there is no
    // pre-write content verification here — only mutual exclusion.
    let file_path = Path::new(&path);
    let _guard = match crate::tools::file_mutation_guard::FileMutationGuard::acquire(file_path)
        .await
    {
        Ok(guard) => guard,
        Err(message) => return ToolResult::error(format!("Write failed for '{path}': {message}")),
    };

    match mode {
        WriteMode::Overwrite => match fs.write_text(file_path, &content).await {
            Ok(()) => ToolResult::success(format!("Wrote {} bytes to {}", content.len(), path)),
            Err(error) => ToolResult::error(format!("Write failed for '{path}': {error}")),
        },
        WriteMode::Append => {
            // Create parent directories as needed, mirroring the overwrite
            // path.
            if let Some(parent) = file_path.parent()
                && !parent.as_os_str().is_empty()
                && let Err(error) = fs.mkdir(parent, true).await
            {
                return ToolResult::error(format!(
                    "Append failed for '{path}': failed to create parent dirs: {error}"
                ));
            }

            // Append to the file, creating it if it does not exist. The
            // provider flushes before reporting success so a crash right
            // after cannot lose the appended data.
            match fs.append(file_path, &content).await {
                Ok(()) => {
                    ToolResult::success(format!("Appended {} bytes to {}", content.len(), path))
                }
                Err(error) => ToolResult::error(format!("Append failed for '{path}': {error}")),
            }
        }
    }
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
    async fn should_create_file_with_content_when_writing_a_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hello.txt");
        let result = write_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "hello\n".to_string(),
            WriteMode::Overwrite,
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("Wrote 6 bytes"), "message was: {text}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hello\n",
            "file content must match"
        );
    }

    #[tokio::test]
    async fn should_create_parent_directories_when_writing_to_a_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a").join("b").join("deep.txt");
        let result = write_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "deep\n".to_string(),
            WriteMode::Overwrite,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep\n");
    }

    #[tokio::test]
    async fn should_overwrite_content_when_writing_to_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("overwrite.txt");
        std::fs::write(&path, "old content\n").unwrap();
        let result = write_file(
            local_fs(),
            path.to_string_lossy().into_owned(),
            "new content\n".to_string(),
            WriteMode::Overwrite,
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content\n");
    }

    #[tokio::test]
    async fn should_write_an_outside_absolute_file_when_writing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");

        let tool = WriteTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "content": "pwned"
            }))
            .expect("resolve");

        // A clean absolute path outside the sandbox flows into the plan; the
        // permission chain (not the tool) decides whether it may run.
        let ToolExecution::Runnable { plan, execute } = execution else {
            panic!("expected a runnable execution for an outside absolute path");
        };
        let access = plan.accesses.first().expect("one file access");
        // The target does not exist yet, so the canonical expectation is the
        // canonicalized parent dir (the deepest existing ancestor) plus the
        // missing tail — exactly what the sandbox classification returns.
        let expected = outside.path().canonicalize().unwrap().join("secret.txt");
        assert_eq!(
            access.path.as_deref(),
            Some(expected.to_str().unwrap()),
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
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "pwned");
    }

    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_writing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = WriteTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": "../escape.txt",
                "content": "pwned"
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

    #[cfg(unix)]
    #[tokio::test]
    async fn should_refuse_dangling_symlink_escape_when_writing_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dangling_target = outside.path().join("authorized_keys");
        std::os::unix::fs::symlink(&dangling_target, tmp.path().join("leak.txt")).unwrap();

        let tool = WriteTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": tmp.path().join("leak.txt").to_string_lossy(),
                "content": "pwned"
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
            !outside.path().join("authorized_keys").exists(),
            "the dangling symlink target must never be created"
        );
    }

    #[tokio::test]
    async fn should_write_file_inside_workspace_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("new.txt");

        let tool = WriteTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "hello\n"
            }))
            .expect("resolve");

        // The write target does not exist yet — validation must allow it.
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected a runnable execution for a missing in-workspace target");
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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn should_resolve_relative_path_against_workspace_root_when_writing() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = WriteTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": "rel.txt",
                "content": "rel\n"
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
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("rel.txt")).unwrap(),
            "rel\n"
        );
    }

    #[tokio::test]
    async fn should_overwrite_by_default_when_mode_not_specified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("default.txt");
        std::fs::write(&path, "original\n").unwrap();

        let tool = WriteTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "replaced\n"
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
        assert!(
            text.contains("Wrote 9 bytes"),
            "overwrite message should say 'Wrote', was: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "replaced\n",
            "file must contain only the new content"
        );
    }

    #[tokio::test]
    async fn should_append_to_existing_when_mode_is_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("append.txt");
        std::fs::write(&path, "line one\n").unwrap();

        let tool = WriteTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "line two\n",
                "mode": "append"
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
        assert!(
            text.contains("Appended 9 bytes"),
            "append message should say 'Appended', was: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line one\nline two\n",
            "original content must be preserved and new content appended"
        );
    }

    #[tokio::test]
    async fn should_create_new_file_when_mode_is_append_and_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new_append.txt");
        assert!(!path.exists(), "file must not exist before the test");

        let tool = WriteTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "created via append\n",
                "mode": "append"
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
        assert!(
            text.contains("Appended"),
            "append message should say 'Appended', was: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "created via append\n",
            "file must be created with the appended content"
        );
    }
    // -------------------------------------------------------------------------
    // T-13a-1: injected filesystem provider (zero-IO injection)
    // -------------------------------------------------------------------------

    /// In-memory [`mirri_os::DynFileSystem`] double recording the operations
    /// Write performs. Only `mkdir` and `append` are implemented — everything
    /// else keeps the trait's default "not implemented" error.
    #[derive(Default)]
    struct RecordingFs {
        appends: Mutex<Vec<(String, String)>>,
        mkdirs: Mutex<Vec<String>>,
    }

    impl mirri_os::DynFileSystem for RecordingFs {
        fn mkdir<'a>(
            &'a self,
            path: &'a Path,
            _recursive: bool,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.mkdirs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(path.display().to_string());
                Ok(())
            })
        }

        fn append<'a>(
            &'a self,
            path: &'a Path,
            contents: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.appends
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((path.display().to_string(), contents.to_string()));
                Ok(())
            })
        }
    }

    /// Given the Write tool with an injected fake filesystem, When an
    /// append-mode execution runs, Then the append and the parent-directory
    /// creation are recorded on the fake and no disk file is created
    /// (T-13a-1).
    #[tokio::test]
    async fn should_record_append_on_the_fake_filesystem_when_mode_is_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("log.txt");
        let fake = Arc::new(RecordingFs::default());
        let tool = WriteTool::new(None, Arc::clone(&fake) as Arc<dyn DynFileSystem>);

        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "line\n",
                "mode": "append"
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
        assert!(
            text.contains("Appended 5 bytes"),
            "append message should say 'Appended', was: {text}"
        );
        let appends = fake.appends.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            appends.len(),
            1,
            "the append must go through the injected provider"
        );
        assert_eq!(
            appends[0],
            (path.display().to_string(), "line\n".to_string())
        );
        let mkdirs = fake.mkdirs.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            mkdirs.len(),
            1,
            "parent dirs must be created through the injected provider"
        );
        assert_eq!(mkdirs[0], path.parent().unwrap().display().to_string());
        assert!(
            !path.exists(),
            "no disk file must be created by the fake-backed run"
        );
    }

    // -------------------------------------------------------------------------
    // Concurrent-mutation safety (file mutation guard)
    // -------------------------------------------------------------------------

    /// A fake filesystem that yields once inside every read, write, and
    /// append, forcing concurrently-joined mutations to interleave on the
    /// single-threaded test runtime.
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

        fn append<'a>(
            &'a self,
            _path: &'a Path,
            contents: &'a str,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<()>> {
            Box::pin(async {
                tokio::task::yield_now().await;
                self.content
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(contents);
                Ok(())
            })
        }
    }

    /// Given an append lands while an edit of the same file is in flight,
    /// When both complete, Then both changes survive — the append must not
    /// be silently clobbered by the edit's full-file rewrite, and the edit
    /// must re-read the post-append content.
    #[tokio::test]
    async fn should_not_lose_an_append_that_lands_during_a_concurrent_edit() {
        let fs = Arc::new(YieldingFs::default());
        *fs.content.lock().unwrap_or_else(|e| e.into_inner()) = "a\n".to_string();
        let edit_fs: Arc<dyn DynFileSystem> = Arc::clone(&fs) as Arc<dyn DynFileSystem>;
        let write_fs: Arc<dyn DynFileSystem> = Arc::clone(&fs) as Arc<dyn DynFileSystem>;
        let (edit_result, write_result) = tokio::join!(
            crate::tools::edit::edit_file_for_tests(
                edit_fs,
                "shared.log".to_string(),
                "a".to_string(),
                "x".to_string(),
                false
            ),
            write_file(
                write_fs,
                "shared.log".to_string(),
                "tail\n".to_string(),
                WriteMode::Append,
            ),
        );
        assert!(!edit_result.is_error, "edit failed: {}", {
            match &edit_result.output {
                crate::tool::ToolOutput::Text(t) => t.clone(),
                crate::tool::ToolOutput::Parts(_) => String::new(),
            }
        });
        assert!(!write_result.is_error, "append failed");
        let final_content = fs.content.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            final_content.contains('x') && final_content.contains("tail\n"),
            "both the edit and the append must survive, got: {final_content:?}"
        );
    }

    /// Given the Write tool's parameter schema (Task 5, plan 2026-10-10-
    /// sandbox-path-permission D5), When the `path` description is
    /// inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = WriteTool::new(None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }
}
