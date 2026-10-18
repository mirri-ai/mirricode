//! Filesystem persistence for background tasks, mirroring the TS
//! `AgentTaskPersistence` design at session scope.
//!
//! Layout under the session directory:
//!
//! ```text
//! <tasks_dir>/
//!   <task_id>.json          task metadata, atomically rewritten on every
//!                           status change (temp + fsync + rename, mode 0o600)
//!   <task_id>/output.log    append-only raw output stream
//! ```
//!
//! Every path segment derived from a task id is validated against a
//! restrictive character set (lowercase/uppercase alphanumerics and hyphens
//! only, at most 128 characters) before use as a path component, so a
//! hostile task id can never escape the tasks directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_os::SyncFs;

use crate::error::{EngineError, EngineResult};
use crate::task::AgentTask;

/// Persists background tasks to the filesystem.
///
/// All methods are synchronous; the struct is cheaply shareable behind an
/// `Arc` and performs no internal synchronization itself (callers — the
/// [`TaskManager`](crate::task::TaskManager) — serialize access). File IO
/// goes through the injected [`SyncFs`] provider (zero-IO T13b; the engine
/// default is `LocalSyncFs`).
pub struct TaskPersistence {
    tasks_dir: PathBuf,
    fs: Arc<dyn SyncFs>,
}

impl TaskPersistence {
    /// Create a persistence root anchored at `tasks_dir`
    /// (conventionally `<session_dir>/tasks`).
    pub fn new(tasks_dir: PathBuf, fs: Arc<dyn SyncFs>) -> Self {
        Self { tasks_dir, fs }
    }

    /// The root tasks directory.
    pub fn tasks_dir(&self) -> &Path {
        &self.tasks_dir
    }

    /// Validate a task ID before it is used as a path segment.
    ///
    /// Guards against path traversal (`../`, absolute paths) and exotic
    /// characters: only ASCII alphanumerics and hyphens are accepted, with
    /// an upper length bound. Mirrors the intent of the TS `VALID_TASK_ID`
    /// regex.
    fn validate_task_id(id: &str) -> EngineResult<()> {
        if id.is_empty() || id.len() > 128 {
            return Err(EngineError::RequestInvalid(
                "invalid task id: must be 1-128 characters".into(),
            ));
        }
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(EngineError::RequestInvalid(
                "invalid task id: only ASCII alphanumerics and hyphens allowed".into(),
            ));
        }
        Ok(())
    }

    /// Write (or update) a task's metadata JSON atomically.
    ///
    /// The tasks directory is created (private, mode `0o700` on Unix) via
    /// the injected [`SyncFs`]; the file itself is written to a sibling temp
    /// file, fsynced, and renamed into place by the shared persistence
    /// primitive (mode `0o600` on Unix).
    pub fn save_task(&self, task: &AgentTask) -> EngineResult<()> {
        Self::validate_task_id(&task.id)?;
        self.fs
            .mkdir_private(&self.tasks_dir)
            .map_err(io_to_engine)?;
        let path = self.tasks_dir.join(format!("{}.json", task.id));
        let json = serde_json::to_string_pretty(task)
            .map_err(|e| EngineError::RequestInvalid(format!("cannot serialize task: {e}")))?;
        write_atomic(&path, &json)
    }

    /// Append a chunk to the task's output log.
    ///
    /// The chunk is appended verbatim and the file is fsynced before
    /// returning, so an acknowledged append survives a crash ([`SyncFs::append`]
    /// carries the append + fsync semantics). On Unix the per-task directory
    /// is created with mode `0o700`.
    pub fn append_output(&self, task_id: &str, chunk: &str) -> EngineResult<()> {
        Self::validate_task_id(task_id)?;
        let dir = self.tasks_dir.join(task_id);
        self.fs.mkdir_private(&dir).map_err(io_to_engine)?;
        let path = dir.join("output.log");
        self.fs.append(&path, chunk).map_err(io_to_engine)?;
        Ok(())
    }

    /// Load all tasks from disk, sorted by `created_at`.
    ///
    /// Reads every `*.json` file directly inside `tasks_dir` (temp files and
    /// per-task output directories are ignored by extension). A file that
    /// exists but cannot be deserialized is skipped with a warning — one
    /// corrupt record must not hide the rest.
    pub fn load_tasks(&self) -> EngineResult<Vec<AgentTask>> {
        if !self.fs.exists(&self.tasks_dir) {
            return Ok(Vec::new());
        }
        let mut tasks = Vec::new();
        for entry in self.fs.iterdir(&self.tasks_dir).map_err(io_to_engine)? {
            let path = PathBuf::from(&entry.path);
            if path.extension().is_some_and(|ext| ext == "json") {
                let contents = match self.fs.read_text(&path) {
                    Ok(c) => c,
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "skipping unreadable task file"
                        );
                        continue;
                    }
                };
                match serde_json::from_str::<AgentTask>(&contents) {
                    Ok(task) => tasks.push(task),
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "skipping corrupt task file"
                        );
                    }
                }
            }
        }
        tasks.sort_by_key(|task| task.created_at);
        Ok(tasks)
    }

    /// Read output log bytes starting at `offset`, up to `max_bytes`.
    ///
    /// Returns an empty string when the task has no output log or the
    /// offset is at (or past) end of file. The offset read
    /// ([`SyncFs::read_range`]) only fetches the requested window, so a
    /// huge log costs no more than `max_bytes`.
    pub fn read_output(
        &self,
        task_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> EngineResult<String> {
        Self::validate_task_id(task_id)?;
        let path = self.tasks_dir.join(task_id).join("output.log");
        if !self.fs.exists(&path) {
            return Ok(String::new());
        }
        let bytes = self
            .fs
            .read_range(&path, offset, max_bytes)
            .map_err(io_to_engine)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Get the total size of the task's output log in bytes, or `0` when the
    /// task has no output log.
    pub fn output_size(&self, task_id: &str) -> EngineResult<u64> {
        Self::validate_task_id(task_id)?;
        let path = self.tasks_dir.join(task_id).join("output.log");
        if !self.fs.exists(&path) {
            return Ok(0);
        }
        Ok(self.fs.stat(&path).map(|stat| stat.size).unwrap_or(0))
    }

    /// Whether the task's output log exists on disk — the authoritative
    /// full-output probe (an empty-but-created log counts: the stream is
    /// the complete source even when nothing was appended yet).
    pub fn has_output_log(&self, task_id: &str) -> EngineResult<bool> {
        Self::validate_task_id(task_id)?;
        let path = self.tasks_dir.join(task_id).join("output.log");
        Ok(self.fs.exists(&path))
    }

    /// The path of the task's output log, anchored under the task's
    /// directory (no existence check — pair with [`Self::has_output_log`]).
    pub fn output_log_path(&self, task_id: &str) -> EngineResult<PathBuf> {
        Self::validate_task_id(task_id)?;
        Ok(self.tasks_dir.join(task_id).join("output.log"))
    }
}

/// Atomically replace `path` with `contents` via the shared persistence
/// primitive: unique sibling temp file (mode 0o600 on Unix), fsync, rename,
/// then a best-effort parent-directory fsync. Concurrent writers to the
/// same task never share a temp file, and a failed write leaves the
/// previous contents untouched.
fn write_atomic(path: &Path, contents: &str) -> EngineResult<()> {
    mirri_persistence::write_file_atomically(path, contents.as_bytes()).map_err(io_to_engine)
}

/// Convert a persistence I/O failure into an [`EngineError`].
///
/// Accepts both `std::io::Error` (the atomic writer) and `anyhow::Error`
/// (the injected [`SyncFs`] boundary); the message carries the
/// task-persistence context so warnings logged by the
/// [`TaskManager`](crate::task::TaskManager) identify the failing layer.
fn io_to_engine(error: impl std::fmt::Display) -> EngineError {
    EngineError::RequestInvalid(format!("task persistence I/O error: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskKind, TaskManager, TaskStatus};
    use chrono::Utc;
    use std::sync::Arc;

    /// A task id matching the shape `TaskManager::register` generates
    /// (`task-` prefix plus hex suffix).
    const TASK_ID: &str = "task-1a2b3c4d";

    fn persisted_task(id: &str, status: TaskStatus) -> AgentTask {
        AgentTask {
            id: id.to_string(),
            kind: TaskKind::Process {
                command: Some("echo test".to_string()),
                dispatcher: None,
            },
            description: "test task".to_string(),
            status,
            created_at: Utc::now(),
            ended_at: None,
            output: String::new(),
            session_id: "session-1".to_string(),
            deadline: None,
            detached: true,
            notification_suppressed: false,
            notification_delivered_at: None,
            fingerprint: None,
            lifetime_exempt: false,
        }
    }

    /// A fresh persistence root under a tempdir; the returned `TempDir`
    /// must stay alive for the test's duration.
    fn persistence_fixture() -> (tempfile::TempDir, PathBuf, Arc<TaskPersistence>) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tasks");
        let persistence = Arc::new(TaskPersistence::new(
            dir.clone(),
            Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        (tmp, dir, persistence)
    }

    #[test]
    fn should_persist_task_to_disk_when_registered_with_persistence() {
        let (tmp, dir, persistence) = persistence_fixture();
        let manager = TaskManager::with_persistence(persistence);

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build watcher".into(),
            "session-1".into(),
        );

        let task_path = dir.join(format!("{id}.json"));
        assert!(task_path.exists(), "register must write the task JSON");
        let on_disk: AgentTask =
            serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
        assert_eq!(on_disk.id, id);
        assert_eq!(on_disk.description, "build watcher");
        assert_eq!(on_disk.status, TaskStatus::Running);
        assert_eq!(on_disk.session_id, "session-1");
        drop(tmp);
    }

    #[test]
    fn should_load_tasks_from_disk_when_manager_created_with_persistence() {
        let (tmp, dir, persistence) = persistence_fixture();
        let task = persisted_task(TASK_ID, TaskStatus::Completed { exit_code: 0 });
        persistence.save_task(&task).unwrap();

        let manager = TaskManager::with_persistence(persistence);

        let loaded = manager.get(TASK_ID).expect("persisted task is loaded");
        assert_eq!(loaded, task);
        drop(dir);
        drop(tmp);
    }

    #[test]
    fn should_reconcile_running_tasks_as_stopped_on_load() {
        let (tmp, dir, persistence) = persistence_fixture();
        persistence
            .save_task(&persisted_task(TASK_ID, TaskStatus::Running))
            .unwrap();

        let manager = TaskManager::with_persistence(Arc::clone(&persistence));

        assert_eq!(
            manager
                .get(TASK_ID)
                .expect("persisted task is loaded")
                .status,
            TaskStatus::Stopped,
            "a running task on disk has no live process after restart"
        );
        // The reconciled status is written back so a later load sees
        // Stopped directly rather than reconciling again.
        let on_disk: AgentTask = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{TASK_ID}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk.status, TaskStatus::Stopped);
        drop(tmp);
    }

    #[test]
    fn should_append_output_to_disk_when_output_appended() {
        let (tmp, dir, persistence) = persistence_fixture();
        let manager = TaskManager::with_persistence(persistence);

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "logger".into(),
            "session-1".into(),
        );
        manager.append_output(&id, "line one\n");
        manager.append_output(&id, "line two\n");

        let log_path = dir.join(&id).join("output.log");
        assert!(
            log_path.exists(),
            "appending output must create the task's output.log"
        );
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            "line one\nline two\n",
            "output chunks must land in the log in append order"
        );
        drop(tmp);
    }

    #[test]
    fn should_update_task_status_on_disk_when_completed() {
        let (tmp, dir, persistence) = persistence_fixture();
        let manager = TaskManager::with_persistence(persistence);

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".into(),
            "session-1".into(),
        );
        manager.complete(&id, 0);

        let on_disk: AgentTask =
            serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap())
                .unwrap();
        assert_eq!(on_disk.status, TaskStatus::Completed { exit_code: 0 });
        drop(tmp);
    }

    #[test]
    fn should_not_persist_when_persistence_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = TaskManager::new();

        let id = manager.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "in-memory".into(),
            "session-1".into(),
        );
        manager.append_output(&id, "data");
        manager.complete(&id, 0);

        assert!(
            !tmp.path().join("tasks").exists(),
            "a manager without persistence must not create any files"
        );
        drop(tmp);
    }

    #[test]
    fn should_expose_the_output_log_path_and_existence_for_a_task() {
        let (tmp, dir, persistence) = persistence_fixture();

        assert!(
            !persistence.has_output_log(TASK_ID).unwrap(),
            "no log exists before any output is appended"
        );
        assert_eq!(
            persistence.output_log_path(TASK_ID).unwrap(),
            dir.join(TASK_ID).join("output.log"),
            "the log path is anchored under the task's directory"
        );

        persistence.append_output(TASK_ID, "data").unwrap();
        assert!(
            persistence.has_output_log(TASK_ID).unwrap(),
            "the log exists once output has been appended"
        );
        drop(tmp);
    }

    #[test]
    fn should_round_trip_a_fingerprint_through_the_task_archive() {
        // Given a persisted running task carrying a process fingerprint
        let (_tmp, _dir, persistence) = persistence_fixture();
        let mut task = persisted_task(TASK_ID, TaskStatus::Running);
        task.fingerprint = Some(mirri_os::process::ProcessFingerprint {
            pid: 4242,
            pgid: 4242,
            start_ticks: 987_654_321,
        });
        persistence.save_task(&task).unwrap();

        // When the archive is loaded through a fresh manager
        let loaded = TaskManager::with_persistence(Arc::clone(&persistence));

        // Then the fingerprint survives the round trip (identity is
        // preserved across a restart so the sweep can verify liveness).
        let reloaded = loaded.get(TASK_ID).expect("task loads");
        assert_eq!(reloaded.fingerprint.as_ref().unwrap().pid, 4242);
        assert_eq!(reloaded.fingerprint.as_ref().unwrap().start_ticks, 987_654_321);
    }

    #[test]
    fn should_load_a_legacy_archive_without_fingerprint_as_undeterminable() {
        // Given a legacy archive written before the fingerprint field existed
        let (_tmp, _dir, persistence) = persistence_fixture();
        persistence
            .save_task(&persisted_task(TASK_ID, TaskStatus::Completed { exit_code: 0 }))
            .unwrap();

        // When the archive is loaded
        let loaded = TaskManager::with_persistence(Arc::clone(&persistence));

        // Then the fingerprint is undeterminable (`None`) — the sweep
        // degrades such archives to report-only.
        assert!(loaded.get(TASK_ID).unwrap().fingerprint.is_none());
    }

    #[test]
    fn should_reject_invalid_task_ids() {
        let (tmp, dir, persistence) = persistence_fixture();

        let traversal = persisted_task("../escape", TaskStatus::Running);
        assert!(persistence.save_task(&traversal).is_err());
        assert!(persistence.append_output("../escape", "data").is_err());
        assert!(persistence.read_output("../escape", 0, 100).is_err());
        assert!(persistence.output_size("../escape").is_err());

        assert!(
            persistence.load_tasks().unwrap().is_empty(),
            "a rejected id must not write anything outside the tasks dir"
        );
        assert!(!tmp.path().join("escape.json").exists());
        drop(dir);
        drop(tmp);
    }

    // -----------------------------------------------------------------------
    // T-13b2-1 — in-memory SyncFs double (zero-IO T13b)
    // -----------------------------------------------------------------------

    /// In-memory [`SyncFs`] double: files and directories live in maps, and
    /// every interesting operation is recorded so tests can assert the
    /// persistence layer went through the injected provider (and with which
    /// arguments — e.g. the exact `read_range` offset).
    struct MemoryFs {
        files: std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<u8>>>,
        dirs: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
        read_range_calls: std::sync::Mutex<Vec<(PathBuf, u64, usize)>>,
        append_calls: std::sync::Mutex<Vec<(PathBuf, String)>>,
        mkdir_private_calls: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl MemoryFs {
        fn new() -> Self {
            Self {
                files: std::sync::Mutex::new(std::collections::HashMap::new()),
                dirs: std::sync::Mutex::new(std::collections::HashSet::new()),
                read_range_calls: std::sync::Mutex::new(Vec::new()),
                append_calls: std::sync::Mutex::new(Vec::new()),
                mkdir_private_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn seed(&self, path: &Path, contents: &str) {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents.as_bytes().to_vec());
        }

        fn read_range_calls(&self) -> Vec<(PathBuf, u64, usize)> {
            self.read_range_calls.lock().unwrap().clone()
        }

        fn append_calls(&self) -> Vec<(PathBuf, String)> {
            self.append_calls.lock().unwrap().clone()
        }

        fn mkdir_private_calls(&self) -> Vec<PathBuf> {
            self.mkdir_private_calls.lock().unwrap().clone()
        }
    }

    impl mirri_os::SyncFs for MemoryFs {
        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
        }

        fn stat(&self, path: &Path) -> anyhow::Result<mirri_os::fs::FileStat> {
            let size = self
                .files
                .lock()
                .unwrap()
                .get(path)
                .map(|bytes| bytes.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))?;
            Ok(mirri_os::fs::FileStat {
                size,
                is_file: true,
                is_dir: false,
                is_symlink: false,
                mtime: 0,
                atime: 0,
                mode: 0o600,
            })
        }

        fn exists(&self, path: &Path) -> bool {
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();
            // A directory "exists" when it holds at least one entry, so a
            // seeded tasks dir is visible without an explicit dir record.
            files.contains_key(path)
                || dirs.contains(path)
                || files.keys().any(|k| k.starts_with(path))
                || dirs.iter().any(|k| k.starts_with(path))
        }

        fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<mirri_os::fs::DirEntry>> {
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();
            let mut entries = Vec::new();
            for file_path in files.keys() {
                if file_path.parent() == Some(path) {
                    entries.push(mirri_os::fs::DirEntry {
                        name: file_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: file_path.to_string_lossy().into_owned(),
                        is_dir: false,
                        is_file: true,
                    });
                }
            }
            for dir_path in dirs.iter() {
                if dir_path.parent() == Some(path) {
                    entries.push(mirri_os::fs::DirEntry {
                        name: dir_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: dir_path.to_string_lossy().into_owned(),
                        is_dir: true,
                        is_file: false,
                    });
                }
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        }

        fn append(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
            self.append_calls
                .lock()
                .unwrap()
                .push((path.to_path_buf(), contents.to_string()));
            self.files
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_default()
                .extend_from_slice(contents.as_bytes());
            Ok(())
        }

        fn read_range(
            &self,
            path: &Path,
            offset: u64,
            max_bytes: usize,
        ) -> anyhow::Result<Vec<u8>> {
            self.read_range_calls
                .lock()
                .unwrap()
                .push((path.to_path_buf(), offset, max_bytes));
            let files = self.files.lock().unwrap();
            let bytes = files
                .get(path)
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))?;
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(max_bytes).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }

        fn mkdir_private(&self, path: &Path) -> anyhow::Result<()> {
            self.mkdir_private_calls
                .lock()
                .unwrap()
                .push(path.to_path_buf());
            self.dirs.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn should_roundtrip_all_apis_through_an_in_memory_sync_fs_double() {
        // Given a persistence layer whose filesystem is an in-memory double
        // seeded with one task record (the JSON write itself goes through
        // the shared atomic writer, outside the SyncFs seam)
        let fs = Arc::new(MemoryFs::new());
        let tasks_dir = PathBuf::from("/virtual/tasks");
        let persistence = TaskPersistence::new(
            tasks_dir.clone(),
            Arc::clone(&fs) as Arc<dyn mirri_os::SyncFs>,
        );
        let task = persisted_task(TASK_ID, TaskStatus::Running);
        fs.seed(
            &tasks_dir.join(format!("{TASK_ID}.json")),
            &serde_json::to_string_pretty(&task).unwrap(),
        );

        // When the task is loaded, the store is enumerated and read through
        // the double
        let loaded = persistence.load_tasks().unwrap();
        assert_eq!(loaded.len(), 1, "the seeded record must be visible");
        assert_eq!(loaded[0].id, TASK_ID);

        // And appending output routes through the double's append (the
        // fsync-carrying operation), accumulating in order
        persistence.append_output(TASK_ID, "chunk one").unwrap();
        persistence.append_output(TASK_ID, "chunk two").unwrap();
        assert_eq!(
            fs.append_calls(),
            vec![
                (
                    tasks_dir.join(TASK_ID).join("output.log"),
                    "chunk one".to_string()
                ),
                (
                    tasks_dir.join(TASK_ID).join("output.log"),
                    "chunk two".to_string()
                ),
            ],
            "appends must go through the injected provider's append"
        );

        // And an offset read requests exactly the (offset, max) window
        let window = persistence.read_output(TASK_ID, 10, 4).unwrap();
        assert_eq!(window, "hunk", "the window must start at the byte offset");
        assert_eq!(
            fs.read_range_calls(),
            vec![(tasks_dir.join(TASK_ID).join("output.log"), 10, 4)],
            "read_output must fetch only the requested offset window"
        );

        // And the full stream, size, and existence probe all reflect the
        // double's store
        assert_eq!(
            persistence.read_output(TASK_ID, 0, 100).unwrap(),
            "chunk onechunk two"
        );
        assert_eq!(persistence.output_size(TASK_ID).unwrap(), 18);
        assert!(persistence.has_output_log(TASK_ID).unwrap());
        assert!(!persistence.has_output_log("task-other").unwrap());
    }

    #[test]
    fn should_create_private_dirs_through_the_injected_fs_when_saving() {
        // Given a persistence layer over a real (pre-created) tasks dir but
        // an in-memory filesystem double
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();
        let fs = Arc::new(MemoryFs::new());
        let persistence = TaskPersistence::new(
            tmp.path().join("tasks"),
            Arc::clone(&fs) as Arc<dyn mirri_os::SyncFs>,
        );

        // When a task is saved, the private-directory creation goes through
        // the double (the JSON write itself uses the shared atomic writer)
        persistence
            .save_task(&persisted_task(TASK_ID, TaskStatus::Running))
            .unwrap();

        assert_eq!(
            fs.mkdir_private_calls(),
            vec![tmp.path().join("tasks")],
            "saving must create the tasks dir through the injected provider"
        );
    }

    #[test]
    fn should_return_no_tasks_when_the_double_has_no_tasks_dir() {
        // Given a persistence root that does not exist in the double's store
        let fs = Arc::new(MemoryFs::new());
        let persistence = TaskPersistence::new(
            PathBuf::from("/virtual/elsewhere"),
            Arc::clone(&fs) as Arc<dyn mirri_os::SyncFs>,
        );

        // When tasks are loaded
        let loaded = persistence.load_tasks().unwrap();

        // Then the result is empty without touching the host filesystem
        assert!(loaded.is_empty());
        assert!(fs.read_range_calls().is_empty());
    }
}
