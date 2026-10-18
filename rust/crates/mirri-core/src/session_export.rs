//! Session export — package a session directory into a portable zip archive.
//!
//! This module ports the TypeScript `sessionExport` feature. It reads a
//! session directory (containing `state.json`, `wire.jsonl`, optional
//! `snapshot.json`), builds a manifest with metadata, and writes a zip
//! archive containing the session files plus a `manifest.json`.
//!
//! ## Directory layout
//!
//! ```text
//! <home_dir>/.mirri-code/
//!   sessions/
//!     <session_id>/
//!       state.json
//!       wire.jsonl
//!       snapshot.json  (optional)
//!   logs/
//!     global.log      (optional, included when include_global_log is true)
//!   exports/
//!     <session_id>.zip  (default output location)
//! ```
//!
//! ## Archive contents
//!
//! The zip archive contains:
//! - All files from the session directory (preserving filenames)
//! - `manifest.json` — the [`ExportSessionManifest`] serialised as JSON
//! - Optional global/desktop/web log files when requested
//!
//! ## Zero-IO note
//!
//! All filesystem access goes through an injected [`SyncFs`] provider
//! (engine default: `LocalSyncFs`). The zip archive is built entirely in
//! memory and persisted with a single `write_bytes` call. Trade-off
//! (accepted, T13 J4): a large session's `wire.log` is held in memory
//! twice (source bytes + zip buffer) during export, so peak memory grows
//! with archive size — see ROADMAP §5 for the optional streaming-FileSink
//! follow-up.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mirri_os::SyncFs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during session export.
#[derive(Debug, Error)]
pub enum SessionExportError {
    /// An I/O error occurred (file read/write, directory traversal).
    ///
    /// Holds the original [`std::io::Error`] so the source chain survives
    /// `#[from]` conversion.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A zip-archive error occurred (compression, finalisation).
    #[error("zip archive error: {0}")]
    Zip(String),
    /// The session directory was not found under the expected path.
    #[error("session not found: {0}")]
    NotFound(String),
    /// The total size of files to archive exceeds the configured limit.
    #[error("archive too large: {size} bytes exceeds limit of {limit}")]
    TooLarge { size: u64, limit: u64 },
}

impl From<zip::result::ZipError> for SessionExportError {
    fn from(err: zip::result::ZipError) -> Self {
        Self::Zip(err.to_string())
    }
}

/// Convert an injected-filesystem failure into the [`SessionExportError::Io`]
/// variant this module historically surfaced for host-fs errors.
fn fs_err(error: anyhow::Error) -> SessionExportError {
    SessionExportError::Io(std::io::Error::other(format!("{error:#}")))
}

// ---------------------------------------------------------------------------
// Shell environment
// ---------------------------------------------------------------------------

/// Terminal and shell environment metadata captured at export time.
///
/// Mirrors the TypeScript `ShellEnvironment` interface. All fields are
/// optional because not all environment variables are guaranteed to be set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShellEnvironment {
    /// `$TERM` — the terminal type (e.g. `"xterm-256color"`).
    pub term: Option<String>,
    /// `$TERM_PROGRAM` — the terminal application (e.g. `"iTerm.app"`).
    pub term_program: Option<String>,
    /// `$TERM_PROGRAM_VERSION` — terminal application version.
    pub term_program_version: Option<String>,
    /// Detected multiplexer (`"tmux"`, `"screen"`, or `None`).
    pub multiplexer: Option<String>,
    /// `$SHELL` — the user's shell binary path.
    pub shell: Option<String>,
}

impl ShellEnvironment {
    /// Capture the shell environment from the current process's environment
    /// variables.
    pub fn capture() -> Self {
        let multiplexer = if std::env::var("TMUX").is_ok() {
            Some("tmux".to_string())
        } else if std::env::var("STY").is_ok() {
            Some("screen".to_string())
        } else {
            None
        };

        Self {
            term: std::env::var("TERM").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
            term_program_version: std::env::var("TERM_PROGRAM_VERSION").ok(),
            multiplexer,
            shell: std::env::var("SHELL").ok(),
        }
    }
}

// ---------------------------------------------------------------------------
// Export payload and options
// ---------------------------------------------------------------------------

/// The request payload for a session export operation.
///
/// Provided by the host (CLI or desktop app) when the user requests an export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSessionPayload {
    /// The session id to export.
    pub session_id: String,
    /// Explicit output path for the zip archive. When `None`, the exporter
    /// writes to `<home_dir>/.mirri-code/exports/<session_id>.zip`.
    pub output_path: Option<String>,
    /// Whether to include the global log file in the archive.
    pub include_global_log: Option<bool>,
    /// Whether to include the desktop log file in the archive.
    pub include_desktop_log: Option<bool>,
    /// The caller's version string (recorded in the manifest).
    pub version: String,
    /// The desktop app version, if applicable.
    pub desktop_version: Option<String>,
    /// How the app was installed (e.g. `"npm"`, `"homebrew"`).
    pub install_source: Option<String>,
    /// Captured shell environment metadata.
    pub shell_env: Option<ShellEnvironment>,
}

/// Optional export-time configuration.
#[derive(Debug, Clone, Default)]
pub struct ExportSessionOptions {
    /// Path to a web log file to include in the archive.
    pub web_log: Option<String>,
    /// Maximum total uncompressed bytes allowed in the archive. When the
    /// session files exceed this limit, export fails with
    /// [`SessionExportError::TooLarge`].
    pub max_archive_bytes: Option<u64>,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Metadata manifest included as `manifest.json` inside the archive.
///
/// Records provenance, environment, and session metadata so the archive is
/// self-describing and can be inspected without running the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSessionManifest {
    /// The session id that was exported.
    pub session_id: String,
    /// When the export was created.
    pub exported_at: DateTime<Utc>,
    /// The version of the app that produced the export.
    pub kimi_code_version: String,
    /// The wire protocol version in use.
    pub wire_protocol_version: String,
    /// Operating system name (e.g. `"macos"`, `"linux"`).
    pub os: String,
    /// The runtime version (e.g. the Rust toolchain or Node.js version).
    ///
    /// Serialises under the key `nodejsVersion` — the v2 authoritative
    /// contract names this field after the TS runtime even when the Rust
    /// engine fills it with its own toolchain version.
    #[serde(rename = "nodejsVersion")]
    pub runtime_version: String,
    /// First activity timestamp from session state, if available.
    pub session_first_activity: Option<DateTime<Utc>>,
    /// Last activity timestamp from session state, if available.
    pub session_last_activity: Option<DateTime<Utc>>,
    /// Session title, if available.
    pub title: Option<String>,
    /// The workspace directory path.
    pub workspace_dir: Option<String>,
    /// Path to the session log directory.
    pub session_log_path: Option<String>,
    /// Path to the global log file, if included.
    pub global_log_path: Option<String>,
    /// Path to the desktop log file, if included.
    pub desktop_log_path: Option<String>,
    /// Path to the web log file, if included.
    pub web_log_path: Option<String>,
    /// Desktop app version, if applicable.
    pub desktop_version: Option<String>,
    /// How the app was installed.
    pub install_source: Option<String>,
    /// Captured shell environment.
    pub shell_env: Option<ShellEnvironment>,
}

/// The result of a successful session export.
#[derive(Debug, Clone)]
pub struct ExportSessionResult {
    /// Path to the created zip archive.
    pub zip_path: String,
    /// List of entry names inside the archive (including `manifest.json`).
    pub entries: Vec<String>,
    /// The session directory that was exported.
    pub session_dir: String,
    /// The manifest that was included in the archive.
    pub manifest: ExportSessionManifest,
}

// ---------------------------------------------------------------------------
// Session exporter
// ---------------------------------------------------------------------------

/// Subdirectory under the Mirri home directory where the rust engine's
/// sessions live (the v3 storage root — ADR-006-R2).
const V3_SESSIONS_SUBDIR: &str = "v3_sessions";

/// Subdirectory under the Mirri home directory where the TS-era sessions
/// live (the pre-migration root; `mirri migrate` copies them out).
const SESSIONS_SUBDIR: &str = "sessions";

/// Subdirectory under the Mirri home directory where logs are stored.
const LOGS_SUBDIR: &str = "logs";

/// Subdirectory under the Mirri home directory where exports are written.
const EXPORTS_SUBDIR: &str = "exports";

/// The wire protocol version recorded in the manifest.
///
/// This intentionally matches the session wire protocol version shared by
/// the TypeScript reference implementation and the Rust persistence layer
/// (`mirri_persistence::migration::CURRENT_VERSION` is `"2.0"` after the
/// storage-root cut — the identity 1.6 → 2.0 migration). The export manifest
/// is a self-describing metadata record consumed by the TS app and by
/// humans inspecting archives, so the value is part of the archive's public
/// contract. Changing it without a coordinated wire-format upgrade across
/// both engines would make archives claim a protocol they do not implement
/// — do not modify this casually.
const WIRE_PROTOCOL_VERSION: &str = "2.0";

/// The name of the manifest file inside the archive.
const MANIFEST_ENTRY_NAME: &str = "manifest.json";

/// Packages a session directory into a portable zip archive.
///
/// The exporter is cheap to construct — it only holds the Mirri home directory
/// path (the `.mirri-code` data root, not the user's home). Each call to
/// [`SessionExporter::export`] performs file I/O and produces a
/// self-contained zip archive.
pub struct SessionExporter {
    /// The Mirri home directory (`<user home>/.mirri-code`, or the injected
    /// engine home), under which `sessions/`, `logs/`, and `exports/` live.
    pub home_dir: PathBuf,
    /// The user's home directory, when known (the [`SessionExporter::new`]
    /// path). OS-standard desktop log locations resolve from here; the
    /// injected-home constructor has no user home and falls back to
    /// `<mirri home>/logs/desktop.log`.
    user_home: Option<PathBuf>,
    /// Filesystem provider backing the export reads and the zip write
    /// (zero-IO T13c-2; the engine default is `LocalSyncFs`).
    fs: Arc<dyn SyncFs>,
}

impl SessionExporter {
    /// Create a new exporter rooted at the given user home directory — the
    /// Mirri home is derived as `<home_dir>/.mirri-code`.
    ///
    /// Filesystem access goes through the host-local `LocalSyncFs`; inject a
    /// double with [`SessionExporter::with_fs`] instead.
    pub fn new(home_dir: impl Into<PathBuf>) -> Self {
        Self::user_home_with_fs(home_dir.into(), Arc::new(mirri_os::LocalSyncFs::new()))
    }

    /// Create a new exporter rooted at the given Mirri home directory (the
    /// `.mirri-code` data root itself). Embedded callers that received an
    /// explicit home (the NAPI `createEngine(homeDir)` injection) use this so
    /// the export root, session dir, and log dir all resolve under the
    /// injected home instead of the ambient `$HOME`.
    pub fn with_mirri_home(mirri_home: impl Into<PathBuf>) -> Self {
        Self::mirri_home_with_fs(mirri_home, Arc::new(mirri_os::LocalSyncFs::new()))
    }

    /// Create a new exporter rooted at the given Mirri home whose session
    /// reads, log reads, and zip write all go through `fs`.
    pub fn with_fs(mirri_home: impl Into<PathBuf>, fs: Arc<dyn SyncFs>) -> Self {
        Self::mirri_home_with_fs(mirri_home, fs)
    }

    /// Shared constructor: Mirri home + injected filesystem provider.
    fn mirri_home_with_fs(mirri_home: impl Into<PathBuf>, fs: Arc<dyn SyncFs>) -> Self {
        Self {
            home_dir: mirri_home.into(),
            user_home: None,
            fs,
        }
    }

    /// Shared constructor for the user-home form (desktop log locations
    /// resolve under the user home).
    fn user_home_with_fs(user_home: PathBuf, fs: Arc<dyn SyncFs>) -> Self {
        Self {
            home_dir: user_home.join(".mirri-code"),
            user_home: Some(user_home),
            fs,
        }
    }

    /// Compute the session directory path for a given session id.
    ///
    /// Dual-root (ADR-006-R2): the rust engine writes sessions under
    /// `<home>/v3_sessions/` while TS-era sessions live under
    /// `<home>/sessions/`. The v3 root is consulted first — both for the
    /// flat form (`<root>/<session_id>/`) and the TS bucketed form
    /// (`<root>/<bucket>/<session_id>/`) — with the legacy root as the
    /// pre-migration-era fallback. The v3 flat path is returned when
    /// neither root has the session so the caller's NotFound error names the
    /// canonical location.
    fn session_dir(&self, session_id: &str) -> PathBuf {
        let v3_root = self.home_dir.join(V3_SESSIONS_SUBDIR);
        if let Some(dir) = self.find_session_dir_in_root(&v3_root, session_id) {
            return dir;
        }
        let legacy_root = self.home_dir.join(SESSIONS_SUBDIR);
        if let Some(dir) = self.find_session_dir_in_root(&legacy_root, session_id) {
            return dir;
        }
        v3_root.join(session_id)
    }

    /// Locate a session under a single root: the flat layout
    /// (`<root>/<session_id>/`) first, then the TS bucketed layout
    /// (`<root>/<bucket>/<session_id>/`) via a scan of the root's
    /// first-level directories.
    fn find_session_dir_in_root(&self, root: &Path, session_id: &str) -> Option<PathBuf> {
        let flat = root.join(session_id);
        if self.fs.exists(&flat) {
            return Some(flat);
        }
        if let Ok(entries) = self.fs.iterdir(root) {
            for entry in entries {
                if !entry.is_dir {
                    continue;
                }
                let candidate = Path::new(&entry.path).join(session_id);
                if self.fs.exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// Compute the default export output path for a given session id.
    fn default_output_path(&self, session_id: &str) -> PathBuf {
        self.home_dir
            .join(EXPORTS_SUBDIR)
            .join(format!("{session_id}.zip"))
    }

    /// Compute the global log file path.
    fn global_log_path(&self) -> PathBuf {
        self.home_dir.join(LOGS_SUBDIR).join("global.log")
    }

    /// Export a session to a zip archive.
    ///
    /// Reads all files from the session directory, optionally includes log
    /// files, builds a manifest, and writes a zip archive.
    ///
    /// # Errors
    ///
    /// - [`SessionExportError::NotFound`] — the session directory does not
    ///   exist.
    /// - [`SessionExportError::TooLarge`] — the total file size exceeds
    ///   `options.max_archive_bytes`.
    /// - [`SessionExportError::Io`] — a file read or write failed.
    /// - [`SessionExportError::Zip`] — zip archive creation failed.
    pub fn export(
        &self,
        payload: ExportSessionPayload,
        options: Option<ExportSessionOptions>,
    ) -> Result<ExportSessionResult, SessionExportError> {
        let options = options.unwrap_or_default();
        let session_dir = self.session_dir(&payload.session_id);

        if !self.fs.exists(&session_dir) {
            return Err(SessionExportError::NotFound(
                session_dir.display().to_string(),
            ));
        }

        // Collect session files.
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        let mut total_size: u64 = 0;

        for entry in self.fs.iterdir(&session_dir).map_err(fs_err)? {
            if !entry.is_file {
                continue;
            }
            let name = entry.name;
            let data = self.fs.read_bytes(Path::new(&entry.path)).map_err(fs_err)?;
            total_size += data.len() as u64;
            entries.push((name, data));
        }

        // Optionally include the global log.
        let global_log_path = if payload.include_global_log == Some(true) {
            let log_path = self.global_log_path();
            if self.fs.exists(&log_path) {
                let data = self.fs.read_bytes(&log_path).map_err(fs_err)?;
                total_size += data.len() as u64;
                entries.push(("global.log".to_string(), data));
                Some(log_path.display().to_string())
            } else {
                None
            }
        } else {
            None
        };

        // Optionally include the desktop log.
        let desktop_log_path = if payload.include_desktop_log == Some(true) {
            let log_path = self.desktop_log_path();
            if self.fs.exists(&log_path) {
                let data = self.fs.read_bytes(&log_path).map_err(fs_err)?;
                total_size += data.len() as u64;
                entries.push(("desktop.log".to_string(), data));
                Some(log_path.display().to_string())
            } else {
                None
            }
        } else {
            None
        };

        // Optionally include the web log.
        let web_log_path = if let Some(ref web_log) = options.web_log {
            let log_path = PathBuf::from(web_log);
            if self.fs.exists(&log_path) {
                let data = self.fs.read_bytes(&log_path).map_err(fs_err)?;
                total_size += data.len() as u64;
                entries.push(("web.log".to_string(), data));
                Some(web_log.clone())
            } else {
                None
            }
        } else {
            None
        };

        // Enforce size limit.
        if let Some(limit) = options.max_archive_bytes
            && total_size > limit
        {
            return Err(SessionExportError::TooLarge {
                size: total_size,
                limit,
            });
        }

        // Build the manifest.
        let (session_first_activity, session_last_activity, title) =
            self.extract_session_metadata(&session_dir);

        let manifest = ExportSessionManifest {
            session_id: payload.session_id.clone(),
            exported_at: Utc::now(),
            kimi_code_version: payload.version.clone(),
            wire_protocol_version: WIRE_PROTOCOL_VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            runtime_version: Self::runtime_version(),
            session_first_activity,
            session_last_activity,
            title,
            workspace_dir: std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string()),
            session_log_path: Some(session_dir.display().to_string()),
            global_log_path,
            desktop_log_path,
            web_log_path,
            desktop_version: payload.desktop_version.clone(),
            install_source: payload.install_source.clone(),
            shell_env: payload.shell_env.clone(),
        };

        // Serialise manifest and add it as an entry. The failure is an
        // invalid-manifest condition, so wrap it as an I/O error carrying the
        // serde message (there is no dedicated JSON variant).
        let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(|e| {
            SessionExportError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("manifest serialisation failed: {e}"),
            ))
        })?;
        entries.push((MANIFEST_ENTRY_NAME.to_string(), manifest_json));

        // Determine the output path.
        let output_path = payload
            .output_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_output_path(&payload.session_id));

        // Ensure the parent directory exists.
        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
            && !self.fs.exists(parent)
        {
            self.fs.mkdir(parent, true).map_err(fs_err)?;
        }

        // Create the zip archive.
        let entry_names = entries
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();

        self.write_zip(&output_path, &entries)?;

        Ok(ExportSessionResult {
            zip_path: output_path.display().to_string(),
            entries: entry_names,
            session_dir: session_dir.display().to_string(),
            manifest,
        })
    }

    /// Write a list of (name, data) entries to a zip file at the given path.
    ///
    /// The archive is assembled entirely in memory and persisted through a
    /// single [`SyncFs::write_bytes`] call. Trade-off (accepted, T13 J4):
    /// the full zip buffer plus the already-collected entry payloads are held
    /// in memory simultaneously, so peak usage grows with archive size —
    /// see the module docs and ROADMAP §5 for the streaming follow-up.
    fn write_zip(
        &self,
        path: &Path,
        entries: &[(String, Vec<u8>)],
    ) -> Result<(), SessionExportError> {
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        for (name, data) in entries {
            zip.start_file(name, options)?;
            zip.write_all(data)?;
        }

        let archive = zip.finish()?.into_inner();

        self.fs.write_bytes(path, &archive).map_err(fs_err)?;
        Ok(())
    }

    /// Extract metadata (first activity, last activity, title) from the
    /// session's `state.json` file, if present.
    fn extract_session_metadata(
        &self,
        session_dir: &Path,
    ) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<String>) {
        let state_path = session_dir.join("state.json");
        let Ok(contents) = self.fs.read_text(&state_path) else {
            return (None, None, None);
        };
        let Ok(json): Result<serde_json::Value, _> = serde_json::from_str(&contents) else {
            return (None, None, None);
        };

        let first_activity = json
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let last_activity = json
            .get("updated_at")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));

        let title = json
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        (first_activity, last_activity, title)
    }

    /// Compute the desktop log file path based on the current OS.
    fn desktop_log_path(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        if let Some(home) = &self.user_home {
            return home.join("Library/Logs/mirri-code/desktop.log");
        }
        #[cfg(target_os = "linux")]
        if let Some(home) = &self.user_home {
            return home.join(".local/share/mirri-code/logs/desktop.log");
        }
        #[cfg(target_os = "windows")]
        if let Some(home) = &self.user_home {
            return home.join("AppData/Roaming/mirri-code/logs/desktop.log");
        }
        // Fallback (no known user home — the injected Mirri-home constructor):
        // keep the log inside the Mirri home.
        self.home_dir.join(LOGS_SUBDIR).join("desktop.log")
    }

    /// Get the runtime version string.
    fn runtime_version() -> String {
        // Report the Rust compiler version used to build this crate.
        // This is a compile-time constant, not a runtime call.
        format!("rustc {}", env!("CARGO_PKG_VERSION"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create a session directory with the given files.
    fn make_session_dir(home: &TempDir, session_id: &str, files: &[(&str, &str)]) -> PathBuf {
        let session_dir = home.path().join(SESSIONS_SUBDIR).join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        for (name, contents) in files {
            fs::write(session_dir.join(name), contents).unwrap();
        }
        session_dir
    }

    /// Read back the entry names from a zip archive.
    fn read_zip_entries(path: &str) -> Vec<String> {
        let file = fs::File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    /// Read the manifest.json from a zip archive.
    fn read_manifest_from_zip(path: &str) -> ExportSessionManifest {
        let file = fs::File::open(path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut entry = archive.by_name(MANIFEST_ENTRY_NAME).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn should_build_manifest_with_all_fields_when_exporting_session() {
        // Given a session directory with state.json and wire.jsonl
        let home = TempDir::new().unwrap();
        let state_json = serde_json::json!({
            "title": "My Session",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-02T12:00:00Z",
        })
        .to_string();
        make_session_dir(
            &home,
            "sess-1",
            &[
                ("state.json", &state_json),
                ("wire.jsonl", "{\"type\":\"user\"}"),
            ],
        );

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-1".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0.0".into(),
            desktop_version: Some("0.5.0".into()),
            install_source: Some("npm".into()),
            shell_env: Some(ShellEnvironment {
                term: Some("xterm-256color".into()),
                term_program: Some("iTerm.app".into()),
                term_program_version: Some("3.4.0".into()),
                multiplexer: None,
                shell: Some("/bin/zsh".into()),
            }),
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the manifest has all expected fields
        assert_eq!(result.manifest.session_id, "sess-1");
        assert_eq!(result.manifest.kimi_code_version, "1.0.0");
        assert_eq!(result.manifest.wire_protocol_version, "2.0");
        assert_eq!(result.manifest.desktop_version.as_deref(), Some("0.5.0"));
        assert_eq!(result.manifest.install_source.as_deref(), Some("npm"));
        assert_eq!(result.manifest.title.as_deref(), Some("My Session"));
        assert!(result.manifest.session_first_activity.is_some());
        assert!(result.manifest.session_last_activity.is_some());
        assert!(result.manifest.shell_env.is_some());
        assert_eq!(
            result.manifest.shell_env.as_ref().unwrap().term.as_deref(),
            Some("xterm-256color")
        );
    }

    #[test]
    fn should_include_all_session_files_when_exporting_session() {
        // Given a session directory with multiple files
        let home = TempDir::new().unwrap();
        make_session_dir(
            &home,
            "sess-2",
            &[
                ("state.json", "{\"title\":\"test\"}"),
                ("wire.jsonl", "line1\nline2\n"),
                ("snapshot.json", "{\"state\":\"saved\"}"),
            ],
        );

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-2".into(),
            output_path: Some(home.path().join("out.zip").display().to_string()),
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the zip contains all session files plus manifest.json
        assert!(result.entries.contains(&"state.json".to_string()));
        assert!(result.entries.contains(&"wire.jsonl".to_string()));
        assert!(result.entries.contains(&"snapshot.json".to_string()));
        assert!(result.entries.contains(&"manifest.json".to_string()));
        assert_eq!(result.entries.len(), 4);

        // And the zip file actually exists
        assert!(PathBuf::from(&result.zip_path).exists());

        // And the zip file contains the same entries
        let zip_entries = read_zip_entries(&result.zip_path);
        assert!(zip_entries.contains(&"state.json".to_string()));
        assert!(zip_entries.contains(&"wire.jsonl".to_string()));
        assert!(zip_entries.contains(&"snapshot.json".to_string()));
        assert!(zip_entries.contains(&"manifest.json".to_string()));
    }

    #[test]
    fn should_return_not_found_when_session_directory_does_not_exist() {
        // Given a home directory with no sessions
        let home = TempDir::new().unwrap();
        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "nonexistent".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None);

        // Then it returns NotFound error
        assert!(matches!(result, Err(SessionExportError::NotFound(_))));
    }

    #[test]
    fn should_return_too_large_when_files_exceed_max_archive_bytes() {
        // Given a session directory with a file larger than the limit
        let home = TempDir::new().unwrap();
        let big_data = "X".repeat(10_000);
        make_session_dir(&home, "sess-big", &[("big.txt", &big_data)]);

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-big".into(),
            output_path: Some(home.path().join("out.zip").display().to_string()),
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };
        let options = ExportSessionOptions {
            web_log: None,
            max_archive_bytes: Some(100),
        };

        // When exporting with a low size limit
        let result = exporter.export(payload, Some(options));

        // Then it returns TooLarge error
        assert!(matches!(
            result,
            Err(SessionExportError::TooLarge { size, limit })
                if size > 100 && limit == 100
        ));
    }

    #[test]
    fn should_write_manifest_json_when_exporting_session() {
        // Given a session directory
        let home = TempDir::new().unwrap();
        make_session_dir(&home, "sess-3", &[("wire.jsonl", "{}")]);

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-3".into(),
            output_path: Some(home.path().join("export.zip").display().to_string()),
            include_global_log: None,
            include_desktop_log: None,
            version: "2.0.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the manifest.json inside the zip is valid and has the right session id
        let manifest = read_manifest_from_zip(&result.zip_path);
        assert_eq!(manifest.session_id, "sess-3");
        assert_eq!(manifest.kimi_code_version, "2.0.0");
        assert_eq!(manifest.wire_protocol_version, "2.0");
    }

    #[test]
    fn should_include_global_log_when_requested_and_present() {
        // Given a session directory and a global log file
        let home = TempDir::new().unwrap();
        make_session_dir(&home, "sess-4", &[("wire.jsonl", "{}")]);

        let logs_dir = home.path().join(LOGS_SUBDIR);
        fs::create_dir_all(&logs_dir).unwrap();
        fs::write(logs_dir.join("global.log"), "global log line\n").unwrap();

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-4".into(),
            output_path: Some(home.path().join("out.zip").display().to_string()),
            include_global_log: Some(true),
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the archive contains global.log and the manifest references it
        assert!(result.entries.contains(&"global.log".to_string()));
        assert!(result.manifest.global_log_path.is_some());
    }

    #[test]
    fn should_extract_title_and_timestamps_when_session_has_state_json() {
        // Given a session directory with a state.json containing metadata
        let home = TempDir::new().unwrap();
        let state_json = serde_json::json!({
            "title": "Debugging Session",
            "created_at": "2024-06-01T08:00:00Z",
            "updated_at": "2024-06-01T10:30:00Z",
        })
        .to_string();
        make_session_dir(&home, "sess-meta", &[("state.json", &state_json)]);

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-meta".into(),
            output_path: Some(home.path().join("out.zip").display().to_string()),
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the manifest has the extracted metadata
        assert_eq!(result.manifest.title.as_deref(), Some("Debugging Session"));
        let first = result.manifest.session_first_activity.unwrap();
        let last = result.manifest.session_last_activity.unwrap();
        assert_eq!(first.to_rfc3339(), "2024-06-01T08:00:00+00:00");
        assert_eq!(last.to_rfc3339(), "2024-06-01T10:30:00+00:00");
    }

    #[test]
    fn should_use_default_output_path_when_output_path_is_none() {
        // Given a session directory
        let home = TempDir::new().unwrap();
        make_session_dir(&home, "sess-default", &[("wire.jsonl", "{}")]);

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-default".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting without an explicit output path
        let result = exporter.export(payload, None).unwrap();

        // Then the zip was written to the default exports path
        let expected = home.path().join(EXPORTS_SUBDIR).join("sess-default.zip");
        assert_eq!(result.zip_path, expected.display().to_string());
        assert!(expected.exists());
    }

    #[test]
    fn should_write_under_the_mirri_home_directly_when_constructed_with_mirri_home() {
        // Given an injected Mirri home (not a user home) holding a session
        let mirri_home = TempDir::new().unwrap();
        let session_dir = mirri_home
            .path()
            .join(SESSIONS_SUBDIR)
            .join("sess-injected");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("wire.jsonl"), "{}").unwrap();

        let exporter = SessionExporter::with_mirri_home(mirri_home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-injected".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting without an explicit output path
        let result = exporter.export(payload, None).unwrap();

        // Then the zip was written under the injected home's exports dir —
        // NOT nested under a second `.mirri-code` level
        let expected = mirri_home
            .path()
            .join(EXPORTS_SUBDIR)
            .join("sess-injected.zip");
        assert_eq!(result.zip_path, expected.display().to_string());
        assert!(expected.exists());
    }

    #[test]
    fn should_roundtrip_all_fields_when_shell_environment_is_serialized() {
        // Given a ShellEnvironment constructed with explicit values
        let env = ShellEnvironment {
            term: Some("xterm-256color".into()),
            term_program: Some("iTerm.app".into()),
            term_program_version: Some("3.5.0".into()),
            multiplexer: Some("tmux".into()),
            shell: Some("/bin/zsh".into()),
        };

        // When serializing and deserializing
        let json = serde_json::to_string(&env).unwrap();
        let decoded: ShellEnvironment = serde_json::from_str(&json).unwrap();

        // Then all fields survive the round-trip
        assert_eq!(decoded.term.as_deref(), Some("xterm-256color"));
        assert_eq!(decoded.term_program.as_deref(), Some("iTerm.app"));
        assert_eq!(decoded.term_program_version.as_deref(), Some("3.5.0"));
        assert_eq!(decoded.multiplexer.as_deref(), Some("tmux"));
        assert_eq!(decoded.shell.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn should_default_all_fields_to_none_when_shell_environment_is_defaulted() {
        // Given a default ShellEnvironment
        let env = ShellEnvironment::default();

        // When inspecting its fields
        // Then all are None
        assert!(env.term.is_none());
        assert!(env.term_program.is_none());
        assert!(env.term_program_version.is_none());
        assert!(env.multiplexer.is_none());
        assert!(env.shell.is_none());
    }

    #[test]
    fn should_serialize_manifest_and_shell_env_with_camelcase_wire_keys() {
        // Given a fully populated manifest
        let manifest = ExportSessionManifest {
            session_id: "sess-wire".into(),
            exported_at: Utc::now(),
            kimi_code_version: "1.0.0".into(),
            wire_protocol_version: "2.0".into(),
            os: "macos".into(),
            runtime_version: "rustc 1.0".into(),
            session_first_activity: None,
            session_last_activity: None,
            title: Some("wire shape".into()),
            workspace_dir: Some("/tmp/ws".into()),
            session_log_path: Some("/tmp/ws/sessions/sess-wire".into()),
            global_log_path: None,
            desktop_log_path: None,
            web_log_path: None,
            desktop_version: None,
            install_source: None,
            shell_env: Some(ShellEnvironment {
                term: Some("xterm-256color".into()),
                term_program: Some("iTerm.app".into()),
                term_program_version: Some("3.5.0".into()),
                multiplexer: Some("tmux".into()),
                shell: Some("/bin/zsh".into()),
            }),
        };

        // When serialising to the wire / manifest.json representation
        let value = serde_json::to_value(&manifest).expect("manifest must serialise to JSON");

        // Then the manifest keys are exactly the 17 v2 camelCase contract
        // keys (with nodejsVersion, never runtimeVersion)
        let mut manifest_keys: Vec<&str> = value
            .as_object()
            .expect("manifest serialises to an object")
            .keys()
            .map(String::as_str)
            .collect();
        manifest_keys.sort_unstable();
        assert_eq!(
            manifest_keys,
            vec![
                "desktopLogPath",
                "desktopVersion",
                "exportedAt",
                "globalLogPath",
                "installSource",
                "kimiCodeVersion",
                "nodejsVersion",
                "os",
                "sessionFirstActivity",
                "sessionId",
                "sessionLastActivity",
                "sessionLogPath",
                "shellEnv",
                "title",
                "webLogPath",
                "wireProtocolVersion",
                "workspaceDir",
            ]
        );

        // And the nested shell environment uses camelCase keys too
        let shell_env = &value["shellEnv"];
        let mut shell_keys: Vec<&str> = shell_env
            .as_object()
            .expect("shellEnv serialises to an object")
            .keys()
            .map(String::as_str)
            .collect();
        shell_keys.sort_unstable();
        assert_eq!(
            shell_keys,
            vec![
                "multiplexer",
                "shell",
                "term",
                "termProgram",
                "termProgramVersion"
            ]
        );
    }

    // -------------------------------------------------------------------------
    // Zero-IO T13c-2: injected SyncFs double
    // -------------------------------------------------------------------------

    /// In-memory [`SyncFs`] double: a flat virtual file tree plus a log of
    /// every `write_bytes` call (the zip bytes included).
    struct InMemoryFs {
        files: std::sync::Mutex<HashMap<PathBuf, Vec<u8>>>,
        writes: std::sync::Mutex<Vec<(PathBuf, Vec<u8>)>>,
    }

    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: std::sync::Mutex::new(HashMap::new()),
                writes: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// Stage a virtual file.
        fn add_file(&self, path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) {
            self.files
                .lock()
                .unwrap()
                .insert(path.into(), contents.into());
        }

        /// The bytes written through `write_bytes` to `path`.
        fn written(&self, path: &Path) -> Vec<u8> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| p == path)
                .expect("path must have been written")
                .1
                .clone()
        }

        /// How many `write_bytes` calls targeted `path`.
        fn write_count(&self, path: &Path) -> usize {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p == path)
                .count()
        }
    }

    impl SyncFs for InMemoryFs {
        fn read_bytes(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such file: {}", path.display()))
        }

        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            let bytes = self.read_bytes(path)?;
            String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("invalid utf-8: {e}"))
        }

        fn write_bytes(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            self.writes
                .lock()
                .unwrap()
                .push((path.to_path_buf(), bytes.to_vec()));
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            let files = self.files.lock().unwrap();
            files.contains_key(path) || files.keys().any(|p| p.starts_with(path))
        }

        fn iterdir(&self, path: &Path) -> anyhow::Result<Vec<mirri_os::DirEntry>> {
            let files = self.files.lock().unwrap();
            Ok(files
                .keys()
                .filter(|p| p.parent() == Some(path))
                .map(|p| mirri_os::DirEntry {
                    name: p
                        .file_name()
                        .expect("staged paths end in a file name")
                        .to_string_lossy()
                        .into_owned(),
                    path: p.display().to_string(),
                    is_dir: false,
                    is_file: true,
                })
                .collect())
        }

        fn mkdir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
            // Directories are implicit in the flat file map (the marker the
            // parent-creation branch would insert is never read back).
            Ok(())
        }
    }

    /// should produce a valid zip archive through the injected fs when
    /// exporting with an in-memory double (T-13c2-1)
    #[test]
    fn should_write_zip_bytes_and_entry_list_through_injected_fs_when_exporting_with_double() {
        // Given a virtual home holding a session and the global log — no
        // real disk access anywhere in this test
        let home = PathBuf::from("/virtual/home");
        let fs = std::sync::Arc::new(InMemoryFs::new());
        fs.add_file(
            home.join("sessions/sess-mem/state.json"),
            r#"{"title":"Memory Session","created_at":"2024-05-01T00:00:00Z","updated_at":"2024-05-02T00:00:00Z"}"#,
        );
        fs.add_file(
            home.join("sessions/sess-mem/wire.jsonl"),
            "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
        );
        fs.add_file(home.join("logs/global.log"), "global log line\n");

        let exporter = SessionExporter::with_fs(
            home.clone(),
            std::sync::Arc::clone(&fs) as std::sync::Arc<dyn SyncFs>,
        );
        let payload = ExportSessionPayload {
            session_id: "sess-mem".into(),
            output_path: None,
            include_global_log: Some(true),
            include_desktop_log: None,
            version: "1.0.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting through the in-memory double
        let result = exporter.export(payload, None).unwrap();

        // Then the zip is persisted with a single write through the double
        // (the in-memory-archive guarantee: one write_bytes, no streaming
        // File::create)
        let zip_path = home.join(EXPORTS_SUBDIR).join("sess-mem.zip");
        assert_eq!(result.zip_path, zip_path.display().to_string());
        assert_eq!(fs.write_count(&zip_path), 1);

        // And the written bytes are a real zip: they open with the PK
        // local-file-header magic
        let bytes = fs.written(&zip_path);
        assert_eq!(
            &bytes[..4],
            b"PK\x03\x04",
            "zip must start with the PK header"
        );

        // And the entry list matches the session files plus the optional
        // global log and the manifest
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["global.log", "manifest.json", "state.json", "wire.jsonl"]
        );

        // And entry contents survive byte-for-byte through the round-trip
        let mut wire_contents = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("wire.jsonl").unwrap(),
            &mut wire_contents,
        )
        .unwrap();
        assert_eq!(
            wire_contents,
            "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n"
        );

        // And the manifest extracted its metadata from the virtual state.json
        assert_eq!(result.manifest.title.as_deref(), Some("Memory Session"));
        assert!(result.manifest.global_log_path.is_some());
    }

    /// Create a rust-written session under the v3 storage root
    /// (`v3_sessions/<bucket>/<session_id>/`).
    fn make_v3_session_dir(
        home: &TempDir,
        bucket: &str,
        session_id: &str,
        files: &[(&str, &str)],
    ) -> PathBuf {
        let session_dir = home
            .path()
            .join(V3_SESSIONS_SUBDIR)
            .join(bucket)
            .join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        for (name, contents) in files {
            fs::write(session_dir.join(name), contents).unwrap();
        }
        session_dir
    }

    #[test]
    fn should_export_rust_written_sessions_from_the_v3_root() {
        // Given a session bucketed under the v3 storage root (the rust
        // engine's post-cut layout), with no legacy-root copy
        let home = TempDir::new().unwrap();
        make_v3_session_dir(
            &home,
            "wd_v3_bucket",
            "sess-v3",
            &[("state.json", "{\"title\":\"v3 session\"}")],
        );

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-v3".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the session is found in the v3 root and the archive is built
        assert!(result.entries.contains(&"state.json".to_string()));
        assert!(result.entries.contains(&"manifest.json".to_string()));
        assert_eq!(result.manifest.session_id, "sess-v3");
        assert_eq!(result.manifest.title.as_deref(), Some("v3 session"));
    }

    #[test]
    fn should_prefer_the_v3_root_over_the_legacy_root_for_the_same_session_id() {
        // Given the same session id bucketed in BOTH roots (a --copy
        // leftover), with distinguishable state files
        let home = TempDir::new().unwrap();
        make_v3_session_dir(
            &home,
            "wd_v3_bucket",
            "sess-both",
            &[("state.json", "{\"title\":\"from v3\"}")],
        );
        make_session_dir(
            &home,
            "sess-both",
            &[("state.json", "{\"title\":\"from legacy\"}")],
        );

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-both".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the v3 root's copy wins (the post-migration location)
        assert_eq!(result.manifest.title.as_deref(), Some("from v3"));
    }

    #[test]
    fn should_still_export_pre_migration_sessions_from_the_legacy_root() {
        // Given a TS-era session under the legacy `sessions/` root with no
        // v3 root at all
        let home = TempDir::new().unwrap();
        make_session_dir(
            &home,
            "sess-legacy",
            &[("state.json", "{\"title\":\"legacy session\"}")],
        );

        let exporter = SessionExporter::with_mirri_home(home.path());
        let payload = ExportSessionPayload {
            session_id: "sess-legacy".into(),
            output_path: None,
            include_global_log: None,
            include_desktop_log: None,
            version: "1.0.0".into(),
            desktop_version: None,
            install_source: None,
            shell_env: None,
        };

        // When exporting
        let result = exporter.export(payload, None).unwrap();

        // Then the pre-migration-era fallback finds the session
        assert_eq!(result.manifest.title.as_deref(), Some("legacy session"));
    }
}
