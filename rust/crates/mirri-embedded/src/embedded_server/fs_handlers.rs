//! Host-folder-browser dispatch handlers for the embedded
//! [`EmbeddedServer`](super::EmbeddedServer) — the B5-a `fs.*` surface, the
//! klient `hostFolderBrowser` contract mirrored on the engine wire
//! (snake_case fields, per `protocol/src/rest/fsBrowse.ts`).
//!
//! - `fs.browse(path?)` — list the *directories* under an absolute path
//!   (the contract's entries are directory rows with the `is_dir: true`
//!   literal; file rows are not part of the wire face). The protocol error
//!   semantics are preserved: a non-absolute path is invalid params, a
//!   missing/not-a-directory path is not found, a permission failure is an
//!   internal error carrying the reason.
//! - `fs.home()` — the OS user home plus the recent-roots list. The rust
//!   engine has no recent-workspace history store, so `recent_roots`
//!   degrades honestly to `[]` (TSV row note records the adjudication;
//!   no fabricated history).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use mirri_transport::adapter::EngineError;

use super::responses::{FsBrowseEntry, FsBrowseResult, FsHomeResult, to_result};
use super::{EmbeddedServer, invalid_params, normalize_params, not_found};

impl EmbeddedServer {
    /// `fs.browse` — the directories under an absolute path (the klient
    /// `fsBrowseResponseSchema` snake_case shape). The optional `path`
    /// parameter accepts the positional `["<path>"]`, object
    /// `{"path": "<path>"}`, and bare-string spellings; omitted means the
    /// user's home directory.
    pub(super) async fn dispatch_fs_browse(&self, params: Value) -> Result<Value, EngineError> {
        let path = browse_path_params(params)?
            .map(PathBuf::from)
            .map_or_else(|| self.fs_os_home(), Ok)?;

        let entries = read_child_dirs(&path)?;
        let parent = path.parent().map(|p| p.to_string_lossy().to_string());
        to_result(FsBrowseResult {
            path: path.to_string_lossy().to_string(),
            parent,
            entries,
        })
    }

    /// `fs.home` — the OS user home and the (honestly empty) recent-roots
    /// list, in the klient `fsHomeResponseSchema` snake_case shape.
    pub(super) async fn dispatch_fs_home(&self) -> Result<Value, EngineError> {
        let home = self.fs_os_home()?;
        to_result(FsHomeResult {
            home: home.to_string_lossy().to_string(),
            recent_roots: Vec::new(),
        })
    }

    /// The OS user home — the folder browser's anchor. An unresolvable home
    /// is an honest internal error, never a panic.
    fn fs_os_home(&self) -> Result<PathBuf, EngineError> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| {
                EngineError::Internal("fs.home: neither HOME nor USERPROFILE is set".to_string())
            })
    }
}

/// Parse the optional browse path: the positional `["<path>"]`, object
/// `{"path": "<path>"}`, or bare-string spelling; `None` when omitted.
fn browse_path_params(params: Value) -> Result<Option<String>, EngineError> {
    #[derive(Debug, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BrowseParams {
        #[serde(default)]
        path: Option<String>,
    }
    if params.is_null() {
        return Ok(None);
    }
    if let Value::String(path) = &params {
        return Ok(Some(path.clone()));
    }
    if let Value::Array(items) = &params {
        if items.len() > 1
            || items
                .iter()
                .any(|item| !item.is_null() && !item.is_string())
        {
            return Err(invalid_params(
                "fs.browse takes at most one path: [\"<path>\"] or {\"path\": \"<path>\"}",
            ));
        }
        return Ok(items
            .first()
            .and_then(|item| item.as_str())
            .map(str::to_string));
    }
    let params: BrowseParams = serde_json::from_value(normalize_params(params))
        .map_err(|e| invalid_params(e.to_string()))?;
    Ok(params.path)
}

/// Read the child *directories* of `path` (sorted by name, the deterministic
/// wire order), preserving the protocol error semantics: non-absolute →
/// invalid params; ENOENT / ENOTDIR → not found; EACCES → internal.
fn read_child_dirs(path: &Path) -> Result<Vec<FsBrowseEntry>, EngineError> {
    if !path.is_absolute() {
        return Err(invalid_params(format!(
            "path must be absolute: {}",
            path.display()
        )));
    }
    let read_dir = std::fs::read_dir(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => not_found(format!("path not found: {}", path.display())),
        std::io::ErrorKind::NotADirectory => {
            not_found(format!("path not a directory: {}", path.display()))
        }
        std::io::ErrorKind::PermissionDenied => {
            EngineError::Internal(format!("permission denied: {}", path.display()))
        }
        _ => EngineError::Internal(format!("fs.browse {}: {e}", path.display())),
    })?;
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in read_dir {
        let entry = entry
            .map_err(|e| EngineError::Internal(format!("fs.browse {}: {e}", path.display())))?;
        // The contract's entries carry the `is_dir: true` literal — only
        // directories are listed (file rows are not part of the wire face).
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            entries.push((
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            ));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries
        .into_iter()
        .map(|(name, path)| FsBrowseEntry {
            path: path.to_string_lossy().to_string(),
            name,
            // The contract's directory-row literal — only directories are
            // listed, so the flag is always true on this wire face.
            is_dir: true,
        })
        .collect())
}
