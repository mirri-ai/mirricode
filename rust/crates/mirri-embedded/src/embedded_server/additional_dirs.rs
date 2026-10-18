//! Workspace-local additional-dir plumbing for the embedded server: the
//! `local.toml` read/write helpers (shared by `session.addAdditionalDir` and
//! the plan-D3 directory-grant sink) and the [`LocalAdditionalDirGrantSink`]
//! implementation of the core's [`AdditionalDirGrantSink`] trait.
//!
//! The core defines the trait; this composition-root module implements the
//! zero-IO boundary: applying a grant widens the session's shared
//! [`WorkspacePaths`](mirri_core::tools::WorkspacePaths) (in memory, like
//! the non-persisted `addAdditionalDir` arm), and `persist` additionally
//! writes `<projectRoot>/.mirri-code/local.toml`'s `workspace.additional_dir`
//! — the exact list `seed_workspace_additional_dirs` re-reads on resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_core::SessionScope;
use mirri_core::tools::WorkspacePaths;
use mirri_transport::adapter::EngineError;

use super::invalid_params;

/// The embedded implementation of [`mirri_core::AdditionalDirGrantSink`] —
/// applies a granted directory into the session's shared sandbox and, for a
/// persist grant, into the workspace-local config.
pub struct LocalAdditionalDirGrantSink {
    /// The session's shared sandbox (root + additional dirs) — the same
    /// `Arc` every file tool and the permission chain classify against.
    paths: Arc<WorkspacePaths>,
    /// The project root whose `.mirri-code/local.toml` a persist grant
    /// writes (the same root `session.addAdditionalDir` resolves).
    project_root: PathBuf,
}

impl LocalAdditionalDirGrantSink {
    /// Build a sink over the session's sandbox and the project root that
    /// owns it.
    pub fn new(paths: Arc<WorkspacePaths>, project_root: PathBuf) -> Self {
        Self {
            paths,
            project_root,
        }
    }
}

impl mirri_core::AdditionalDirGrantSink for LocalAdditionalDirGrantSink {
    fn apply_grant(&self, dir: &Path, persist: bool) {
        // The sandbox widening is idempotent (`add_additional` dedups) and
        // affects every agent in the session — the shared handle. This is
        // the same runtime-state path the non-persisted `addAdditionalDir`
        // arm takes; nothing is written to the wire log (a session grant is
        // runtime state, restored after a persist only via local.toml).
        self.paths.add_additional(dir.to_path_buf());
        if persist {
            let config_path = self.project_root.join(".mirri-code").join("local.toml");
            if let Err(error) = persist_grant_dir(&config_path, &self.project_root, dir) {
                tracing::warn!(
                    %error,
                    "could not persist the granted directory; the grant stays session-only"
                );
            }
        }
    }
}

/// Build the session's directory-grant sink (plan D3): a
/// [`LocalAdditionalDirGrantSink`] over the session's shared sandbox and its
/// project root. `None` when the session has no workspace sandbox (legacy
/// sessions) — a session/persist grant decision then degrades to a one-off
/// approval exactly like an unwired sink.
pub fn session_grant_sink(
    session: &Arc<SessionScope>,
) -> Option<Arc<dyn mirri_core::AdditionalDirGrantSink>> {
    let paths = session.workspace_paths()?;
    let cwd = session.workspace_root()?;
    let project_root = find_project_root(cwd);
    Some(Arc::new(LocalAdditionalDirGrantSink::new(
        paths,
        project_root,
    )))
}

/// Merge one granted dir into the persisted `workspace.additional_dir` list,
/// preserving every other key the file carried (the same edit shape
/// `session.addAdditionalDir` applies — a dir already listed is a no-op).
fn persist_grant_dir(
    config_path: &Path,
    project_root: &Path,
    dir: &Path,
) -> Result<(), EngineError> {
    let file = read_workspace_local_toml(config_path)?;
    let file_dirs = resolve_existing_additional_dirs(project_root, file_dirs_of(&file)?);
    if file_dirs.iter().any(|existing| paths_equal(existing, dir)) {
        return Ok(());
    }
    let mut dirs = file_dirs.clone();
    dirs.push(dir.to_path_buf());
    write_workspace_local_toml(config_path, &file, &dirs)
}

/// Find the project root: the nearest ancestor of `start` (inclusive)
/// carrying a `.git` marker, falling back to `start` itself (the TS
/// `findProjectRoot` behavior when the filesystem root is reached).
pub(super) fn find_project_root(start: &std::path::Path) -> PathBuf {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return start.to_path_buf(),
        }
    }
}

/// Read the workspace-local `local.toml` when it exists; a missing file is
/// the empty config (the TS `readWorkspaceLocalToml` undefined arm), an
/// empty file the empty table. Unparseable TOML is a hard error.
pub(super) fn read_workspace_local_toml(
    config_path: &std::path::Path,
) -> Result<toml::Value, EngineError> {
    if !config_path.exists() {
        return Ok(toml::Value::Table(Default::default()));
    }
    let text = std::fs::read_to_string(config_path)
        .map_err(|e| invalid_params(format!("failed to read {}: {e}", config_path.display())))?;
    if text.trim().is_empty() {
        return Ok(toml::Value::Table(Default::default()));
    }
    toml::from_str(&text)
        .map_err(|e| invalid_params(format!("invalid TOML in {}: {e}", config_path.display())))
}

/// Extract the persisted `workspace.additional_dir` list from a parsed
/// local.toml. A missing key is the empty list; anything but an array of
/// strings is a hard error (the TS schema's
/// "workspace.additional_dir must be an array of strings").
pub(super) fn file_dirs_of(file: &toml::Value) -> Result<Vec<String>, EngineError> {
    let Some(workspace) = file.get("workspace") else {
        return Ok(Vec::new());
    };
    let Some(dirs) = workspace.get("additional_dir") else {
        return Ok(Vec::new());
    };
    let Some(array) = dirs.as_array() else {
        return Err(invalid_params(
            "workspace.additional_dir must be an array of strings",
        ));
    };
    array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                invalid_params("workspace.additional_dir must be an array of strings")
            })
        })
        .collect()
}

/// Lexically normalize a path (`.` dropped, `..` resolved without I/O) —
/// the closest std equivalent of the TS `normalize`. `..` folds against
/// the previous normal segment; above the root it collapses (`/..` is
/// `/`), while a leading `..` on a relative path is real movement
/// (`../b` stays `../b`) — the same folding `Path::components` skips and
/// `path_clean`/`normalize` perform.
fn normalize_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut stack: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                // `..` above a root/prefix collapses (`/..` is `/`) —
                // there is no parent to fold against.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                // A leading (or already-collapsed) `..` on a relative path
                // is real upward movement — keep it.
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
}

/// Expand a leading `~` to the home directory (the TS `expandHome`): `~`
/// alone becomes `$HOME`, `~/rest` becomes `$HOME/rest`; an unset `$HOME`
/// leaves the input untouched.
fn expand_home(input: &str) -> PathBuf {
    if input == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(rest),
            Err(_) => PathBuf::from(input),
        }
    } else {
        PathBuf::from(input)
    }
}

/// Resolve one additional dir against the session cwd: `~` expands to the
/// home directory, absolute paths normalize as-is, relative paths resolve
/// under the base (the TS `resolveAdditionalDir(kaos, workDir, ...)` —
/// the base is the caller's work dir for new inputs, the project root for
/// persisted entries).
pub(super) fn resolve_additional_dir(
    base: &std::path::Path,
    input: &str,
) -> Result<PathBuf, EngineError> {
    let expanded = expand_home(input);
    let resolved = if expanded.is_absolute() {
        normalize_path(&expanded)
    } else {
        normalize_path(&base.join(expanded))
    };
    // The directory must exist (the TS `assertDirectory`).
    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => Ok(resolved),
        _ => Err(invalid_params(format!(
            "workspace.additional_dir must exist and be a directory: {}",
            resolved.display()
        ))),
    }
}

/// Resolve the persisted dirs against the project root WITHOUT the
/// existence assertion (the TS `resolveExistingAdditionalDirs` — a dir
/// deleted after it was persisted degrades out of the list, it never fails
/// the call), expanding `~`, normalizing, and deduplicating.
pub(super) fn resolve_existing_additional_dirs(
    project_root: &std::path::Path,
    dirs: Vec<String>,
) -> Vec<PathBuf> {
    let mut resolved: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let expanded = expand_home(&dir);
        let path = if expanded.is_absolute() {
            normalize_path(&expanded)
        } else {
            normalize_path(&project_root.join(expanded))
        };
        if !resolved.iter().any(|existing| paths_equal(existing, &path)) {
            resolved.push(path);
        }
    }
    resolved
}

/// Compare two paths for equality after lexical normalization (the TS
/// `hasSameAdditionalDir`'s `normpath` comparison — no symlink resolution,
/// so an alias via symlink counts as a different dir).
pub(super) fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    a == b
}

/// Persist the additional-dir list back into the workspace-local
/// `local.toml`, preserving every other key the file carried (the TS raw
/// table edit — only `workspace.additional_dir` is replaced).
pub(super) fn write_workspace_local_toml(
    config_path: &std::path::Path,
    file: &toml::Value,
    dirs: &[PathBuf],
) -> Result<(), EngineError> {
    let mut root = file.clone();
    let workspace = root
        .as_table_mut()
        .ok_or_else(|| invalid_params("workspace must be a table"))?
        .entry("workspace")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let table = workspace
        .as_table_mut()
        .ok_or_else(|| invalid_params("workspace must be a table"))?;
    table.insert(
        "additional_dir".to_string(),
        toml::Value::Array(
            dirs.iter()
                .map(|dir| toml::Value::String(dir.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| invalid_params(format!("failed to create {}: {e}", parent.display())))?;
    }
    let text = toml::to_string(&root)
        .map_err(|e| EngineError::Internal(format!("failed to serialize local.toml: {e}")))?;
    std::fs::write(config_path, format!("{text}\n"))
        .map_err(|e| invalid_params(format!("failed to write {}: {e}", config_path.display())))
}
