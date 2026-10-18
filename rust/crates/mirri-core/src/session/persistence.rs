//! Session persistence — reading and writing session state to disk.
//!
//! Each session is persisted as three files under the sessions directory:
//!
//! - `state.json` — session metadata (title, timestamps, tags, and the TS
//!   fields such as `workDir`), written atomically via
//!   [`mirri_persistence::AtomicDocument`].
//! - `wire.jsonl` — the append-only wire op log, managed by
//!   [`mirri_persistence::JsonlAppendLog`].
//! - `snapshot.json` — optional [`AgentState`] checkpoint written on session
//!   close (via [`mirri_persistence::SnapshotStore`]) so a later resume can
//!   replay the wire log from the checkpoint instead of from the beginning.
//!
//! The directory layout is:

//! ```text
//! <sessions_dir>/
//!   <workspace_key>/          (TS parity bucket; only when a key is set)
//!     <session_id>/
//!       state.json
//!       wire.jsonl
//!       snapshot.json   (optional)
//! ```
//!
//! A persistence instance created with a workspace key (see
//! [`SessionPersistence::with_workspace_key`]) nests every session under
//! `<sessions_dir>/<workspace_key>/<session_id>/` — the same per-workspace
//! bucket the TS kernel derives from its `workDir` (the key is produced by
//! [`crate::session::encode_work_dir_key`]). An instance without a key
//! (the legacy form, kept for directly-constructed test instances and the
//! CLI) writes the flat `<sessions_dir>/<session_id>/` layout. Reads
//! (`list_persisted_sessions`) understand BOTH layouts via structural
//! detection, and [`SessionPersistence::migrate_legacy_flat_sessions`]
//! moves flat sessions into their workspace buckets once.
//!
//! [`SessionPersistence`] provides path computation, state save/load, file
//! cleanup, and listing of persisted sessions. It does **not** own or manage
//! the wire log writer handle — that is the responsibility of the session
//! manager at resume time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use mirri_os::{DynFileSystem, SyncFs};
use mirri_persistence::AtomicDocument;

use crate::scope::{SessionId, SessionMetadata};

use super::error::{SessionError, SessionResult};
use super::workdir_key::encode_work_dir_key;

/// Serializable representation of session metadata for `state.json`.
///
/// [`SessionMetadata`] in the scope layer does not derive `Serialize`/
/// `Deserialize` (it carries a `HashSet` which is fine for serde, but the
/// scope layer intentionally keeps persistence concerns out of the config
/// types). This struct acts as the on-disk schema, and conversion is
/// lossless.
///
/// The schema is cross-compatible with the TS kernel's `state.json`
/// (camelCase `archived`, `customTitle`, `isCustomTitle`, `lastPrompt`,
/// `workDir`, `createdAt`, `updatedAt`, `forkedFrom`, `cacheRootId`,
/// `agents`): the TS spellings are accepted via `alias` on deserialise,
/// and every field defaults so a TS-written file missing Rust-only fields
/// still parses.
/// The timestamps accept BOTH engines' value spellings — the TS kernel
/// writes epoch-milliseconds integers (`"createdAt": 1788494508725`), the
/// Rust kernel writes RFC 3339 strings — via
/// [`deserialize_epoch_or_rfc3339`]; the write side stays RFC 3339.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PersistedSessionMetadata {
    /// Layout version written by the producing kernel: TS writes `2`,
    /// rust writes `3` (the v3_sessions storage-root era). Absent on
    /// state.json files predating the version stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u8>,
    #[serde(default)]
    title: String,
    #[serde(
        default,
        alias = "createdAt",
        deserialize_with = "deserialize_epoch_or_rfc3339"
    )]
    created_at: DateTime<Utc>,
    #[serde(
        default,
        alias = "updatedAt",
        deserialize_with = "deserialize_epoch_or_rfc3339"
    )]
    updated_at: DateTime<Utc>,
    #[serde(default)]
    tags: Vec<String>,
    /// Whether the session was archived (TS field): set by the embedded
    /// `session.archive` / `session.restore` lifecycle (TS parity) and
    /// used to filter archived sessions out of default listings.
    #[serde(default)]
    archived: bool,
    /// User-set title distinct from the generated one (TS `customTitle`).
    #[serde(default, alias = "customTitle")]
    custom_title: Option<String>,
    /// Whether `title` is user-set rather than generated (TS `isCustomTitle`).
    #[serde(default, alias = "isCustomTitle")]
    is_custom_title: bool,
    /// The last prompt sent in the session (TS `lastPrompt`).
    #[serde(default, alias = "lastPrompt")]
    last_prompt: Option<String>,
    /// The working directory the session was rooted at (TS `workDir`; the
    /// on-disk TS kernel also spells it as `cwd`).
    #[serde(default, alias = "workDir", alias = "cwd")]
    work_dir: Option<String>,
    /// Free-form TS extension payload (TS `custom`).
    #[serde(default, alias = "custom", skip_serializing_if = "Option::is_none")]
    custom: Option<serde_json::Value>,
    /// The session this one was forked from (TS `forkedFrom`).
    #[serde(default, alias = "forkedFrom", skip_serializing_if = "Option::is_none")]
    forked_from: Option<String>,
    /// Root session id of the fork chain (TS `cacheRootId`): the provider
    /// prompt-cache key lineage minted by the fork pipeline.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "cacheRootId"
    )]
    cache_root_id: Option<String>,
    /// TS agent list (unused by the Rust kernel but preserved). Absent on
    /// a manifest-less session — a fork of such a source mints none (F5).
    #[serde(default, alias = "agents", skip_serializing_if = "Option::is_none")]
    agents: Option<serde_json::Value>,
}

impl From<&SessionMetadata> for PersistedSessionMetadata {
    fn from(metadata: &SessionMetadata) -> Self {
        Self {
            // The rust kernel stamps the v3_sessions storage-root era.
            version: Some(3),
            title: metadata.title.clone(),
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            tags: metadata.tags.iter().cloned().collect(),
            // `archived`, `work_dir` and `last_prompt` are shared with the
            // scope layer and round-trip; `forked_from` and `custom` are
            // scope-layer fields on the fork path (minted by the fork
            // pipeline) and round-trip the same way; the remaining TS-only
            // fields (`customTitle`, `agents`, …) are not part of the
            // scope-layer metadata and default to empty on write (preserved
            // on read by `set_archived`'s load-mutate-save cycle).
            archived: metadata.archived,
            work_dir: metadata.work_dir.clone(),
            custom_title: None,
            is_custom_title: metadata.is_custom_title,
            last_prompt: metadata.last_prompt.clone(),
            custom: metadata
                .custom
                .clone()
                .map(serde_json::Value::Object)
                .filter(|value| !value.as_object().is_some_and(|m| m.is_empty())),
            forked_from: metadata.forked_from.clone(),
            cache_root_id: metadata.cache_root_id.clone(),
            agents: None,
        }
    }
}

impl From<PersistedSessionMetadata> for SessionMetadata {
    fn from(persisted: PersistedSessionMetadata) -> Self {
        Self {
            title: persisted.title,
            is_custom_title: persisted.is_custom_title,
            created_at: persisted.created_at,
            updated_at: persisted.updated_at,
            tags: persisted.tags.into_iter().collect(),
            archived: persisted.archived,
            work_dir: persisted.work_dir,
            last_prompt: persisted.last_prompt,
            forked_from: persisted.forked_from,
            cache_root_id: persisted.cache_root_id,
            custom: persisted.custom.and_then(|value| match value {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            }),
        }
    }
}

/// Deserialise a `state.json` timestamp from either engine's value spelling:
/// the TS kernel writes epoch-milliseconds integers
/// (`"createdAt": 1788494508725`), the Rust kernel writes RFC 3339 strings.
/// Anything else is a serde error (which `load_state` degrades to default
/// metadata, and `list_persisted_sessions` skips with a warning). The write
/// side stays RFC 3339 — see [`PersistedSessionMetadata`].
fn deserialize_epoch_or_rfc3339<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .ok_or_else(|| {
                D::Error::custom(format!("invalid epoch-milliseconds timestamp: {number}"))
            }),
        serde_json::Value::String(text) => DateTime::parse_from_rfc3339(&text)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| D::Error::custom(format!("invalid RFC 3339 timestamp {text:?}: {e}"))),
        other => Err(D::Error::custom(format!(
            "timestamp must be an epoch-milliseconds integer or an RFC 3339 string, got {other}"
        ))),
    }
}

/// Information about a session discovered on disk by
/// [`SessionPersistence::list_persisted_sessions`].
#[derive(Debug, Clone)]
pub struct PersistedSessionInfo {
    /// The session ID (directory name under `sessions_dir`).
    pub session_id: String,
    /// Size of the `wire.jsonl` file in bytes, or `0` if the file does not
    /// exist.
    pub wire_log_size: u64,
    /// Last modification time of `state.json`, or `None` if the file does
    /// not exist.
    pub state_modified: Option<DateTime<Utc>>,
    /// The working directory recorded in `state.json` (TS `workDir`), when
    /// present.
    pub work_dir: Option<String>,
    /// Whether the session is archived (the `archived` flag in `state.json`).
    /// Default listings exclude archived sessions (TS parity); callers opt in
    /// with `include_archived = true`.
    pub archived: bool,
}

/// Manages on-disk persistence of session state and wire logs.
///
/// All path computation is deterministic: given a `sessions_dir` and a
/// session ID, the paths for `state.json` and `wire.jsonl` are fully
/// determined. The manager creates parent directories as needed. Filesystem
/// operations (directory enumeration, stat, removal) go through the
/// injected [`DynFileSystem`] provider (zero-IO T13b; the engine default is
/// `LocalFs`); `state.json` reads and writes remain on the shared
/// [`AtomicDocument`] primitive.
#[derive(Clone)]
pub struct SessionPersistence {
    /// Root directory under which per-session subdirectories are created.
    sessions_dir: PathBuf,
    /// The TS-parity workspace bucket (`wd_<slug>_<hash12>`, derived via
    /// [`crate::session::encode_work_dir_key`]). `Some` nests every session
    /// under `<sessions_dir>/<key>/`; `None` keeps the legacy flat
    /// `<sessions_dir>/<session_id>/` form (directly-constructed test
    /// instances and the CLI).
    workspace_key: Option<String>,
    /// Injected filesystem provider (async hosts → the object-safe
    /// `DynFileSystem` mirror of `FileSystem`).
    io: Arc<dyn DynFileSystem>,
    /// Our own last-written `state.json` snapshot per session — the common
    /// ancestor of the ADR-003-R1 three-way merge (in-memory only; the
    /// shared map is the price of [`SessionPersistence`] being `Clone`, the
    /// per-save mutex section keeps the read-merge-write steps contiguous
    /// within one process).
    last_written: Arc<Mutex<HashMap<String, PersistedSessionMetadata>>>,
}

impl SessionPersistence {
    /// Create a new persistence manager rooted at `sessions_dir`, using the
    /// legacy flat layout (`<sessions_dir>/<session_id>/`).
    pub fn new(sessions_dir: PathBuf, io: Arc<dyn DynFileSystem>) -> Self {
        Self {
            sessions_dir,
            workspace_key: None,
            io,
            last_written: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Builder: nest every session under the workspace bucket `key`
    /// (`<sessions_dir>/<key>/<session_id>/`) — the TS kernel's per-workspace
    /// layout. The key is expected to come from
    /// [`crate::session::encode_work_dir_key`] so a rust-written session
    /// lands in exactly the bucket the TS kernel computes for the same
    /// workspace.
    pub fn with_workspace_key(mut self, key: String) -> Self {
        self.workspace_key = Some(key);
        self
    }

    /// The root sessions directory.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// The workspace bucket key, when one is set (see
    /// [`Self::with_workspace_key`]).
    pub fn workspace_key(&self) -> Option<&str> {
        self.workspace_key.as_deref()
    }

    /// Compute the per-session directory:
    /// `<sessions_dir>/<workspace_key>/<session_id>/` when a workspace key
    /// is set (the TS bucketed layout), `<sessions_dir>/<session_id>/`
    /// otherwise (the legacy flat form).
    pub fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        match &self.workspace_key {
            Some(key) => self.sessions_dir.join(key).join(session_id.as_str()),
            None => self.sessions_dir.join(session_id.as_str()),
        }
    }

    /// The path to the `wire.jsonl` file for the given session.
    ///
    /// This method only computes the path; it does not create the file or
    /// any parent directories.
    pub fn wire_log_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join("wire.jsonl")
    }

    /// The per-subagent journal path (B4 T7/T8, D-55):
    /// `<session_dir>/agents/<agent_id>/wire.jsonl`. Agent ids are
    /// host-minted (`agent-<uuid>`), so the path needs no further
    /// sanitization.
    pub fn agent_wire_log_path(&self, session_id: &SessionId, agent_id: &str) -> PathBuf {
        self.session_dir(session_id)
            .join("agents")
            .join(agent_id)
            .join("wire.jsonl")
    }

    /// The path to the `state.json` file for the given session.
    ///
    /// This method only computes the path; it does not create the file or
    /// any parent directories.
    pub fn state_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join("state.json")
    }

    /// The path to the `snapshot.json` checkpoint file for the given session.
    ///
    /// The snapshot stores a folded [`mirri_proto::AgentState`] plus the op
    /// index it covers; a resume replays only the wire ops after that index.
    /// This method only computes the path; it does not create the file or any
    /// parent directories.
    pub fn snapshot_path(&self, session_id: &SessionId) -> PathBuf {
        self.session_dir(session_id).join("snapshot.json")
    }

    /// Atomically save session metadata to `state.json`.
    ///
    /// The write is crash-safe: a temp file is written and synced before an
    /// atomic rename. Parent directories are created as needed.
    ///
    /// The write is also a **three-way field merge** (ADR-003-R1): when the
    /// on-disk `state.json` differs from our own last-written snapshot
    /// (i.e. another entry point wrote it since), the file is read back and
    /// merged field by field — a field we did not touch takes the disk
    /// value, a field we did change (or both changed) keeps ours (last
    /// writer wins). The merged document is what lands on disk, and it
    /// becomes the new last-written snapshot. A corrupt disk document falls
    /// back to the plain full overwrite (the "one broken copy must not
    /// spread" tolerance), with a warning.
    pub async fn save_state(
        &self,
        session_id: &SessionId,
        metadata: &SessionMetadata,
    ) -> SessionResult<()> {
        let path = self.state_path(session_id);
        let persisted = PersistedSessionMetadata::from(metadata);

        // Snapshot our own last-written state BEFORE any await: the guard
        // must not live across an await (the returned future would not be
        // `Send`). Two in-process saves racing here degrade exactly like the
        // cross-process race the merge itself resolves — the second save
        // sees either the first's ancestor or the disk it produced.
        let ancestor: Option<PersistedSessionMetadata> = {
            let last_written = self
                .last_written
                .lock()
                .map_err(|_| SessionError::StateFile("last-written map poisoned".to_string()))?;
            last_written.get(session_id.as_str()).cloned()
        };

        // Read the current disk content. A foreign write shows up as
        // "differs from our own last-written snapshot" (a by-value
        // comparison; a re-write of identical bytes is not a foreign
        // change). A missing file (first save) or a corrupt one is None.
        let theirs: Option<PersistedSessionMetadata> = match AtomicDocument::read(&path).await {
            Ok(on_disk) => on_disk.filter(|on_disk| Some(on_disk) != ancestor.as_ref()),
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    state = %path.display(),
                    %error,
                    "state.json on disk could not be read before merging; overwriting with ours"
                );
                None
            }
        };

        let to_write = match (&theirs, &ancestor) {
            // No ancestor (first save by this instance) or no foreign write
            // (disk is exactly our last-written snapshot, or absent): the
            // plain full write, behaviour unchanged.
            (_, None) | (None, _) => persisted.clone(),
            // Foreign write against a known ancestor: three-way field merge.
            (Some(theirs), Some(base)) => merge_persisted_metadata(base, &persisted, theirs),
        };

        AtomicDocument::write(&path, &to_write)
            .await
            .map_err(|e| SessionError::StateFile(e.to_string()))?;
        self.last_written
            .lock()
            .map_err(|_| SessionError::StateFile("last-written map poisoned".to_string()))?
            .insert(session_id.to_string(), to_write);
        Ok(())
    }

    /// Load session metadata from `state.json`.
    ///
    /// Returns [`SessionError::StateFile`] if the file does not exist.
    /// A file that exists but cannot be deserialised is tolerated, mirroring
    /// the TS `safeParse` behaviour: a warning is logged and default
    /// metadata is returned so a corrupt `state.json` never blocks a resume.
    pub async fn load_state(&self, session_id: &SessionId) -> SessionResult<SessionMetadata> {
        let path = self.state_path(session_id);
        let loaded: Option<PersistedSessionMetadata> = match AtomicDocument::read(&path).await {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    state = %path.display(),
                    %error,
                    "state.json could not be parsed; falling back to default metadata"
                );
                return Ok(SessionMetadata::default());
            }
        };
        match loaded {
            Some(persisted) => Ok(persisted.into()),
            None => Err(SessionError::StateFile(format!(
                "state.json not found for session {}",
                session_id
            ))),
        }
    }

    /// Delete all on-disk files for a session (the entire per-session
    /// directory).
    ///
    /// This is a best-effort operation: if the directory does not exist, it
    /// succeeds silently. Individual file deletion errors are aggregated
    /// into a single error.
    pub async fn delete_session_files(&self, session_id: &SessionId) -> SessionResult<()> {
        let dir = self.session_dir(session_id);
        if !self.io.exists(&dir).await.unwrap_or(false) {
            return Ok(());
        }
        self.io.remove_dir(&dir, true).await.map_err(|e| {
            SessionError::StateFile(format!(
                "failed to remove session dir {}: {}",
                dir.display(),
                e
            ))
        })?;
        Ok(())
    }

    /// Attempt to read [`PersistedSessionInfo`] from a session directory.
    ///
    /// Returns `None` when the directory has no readable `state.json`
    /// (missing file or unparseable content). Wire log size and state
    /// modification time are gathered from the filesystem when available.
    async fn try_read_session_info(
        &self,
        session_dir: &Path,
        session_id: &str,
    ) -> Option<PersistedSessionInfo> {
        let state_path = session_dir.join("state.json");

        let metadata: Option<PersistedSessionMetadata> =
            match AtomicDocument::read(&state_path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        state = %state_path.display(),
                        %error,
                        "skipping session with unreadable state.json"
                    );
                    return None;
                }
            };
        let metadata = match metadata {
            Some(m) => m,
            None => {
                tracing::warn!(
                    session_id = %session_id,
                    state = %state_path.display(),
                    "skipping session without state.json"
                );
                return None;
            }
        };

        let wire_log_path = session_dir.join("wire.jsonl");
        let wire_log_size = self
            .io
            .stat(&wire_log_path)
            .await
            .map(|s| s.size)
            .unwrap_or(0);

        let state_modified = self
            .io
            .stat(&state_path)
            .await
            .ok()
            .map(|s| s.mtime)
            .map(|secs| DateTime::<Utc>::from_timestamp(secs as i64, 0).unwrap_or_else(Utc::now));

        Some(PersistedSessionInfo {
            session_id: session_id.to_string(),
            wire_log_size,
            state_modified,
            work_dir: metadata.work_dir,
            archived: metadata.archived,
        })
    }

    /// Read one persisted session's full metadata from its session directory
    /// — the single-session form of the listing scan's read.
    ///
    /// The session-index read model uses this for its cold misses: a row
    /// that is missing or stale is rebuilt from the same validated disk read
    /// [`Self::list_persisted_sessions`] performs, so the index and the plain
    /// directory scan never disagree on which sessions are readable. The
    /// parse tolerance is the scan's, NOT [`Self::load_state`]'s: a missing
    /// or unparseable `state.json` yields `None` (the listing skips the
    /// session), never degraded default metadata.
    pub async fn read_persisted_summary(&self, session_id: &SessionId) -> Option<SessionMetadata> {
        match AtomicDocument::read::<PersistedSessionMetadata>(&self.state_path(session_id)).await {
            Ok(Some(persisted)) => Some(persisted.into()),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    state = %self.state_path(session_id).display(),
                    %error,
                    "session-index cold miss could not read state.json"
                );
                None
            }
        }
    }

    /// Set the archived flag on a session's persisted `state.json`.
    ///
    /// Unlike [`Self::load_state`] (which degrades a corrupt file to default
    /// metadata, the TS `safeParse` tolerance), this is an **explicit write**
    /// driven by the `session.archive` / `session.restore` lifecycle: a
    /// missing or unreadable `state.json` is an error, not a silent default.
    /// The loaded struct is written back with only the flag mutated, so the
    /// TS extra fields (`customTitle` / `lastPrompt` / `workDir` / …) survive
    /// the round-trip untouched.
    ///
    /// The write is a bare full-document write (it must keep the TS fields
    /// [`Self::save_state`] cannot carry), so it records the **read** content
    /// — not the archived-tweaked document it writes — as the merge
    /// ancestor in `last_written`. That keeps the on-disk document looking
    /// foreign to the next [`Self::save_state`] (the `archived` flag differs),
    /// forcing the ADR-003-R1 three-way merge instead of the plain-overwrite
    /// branch: a concurrent save that landed a field change while we were
    /// archiving survives (ours == base takes the disk value), instead of
    /// being clobbered back to this process's in-memory view ("被冲掉的版本").
    pub async fn set_archived(&self, session_id: &SessionId, archived: bool) -> SessionResult<()> {
        let path = self.state_path(session_id);
        let loaded: PersistedSessionMetadata = match AtomicDocument::read(&path).await {
            Ok(Some(persisted)) => persisted,
            Ok(None) => {
                return Err(SessionError::StateFile(format!(
                    "state.json not found for session {}",
                    session_id
                )));
            }
            Err(error) => {
                return Err(SessionError::StateFile(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };
        let mut persisted = loaded.clone();
        persisted.archived = archived;
        AtomicDocument::write(&path, &persisted)
            .await
            .map_err(|e| SessionError::StateFile(e.to_string()))?;
        // Ancestor registration AFTER the write: the next save_state must
        // merge against what the disk held when we read it (a foreign
        // change landed there survives), never against our in-memory view.
        self.last_written
            .lock()
            .map_err(|_| SessionError::StateFile("last-written map poisoned".to_string()))?
            .insert(session_id.to_string(), loaded);
        Ok(())
    }

    /// Read a session's TS `agents` manifest from its `state.json` (F5).
    ///
    /// The `agents` key is a TS-kernel-only field the Rust kernel
    /// preserves but never interprets (see [`PersistedSessionMetadata`]).
    /// The fork pipeline uses this to carry the manifest across a fork:
    /// the returned value is the raw JSON the source recorded — `None`
    /// when the source has no manifest (the fork then mints none either).
    /// A missing `state.json` is not an error here (the fork's other state
    /// reads already tolerate it); an unreadable one degrades to `None`
    /// with a warning, matching [`Self::load_state`]'s tolerance.
    pub async fn read_ts_agents_manifest(
        &self,
        session_id: &SessionId,
    ) -> SessionResult<Option<serde_json::Value>> {
        let path = self.state_path(session_id);
        let loaded: PersistedSessionMetadata = match AtomicDocument::read(&path).await {
            Ok(Some(persisted)) => persisted,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    state = %path.display(),
                    %error,
                    "state.json could not be read for the agents manifest; forking without one"
                );
                return Ok(None);
            }
        };
        Ok(loaded.agents)
    }

    /// Write a TS `agents` manifest into a session's `state.json` (F5).
    ///
    /// The load-mutate-save companion of [`Self::read_ts_agents_manifest`]:
    /// the forked session's minted `state.json` gets the source's manifest
    /// spliced in (member ids unchanged), so a TS reader sees the fork's
    /// member identities the same way it saw the source's. Only the
    /// `agents` key is touched — every other field of the freshly minted
    /// state survives untouched. A `None` manifest is a no-op (the fork
    /// of a manifest-less source stays manifest-less).
    pub async fn write_ts_agents_manifest(
        &self,
        session_id: &SessionId,
        agents: Option<&serde_json::Value>,
    ) -> SessionResult<()> {
        let Some(agents) = agents else {
            return Ok(());
        };
        let path = self.state_path(session_id);
        let loaded: PersistedSessionMetadata = match AtomicDocument::read(&path).await {
            Ok(Some(persisted)) => persisted,
            Ok(None) => {
                return Err(SessionError::StateFile(format!(
                    "state.json not found for session {}",
                    session_id
                )));
            }
            Err(error) => {
                return Err(SessionError::StateFile(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };
        let mut persisted = loaded;
        persisted.agents = Some(agents.clone());
        AtomicDocument::write(&path, &persisted)
            .await
            .map_err(|e| SessionError::StateFile(e.to_string()))?;
        Ok(())
    }

    /// List sessions that have a readable `state.json` under `sessions_dir`.
    ///
    /// Supports both the Rust flat layout (`<sessions_dir>/<session_id>/`)
    /// and the TS bucketed layout (`<sessions_dir>/<bucket>/<session_id>/`,
    /// where `<bucket>` is a workDir-keyed directory loosely matching the
    /// `wd_*_*` naming convention used by the TS kernel).
    ///
    /// Detection is structural: a first-level subdirectory that contains
    /// `state.json` is treated as a flat-layout session; one that does not
    /// is treated as a potential bucket and its subdirectories are scanned
    /// for sessions. Directory enumeration goes through the injected
    /// [`DynFileSystem`] provider (zero-IO T13b).
    ///
    /// Each entry includes the wire log file size (if present), the
    /// `state.json` modification time (if present), the session's
    /// `work_dir` from `state.json` (if recorded), and the session's
    /// `archived` flag. Sessions whose `state.json` is missing or
    /// unreadable are skipped with a warning — a listing never presents
    /// broken or incomplete sessions.
    ///
    /// Archived sessions (the `archived` flag in `state.json`, set by
    /// `session.archive`) are excluded unless `include_archived` is set —
    /// the TS engine's default listing filter.
    pub async fn list_persisted_sessions(
        &self,
        include_archived: bool,
    ) -> Vec<PersistedSessionInfo> {
        let mut result = Vec::new();

        let entries = match self.io.iterdir(&self.sessions_dir).await {
            Ok(entries) => entries,
            Err(_) => return result, // Directory doesn't exist → empty list.
        };

        for entry in entries {
            if !entry.is_dir {
                continue;
            }

            let entry_path = PathBuf::from(&entry.path);
            let session_id = entry.name;

            // --- Flat layout: <sessions_dir>/<session_id>/state.json ---
            //
            // If state.json exists directly under this entry, treat it as a
            // flat-layout session. Corrupt state.json is warned and skipped,
            // matching the TS `safeParse` filtering of session lists.
            let state_path = entry_path.join("state.json");
            if self.io.exists(&state_path).await.unwrap_or(false) {
                if let Some(info) = self.try_read_session_info(&entry_path, &session_id).await {
                    // Archived sessions drop out of the default listing
                    // (TS parity); only an explicit opt-in keeps them.
                    if include_archived || !info.archived {
                        result.push(info);
                    }
                }
                continue;
            }

            // --- TS bucketed layout: <sessions_dir>/<bucket>/<session_id>/ ---
            //
            // The TS kernel organises sessions under a workDir-keyed bucket
            // directory (loosely matching the `wd_*_*` naming convention).
            // When a first-level entry has no state.json, treat it as a
            // potential bucket and scan its subdirectories for sessions.
            let bucket_entries = match self.io.iterdir(&entry_path).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for bucket_entry in bucket_entries {
                if !bucket_entry.is_dir {
                    continue;
                }

                let bucket_session_dir = PathBuf::from(&bucket_entry.path);

                if let Some(info) = self
                    .try_read_session_info(&bucket_session_dir, &bucket_entry.name)
                    .await
                    && (include_archived || !info.archived)
                {
                    result.push(info);
                }
            }
        }

        result.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        result
    }

    /// One-time migration: move legacy flat-layout sessions into their
    /// per-workspace buckets.
    ///
    /// The Rust engine historically wrote sessions flat
    /// (`<sessions_dir>/<session_id>/`) while the TS kernel buckets them by
    /// workDir key (`<sessions_dir>/<wd_…>/<session_id>/`). This method
    /// scans the root's first-level directories and, for each one that
    /// structurally looks like a FLAT session (it contains `state.json`
    /// directly — the same structural rule [`Self::list_persisted_sessions`]
    /// uses; anything without one is an existing bucket or foreign data and
    /// is left untouched):
    ///
    /// 1. reads its `state.json` → `workDir`; when the field is absent (a
    ///    rust-written flat session predating the field), the CURRENT
    ///    workspace cwd is used instead and **backfilled** into the state
    ///    file (via [`AtomicDocument`], preserving every other field) — the
    ///    attribution trade-off: an unattributed flat session is assumed to
    ///    belong to the workspace doing the migrating;
    /// 2. derives the bucket key with [`encode_work_dir_key`] and moves the
    ///    directory there (through the injected [`DynFileSystem`], mkdir +
    ///    rename).
    ///
    /// A target that already exists is skipped with a warning — never
    /// overwritten. Idempotent: a re-run finds no flat sessions (everything
    /// already lives in a bucket) and is a no-op. Returns how many sessions
    /// were moved (0 when the layout is already fully bucketed).
    pub async fn migrate_legacy_flat_sessions(
        &self,
        current_work_dir: &str,
    ) -> SessionResult<usize> {
        let entries = match self.io.iterdir(&self.sessions_dir).await {
            Ok(entries) => entries,
            Err(_) => return Ok(0), // Directory doesn't exist → nothing to migrate.
        };

        let mut migrated = 0;
        for entry in entries {
            if !entry.is_dir {
                continue;
            }
            let flat_dir = PathBuf::from(&entry.path);
            let session_id = entry.name;

            // Structural rule shared with the listing: a first-level dir
            // WITH a direct state.json is a flat session; without one it is
            // a bucket (or foreign data) — never touched.
            let state_path = flat_dir.join("state.json");
            if !self.io.exists(&state_path).await.unwrap_or(false) {
                continue;
            }

            // Read the state; an unparseable file is warned and skipped —
            // the migration never moves a session it cannot attribute (the
            // listing's dual-layout read still finds it where it is).
            let mut persisted: PersistedSessionMetadata =
                match AtomicDocument::read(&state_path).await {
                    Ok(Some(persisted)) => persisted,
                    Ok(None) => {
                        tracing::warn!(
                            session_id = %session_id,
                            state = %state_path.display(),
                            "skipping legacy session without state.json"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session_id,
                            state = %state_path.display(),
                            %error,
                            "skipping legacy session with unreadable state.json"
                        );
                        continue;
                    }
                };

            // Backfill: a flat session without `workDir` (rust-written, the
            // field postdates it) is attributed to the CURRENT workspace's
            // cwd and the attribution is persisted before the move — a
            // crash between the two leaves a migratable flat session, never
            // a misattributed bucketed one.
            if persisted.work_dir.is_none() {
                persisted.work_dir = Some(current_work_dir.to_string());
                AtomicDocument::write(&state_path, &persisted)
                    .await
                    .map_err(|e| SessionError::StateFile(e.to_string()))?;
            }
            let work_dir = persisted.work_dir.clone().expect("backfilled above");

            let bucket_key = encode_work_dir_key(Path::new(&work_dir));
            let target = self.sessions_dir.join(&bucket_key).join(&session_id);
            if self.io.exists(&target).await.unwrap_or(false) {
                tracing::warn!(
                    session_id = %session_id,
                    target = %target.display(),
                    "skipping legacy session migration: target already exists"
                );
                continue;
            }
            let bucket_dir = self.sessions_dir.join(&bucket_key);
            self.io.mkdir(&bucket_dir, true).await.map_err(|e| {
                SessionError::StateFile(format!(
                    "failed to create bucket {}: {}",
                    bucket_dir.display(),
                    e
                ))
            })?;
            self.io.rename(&flat_dir, &target).await.map_err(|e| {
                SessionError::StateFile(format!(
                    "failed to move {} to {}: {}",
                    flat_dir.display(),
                    target.display(),
                    e
                ))
            })?;
            migrated += 1;
            tracing::info!(
                session_id = %session_id,
                bucket = %bucket_key,
                "migrated legacy flat session into its workspace bucket"
            );
        }
        Ok(migrated)
    }
}

/// Three-way field merge of a `state.json` document (ADR-003-R1).
///
/// `base` is the merging side's own last-written snapshot (the common
/// ancestor), `ours` is what it is about to write (it is therefore the last
/// writer), `theirs` is the current on-disk document (an earlier foreign
/// write). Per field:
///
/// - ours == base (we did not touch it) → take `theirs` (the foreign change
///   survives our save);
/// - theirs == base (only we touched it) → take `ours` (a no-op that keeps
///   the common shape explicit);
/// - both changed → take `ours` (last writer wins on the same field).
///
/// Field granularity only — nested containers (`tags`, `custom`, `agents`,
/// …) are compared and taken as a whole, no deep merge (ADR-003-R1
/// explicitly scopes the merge at the field level).
fn merge_persisted_metadata(
    base: &PersistedSessionMetadata,
    ours: &PersistedSessionMetadata,
    theirs: &PersistedSessionMetadata,
) -> PersistedSessionMetadata {
    /// The three-way resolution for one field: unchanged on our side defers
    /// to the foreign value, otherwise ours (the last writer) wins.
    fn pick<T: PartialEq + Clone>(base: &T, ours: &T, theirs: &T) -> T {
        if ours == base {
            theirs.clone()
        } else {
            ours.clone()
        }
    }

    PersistedSessionMetadata {
        version: pick(&base.version, &ours.version, &theirs.version),
        title: pick(&base.title, &ours.title, &theirs.title),
        created_at: pick(&base.created_at, &ours.created_at, &theirs.created_at),
        updated_at: pick(&base.updated_at, &ours.updated_at, &theirs.updated_at),
        tags: pick(&base.tags, &ours.tags, &theirs.tags),
        archived: pick(&base.archived, &ours.archived, &theirs.archived),
        custom_title: pick(&base.custom_title, &ours.custom_title, &theirs.custom_title),
        is_custom_title: pick(
            &base.is_custom_title,
            &ours.is_custom_title,
            &theirs.is_custom_title,
        ),
        last_prompt: pick(&base.last_prompt, &ours.last_prompt, &theirs.last_prompt),
        work_dir: pick(&base.work_dir, &ours.work_dir, &theirs.work_dir),
        custom: pick(&base.custom, &ours.custom, &theirs.custom),
        forked_from: pick(&base.forked_from, &ours.forked_from, &theirs.forked_from),
        cache_root_id: pick(
            &base.cache_root_id,
            &ours.cache_root_id,
            &theirs.cache_root_id,
        ),
        agents: pick(&base.agents, &ours.agents, &theirs.agents),
    }
}

/// A session directory located under a sessions root by the structural
/// dual-layout rule shared with
/// [`SessionPersistence::list_persisted_sessions`].
///
/// This is the synchronous shape for callers that need the on-disk location
/// of every session under a root (the `migrate` CLI) without going
/// through the async injected-filesystem listing.
#[derive(Debug, Clone)]
pub struct LocatedSessionDir {
    /// The session id (the session directory's file name).
    pub session_id: String,
    /// The bucket directory name when the TS bucketed layout located the
    /// session; `None` for the flat layout.
    pub bucket: Option<String>,
    /// The session directory's full path.
    pub dir: PathBuf,
}

/// Synchronously enumerate every session directory under `sessions_dir`
/// using the same structural dual-layout detection as
/// [`SessionPersistence::list_persisted_sessions`]: a first-level
/// subdirectory that directly contains `state.json` is a flat-layout
/// session; one that does not is treated as a potential bucket and its
/// subdirectories that contain `state.json` are bucketed sessions.
///
/// Filesystem access goes through the injected [`SyncFs`] provider (the
/// zero-IO contract); callers pass the host-local [`mirri_os::LocalSyncFs`].
///
/// Unlike the async listing this performs no `state.json` parsing — the
/// caller decides what to do with each directory (the storage-root
/// migration moves directories wholesale, unreadable state included) — so
/// a session with a corrupt state file is still enumerated. A missing
/// `sessions_dir` yields an empty list; any other read failure propagates.
pub fn locate_session_dirs(
    sessions_dir: &Path,
    fs: &dyn SyncFs,
) -> anyhow::Result<Vec<LocatedSessionDir>> {
    let mut result = Vec::new();
    let entries = match fs.iterdir(sessions_dir) {
        Ok(entries) => entries,
        // A missing sessions root is an empty listing (mirroring the async
        // listing); any other read failure of the root propagates.
        Err(error) if is_io_not_found(&error) => return Ok(result),
        Err(error) => return Err(error),
    };
    for entry in entries {
        if !entry.is_dir {
            continue;
        }
        let entry_path = PathBuf::from(&entry.path);
        let entry_name = entry.name;

        // --- Flat layout: <sessions_dir>/<session_id>/state.json ---
        if fs.exists(&entry_path.join("state.json")) {
            result.push(LocatedSessionDir {
                session_id: entry_name,
                bucket: None,
                dir: entry_path,
            });
            continue;
        }

        // --- TS bucketed layout: <sessions_dir>/<bucket>/<session_id>/ ---
        for bucket_entry in fs.iterdir(&entry_path)? {
            if !bucket_entry.is_dir {
                continue;
            }
            let bucket_session_dir = PathBuf::from(&bucket_entry.path);
            if fs.exists(&bucket_session_dir.join("state.json")) {
                result.push(LocatedSessionDir {
                    session_id: bucket_entry.name,
                    bucket: Some(entry_name.clone()),
                    dir: bucket_session_dir,
                });
            }
        }
    }
    Ok(result)
}

/// Whether an injected-filesystem error reports a missing path. The
/// [`SyncFs`] implementations surface the underlying
/// [`std::io::ErrorKind::NotFound`] inside their anyhow error chain; the
/// root `iterdir` misses are an empty listing, never an error.
fn is_io_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn should_roundtrip_state_when_saving_then_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );

        let session_id = SessionId::new("test-session-1");
        let mut tags = HashSet::new();
        tags.insert("important".to_string());
        tags.insert("debug".to_string());

        let metadata = SessionMetadata {
            title: "Test Session".to_string(),
            is_custom_title: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            tags,
            archived: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
        };

        persistence
            .save_state(&session_id, &metadata)
            .await
            .unwrap();

        let loaded = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(loaded.title, "Test Session");
        // The custom-title flag round-trips: a user rename survives the
        // restart, so the send path's auto-title stays off for it.
        assert!(loaded.is_custom_title);
        assert_eq!(loaded.tags.len(), 2);
        assert!(loaded.tags.contains("important"));
        assert!(loaded.tags.contains("debug"));
    }

    #[tokio::test]
    async fn should_return_correct_file_paths_when_computing_wire_log_path() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );

        let session_id = SessionId::new("sess-abc");
        let wire_path = persistence.wire_log_path(&session_id);
        let state_path = persistence.state_path(&session_id);

        assert_eq!(wire_path, tmp.path().join("sess-abc").join("wire.jsonl"));
        assert_eq!(state_path, tmp.path().join("sess-abc").join("state.json"));
    }

    #[tokio::test]
    async fn should_include_the_workspace_bucket_when_computing_paths_with_a_key() {
        // Given a persistence layer created with a workspace key
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        )
        .with_workspace_key("wd_project_abc123def456".to_string());

        // When the per-session paths are computed
        let session_id = SessionId::new("sess-abc");
        let session_dir = persistence.session_dir(&session_id);
        let wire_path = persistence.wire_log_path(&session_id);
        let state_path = persistence.state_path(&session_id);
        let snapshot_path = persistence.snapshot_path(&session_id);

        // Then every path nests under the workspace bucket (the TS
        // per-workspace layout)
        let bucket = tmp.path().join("wd_project_abc123def456");
        assert_eq!(session_dir, bucket.join("sess-abc"));
        assert_eq!(wire_path, bucket.join("sess-abc").join("wire.jsonl"));
        assert_eq!(state_path, bucket.join("sess-abc").join("state.json"));
        assert_eq!(snapshot_path, bucket.join("sess-abc").join("snapshot.json"));

        // And the key-less form stays flat (the legacy layout)
        let flat = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        assert_eq!(
            flat.wire_log_path(&session_id),
            tmp.path().join("sess-abc").join("wire.jsonl")
        );
    }

    #[tokio::test]
    async fn should_persist_under_the_bucket_when_saving_state_with_a_key() {
        // Given a persistence layer with a workspace key
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        )
        .with_workspace_key("wd_proj_aabbccddeeff".to_string());

        // When a state is saved
        let session_id = SessionId::new("bucketed-session");
        persistence
            .save_state(&session_id, &SessionMetadata::default())
            .await
            .unwrap();

        // Then the state lands under the bucket and reloads from there
        assert!(
            tmp.path()
                .join("wd_proj_aabbccddeeff")
                .join("bucketed-session")
                .join("state.json")
                .exists()
        );
        assert_eq!(
            persistence.load_state(&session_id).await.unwrap().title,
            "Untitled Session"
        );
    }

    #[tokio::test]
    async fn should_preserve_work_dir_when_saving_then_loading_state() {
        // Given a TS-written state.json carrying a workDir
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("workdir-roundtrip");
        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"T","workDir":"/Users/dev/proj","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        // When the loaded metadata is re-saved
        let loaded = persistence.load_state(&session_id).await.unwrap();
        persistence.save_state(&session_id, &loaded).await.unwrap();

        // Then the workDir round-trips through the scope-layer metadata
        // instead of degrading to None
        let reloaded = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(reloaded.work_dir.as_deref(), Some("/Users/dev/proj"));
    }

    #[tokio::test]
    async fn should_remove_session_files_when_deleting_session() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );

        let session_id = SessionId::new("to-delete");
        let metadata = SessionMetadata::default();
        persistence
            .save_state(&session_id, &metadata)
            .await
            .unwrap();

        assert!(persistence.state_path(&session_id).exists());

        persistence.delete_session_files(&session_id).await.unwrap();

        assert!(!persistence.state_path(&session_id).exists());
    }

    #[tokio::test]
    async fn should_return_all_persisted_sessions_when_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );

        let s1 = SessionId::new("session-1");
        let s2 = SessionId::new("session-2");

        persistence
            .save_state(&s1, &SessionMetadata::default())
            .await
            .unwrap();
        persistence
            .save_state(&s2, &SessionMetadata::default())
            .await
            .unwrap();

        let sessions = persistence.list_persisted_sessions(false).await;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "session-1");
        assert_eq!(sessions[1].session_id, "session-2");
    }

    // -----------------------------------------------------------------------
    // Archived flag parity (TS `state.json` `archived` field)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_roundtrip_archived_flag_when_saving_then_loading_state() {
        // Given a session whose metadata is flagged archived
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("archived-roundtrip");
        let metadata = SessionMetadata {
            archived: true,
            ..SessionMetadata::default()
        };

        // When the state is saved and loaded back
        persistence
            .save_state(&session_id, &metadata)
            .await
            .unwrap();
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then the archived flag round-trips through state.json
        assert!(loaded.archived);
    }

    #[tokio::test]
    async fn should_flip_archived_flag_when_set_archived_called_on_persisted_session() {
        // Given a persisted session saved with the default (un-archived) flag
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("set-archived");
        persistence
            .save_state(&session_id, &SessionMetadata::default())
            .await
            .unwrap();

        // When the flag is set to true
        persistence.set_archived(&session_id, true).await.unwrap();

        // Then the reloaded state carries the flag
        assert!(persistence.load_state(&session_id).await.unwrap().archived);

        // And when it is cleared again
        persistence.set_archived(&session_id, false).await.unwrap();

        // Then the reloaded state no longer carries it
        assert!(!persistence.load_state(&session_id).await.unwrap().archived);
    }

    #[tokio::test]
    async fn should_error_when_set_archived_called_without_state_json() {
        // Given a session with no state.json on disk
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("no-state-set-archived");

        // When the archived flag is set, Then the missing state file is an
        // explicit error (this is a write, not a tolerated read)
        let error = persistence
            .set_archived(&session_id, true)
            .await
            .unwrap_err();
        assert!(matches!(&error, SessionError::StateFile(_)), "{error:?}");
    }

    #[tokio::test]
    async fn should_exclude_archived_sessions_when_listing_by_default() {
        // Given a persisted session with archived=false and one with
        // archived=true (a TS-written state.json spelling)
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let live_session = SessionId::new("plain-session");
        let archived_session = SessionId::new("archived-session");
        persistence
            .save_state(&live_session, &SessionMetadata::default())
            .await
            .unwrap();

        let state_path = persistence.state_path(&archived_session);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"Archived","archived":true,"createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        // When sessions are listed with the default filter
        let sessions = persistence.list_persisted_sessions(false).await;

        // Then the archived session is excluded and the plain one remains
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "plain-session");
        assert!(!sessions[0].archived);

        // And when the listing opts in to archived sessions
        let sessions = persistence.list_persisted_sessions(true).await;

        // Then both are returned and the archived row carries its flag
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "archived-session");
        assert!(sessions[0].archived);
        assert_eq!(sessions[1].session_id, "plain-session");
        assert!(!sessions[1].archived);
    }

    #[tokio::test]
    async fn should_read_the_summary_when_a_persisted_state_json_is_readable() {
        // Given a persisted session with full metadata
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("summary-readable");
        let metadata = SessionMetadata {
            title: "Summarised".to_string(),
            ..SessionMetadata::default()
        };
        persistence
            .save_state(&session_id, &metadata)
            .await
            .unwrap();

        // When the single-session summary is read
        let summary = persistence.read_persisted_summary(&session_id).await;

        // Then the full metadata comes back (the same validated read the
        // listing performs)
        assert_eq!(
            summary.expect("the readable session is summarized").title,
            "Summarised"
        );
    }

    #[tokio::test]
    async fn should_return_none_when_the_summary_state_json_is_missing_or_corrupt() {
        // Given a persisted session with a corrupt state.json and one with
        // no state.json at all
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let corrupt_id = SessionId::new("summary-corrupt");
        let state_path = persistence.state_path(&corrupt_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&state_path, "{ not json").await.unwrap();
        let missing_id = SessionId::new("summary-missing");

        // When the summaries are read, Then both are skipped (the listing's
        // tolerance, not load_state's degraded default)
        assert!(
            persistence
                .read_persisted_summary(&corrupt_id)
                .await
                .is_none()
        );
        assert!(
            persistence
                .read_persisted_summary(&missing_id)
                .await
                .is_none()
        );
    }

    // -----------------------------------------------------------------------
    // 12C — TS camelCase state.json compatibility
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_load_ts_state_json_when_state_uses_camel_case_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("ts-session");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{
  "archived": false,
  "customTitle": null,
  "isCustomTitle": true,
  "lastPrompt": "fix the tests",
  "title": "TS Session",
  "workDir": "/Users/dev/project",
  "custom": {"theme": "dark"},
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-02T00:00:00Z",
  "forkedFrom": null,
  "agents": []
}
"#,
        )
        .await
        .unwrap();

        let loaded = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(loaded.title, "TS Session");
        assert_eq!(
            loaded.created_at,
            chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            loaded.updated_at,
            chrono::DateTime::parse_from_rfc3339("2024-01-02T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[tokio::test]
    async fn should_roundtrip_ts_fields_when_saving_then_loading_state() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("roundtrip-ts-fields");

        // A TS-written state.json loaded and re-saved keeps its extra fields.
        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"T","workDir":"/w","isCustomTitle":true,"archived":true,"createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        let loaded = persistence.load_state(&session_id).await.unwrap();
        persistence.save_state(&session_id, &loaded).await.unwrap();

        let reloaded = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(reloaded.title, "T");
    }

    // -----------------------------------------------------------------------
    // v3 audit P1-2 — last_prompt survives the save/load roundtrip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_preserve_last_prompt_across_a_save_then_load_roundtrip() {
        // Given a session whose metadata carries the last user prompt
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("roundtrip-last-prompt");
        let metadata = SessionMetadata {
            last_prompt: Some("hello v3".to_string()),
            ..SessionMetadata::default()
        };

        // When the state is saved then loaded
        persistence
            .save_state(&session_id, &metadata)
            .await
            .unwrap();
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then the last prompt survives the roundtrip (the audit bug: the
        // From impl hardcoded `None` on write and dropped the field on
        // read, so a load-mutate-save cycle erased it — ADR-009).
        assert_eq!(loaded.last_prompt.as_deref(), Some("hello v3"));
    }

    // -----------------------------------------------------------------------
    // v3 audit P1-3 / P2-4 — TS `cwd` key + state.json `version` field
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_load_ts_state_fields_from_a_ts_state_json_using_the_cwd_key() {
        // Given a TS-written state.json that spells the workspace root as
        // `cwd` (not `workDir`) and stamps the TS layout version
        let ts_state = r#"{
  "id": "session_x",
  "version": 2,
  "cwd": "/tmp/ts-ws",
  "createdAt": 1788494508725,
  "updatedAt": 1788494509999,
  "archived": false,
  "lastPrompt": "hi",
  "title": "t"
}
"#;
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("ts-cwd-key");
        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&state_path, ts_state).await.unwrap();

        // When the state is loaded
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then the `cwd` key populates the work dir
        assert_eq!(loaded.work_dir.as_deref(), Some("/tmp/ts-ws"));

        // And the persisted shape keeps the TS-only fields: the layout
        // version is preserved on read, alongside `lastPrompt`.
        let persisted: PersistedSessionMetadata =
            serde_json::from_str(ts_state).expect("the TS state.json parses");
        assert_eq!(persisted.version, Some(2));
        assert_eq!(persisted.last_prompt.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn should_write_a_version_field_into_new_state_json() {
        // Given a rust session saved via save_state
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("rust-version-stamp");
        persistence
            .save_state(&session_id, &SessionMetadata::default())
            .await
            .unwrap();

        // When reading the raw state.json text
        let raw = tokio::fs::read_to_string(persistence.state_path(&session_id))
            .await
            .unwrap();

        // Then the v3 layout version is stamped
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            json.get("version").and_then(serde_json::Value::as_u64),
            Some(3),
            "expected state.json to carry version 3, got: {raw}"
        );
    }

    #[tokio::test]
    async fn should_return_default_metadata_when_state_json_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("corrupt-state");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&state_path, "this is not json at all")
            .await
            .unwrap();

        // TS `safeParse` tolerance: corrupt state must not fail the load.
        let metadata = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(metadata.title, "Untitled Session");
    }

    // -----------------------------------------------------------------------
    // P11c — cross-engine timestamp value formats
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_restore_the_timestamp_when_state_json_carries_a_ts_epoch_integer() {
        // Given a TS-written state.json whose timestamps are epoch-ms integers
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("ts-epoch-ts");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"TS Epoch","createdAt":1788494508725,"updatedAt":1788494509999,"tags":[]}"#,
        )
        .await
        .unwrap();

        // When the state is loaded
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then both timestamps are restored from the epoch-ms values
        assert_eq!(loaded.title, "TS Epoch");
        assert_eq!(
            loaded.created_at,
            DateTime::<Utc>::from_timestamp_millis(1788494508725).unwrap()
        );
        assert_eq!(
            loaded.updated_at,
            DateTime::<Utc>::from_timestamp_millis(1788494509999).unwrap()
        );
    }

    #[tokio::test]
    async fn should_restore_the_timestamp_when_state_json_carries_an_rfc3339_string() {
        // Given a rust-written state.json whose timestamps are RFC 3339 strings
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("rust-rfc3339-ts");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"Rust RFC3339","createdAt":"2024-06-01T12:30:45.678Z","updatedAt":"2024-06-02T08:00:00+02:00","tags":[]}"#,
        )
        .await
        .unwrap();

        // When the state is loaded
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then both timestamps are restored (offsets normalised to UTC)
        assert_eq!(loaded.title, "Rust RFC3339");
        assert_eq!(
            loaded.created_at,
            DateTime::parse_from_rfc3339("2024-06-01T12:30:45.678Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            loaded.updated_at,
            DateTime::parse_from_rfc3339("2024-06-02T08:00:00+02:00")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[tokio::test]
    async fn should_fall_back_to_default_metadata_when_state_json_timestamp_is_invalid() {
        // Given a state.json whose timestamp is neither spelling
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("invalid-ts");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"Broken","createdAt":true,"updatedAt":"2024-06-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        // When the state is loaded
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // Then the corrupt file degrades to default metadata (TS safeParse
        // tolerance) instead of failing the load
        assert_eq!(loaded.title, "Untitled Session");

        // And the listing skips the session rather than presenting it
        let sessions = persistence.list_persisted_sessions(false).await;
        assert!(sessions.is_empty());
    }

    // -----------------------------------------------------------------------
    // 12I — listing validates state.json and exposes work_dir
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_skip_session_without_state_json_when_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("no-state");
        tokio::fs::create_dir_all(persistence.session_dir(&session_id))
            .await
            .unwrap();

        let sessions = persistence.list_persisted_sessions(false).await;
        assert!(
            sessions.is_empty(),
            "a session directory without state.json must not be listed"
        );
    }

    #[tokio::test]
    async fn should_skip_session_with_corrupt_state_json_when_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("corrupt-state-list");
        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&state_path, "{not json").await.unwrap();

        let sessions = persistence.list_persisted_sessions(false).await;
        assert!(
            sessions.is_empty(),
            "a session with unreadable state.json must not be listed"
        );
    }

    #[tokio::test]
    async fn should_include_work_dir_when_listing_ts_state_json_session() {
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("with-workdir");

        let state_path = persistence.state_path(&session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &state_path,
            r#"{"title":"T","workDir":"/Users/dev/proj","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        let sessions = persistence.list_persisted_sessions(false).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "with-workdir");
        assert_eq!(sessions[0].work_dir.as_deref(), Some("/Users/dev/proj"));
    }

    // -----------------------------------------------------------------------
    // P0-G07 — TS bucketed directory layout support
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_list_sessions_in_ts_bucketed_layout() {
        // Given a sessions directory using the TS workDir-bucketed layout
        // (<sessions_dir>/<bucket>/<session_id>/state.json) mixed with a
        // flat-layout session
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );

        // TS bucketed sessions — bucket dir loosely matches wd_*_*.
        let bucket_dir = tmp.path().join("wd_abc123_def456");
        let ts_session_1 = bucket_dir.join("ts-session-1");
        let ts_session_2 = bucket_dir.join("ts-session-2");
        tokio::fs::create_dir_all(&ts_session_1).await.unwrap();
        tokio::fs::create_dir_all(&ts_session_2).await.unwrap();

        tokio::fs::write(
            ts_session_1.join("state.json"),
            r#"{"title":"TS Session 1","workDir":"/proj","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(ts_session_1.join("wire.jsonl"), "{\"op\":\"test\"}\n")
            .await
            .unwrap();
        tokio::fs::write(
            ts_session_2.join("state.json"),
            r#"{"title":"TS Session 2","workDir":"/proj","createdAt":"2024-01-02T00:00:00Z","updatedAt":"2024-01-02T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        // Flat-layout session alongside the bucket.
        let flat_session = SessionId::new("flat-session");
        persistence
            .save_state(&flat_session, &SessionMetadata::default())
            .await
            .unwrap();

        // When listing persisted sessions
        let sessions = persistence.list_persisted_sessions(false).await;

        // Then all three sessions are found, sorted by session_id
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].session_id, "flat-session");
        assert_eq!(sessions[1].session_id, "ts-session-1");
        assert_eq!(sessions[2].session_id, "ts-session-2");

        // And the TS sessions expose their work_dir from state.json
        assert_eq!(sessions[1].work_dir.as_deref(), Some("/proj"));
        assert_eq!(sessions[2].work_dir.as_deref(), Some("/proj"));

        // And wire log size is detected for ts-session-1, zero for ts-session-2
        assert!(sessions[1].wire_log_size > 0);
        assert_eq!(sessions[2].wire_log_size, 0);
    }

    // -----------------------------------------------------------------------
    // Phase 2 — legacy flat-layout migration into workspace buckets
    // -----------------------------------------------------------------------

    /// Lay down a FLAT legacy session directory (state.json directly under
    /// the sessions root) with an optional `workDir` in its state.
    async fn plant_flat_session(
        tmp: &tempfile::TempDir,
        session_id: &str,
        work_dir: Option<&str>,
        extra_files: &[(&str, &str)],
    ) -> PathBuf {
        let dir = tmp.path().join(session_id);
        let state = match work_dir {
            Some(work_dir) => format!(
                r#"{{"title":"Legacy","workDir":{work_dir:?},"createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}}"#
            ),
            None => r#"{"title":"Legacy","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#.to_string(),
        };
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("state.json"), state)
            .await
            .unwrap();
        for (name, contents) in extra_files {
            tokio::fs::write(dir.join(name), contents).await.unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn should_move_a_flat_session_into_its_work_dir_bucket_when_migrating() {
        // Given a flat legacy session whose state records its workDir
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let flat = plant_flat_session(
            &tmp,
            "session-flat",
            Some("/Users/dev/legacy-proj"),
            &[("wire.jsonl", "{\"op\":\"metadata\"}\n")],
        )
        .await;

        // When the legacy migration runs
        let migrated = persistence
            .migrate_legacy_flat_sessions("/unrelated/current-cwd")
            .await
            .unwrap();

        // Then the session moved into the bucket derived from ITS OWN
        // workDir (not the migrating workspace's cwd)
        assert_eq!(migrated, 1);
        assert!(!flat.exists(), "the flat directory must be gone");
        let bucket = encode_work_dir_key(Path::new("/Users/dev/legacy-proj"));
        let moved = tmp.path().join(&bucket).join("session-flat");
        assert!(moved.join("state.json").exists());
        assert!(moved.join("wire.jsonl").exists());

        // And the listing still finds it (the bucketed read path)
        let sessions = persistence.list_persisted_sessions(false).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-flat");
    }

    #[tokio::test]
    async fn should_backfill_the_current_cwd_when_migrating_a_flat_session_without_work_dir() {
        // Given a flat legacy session whose state predates the workDir field
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        plant_flat_session(&tmp, "session-no-workdir", None, &[]).await;

        // When the legacy migration runs with the current workspace cwd
        let migrated = persistence
            .migrate_legacy_flat_sessions("/Users/dev/current-proj")
            .await
            .unwrap();

        // Then the session is attributed to the current workspace: its
        // state carries the backfilled workDir and the directory moved
        // into that workspace's bucket
        assert_eq!(migrated, 1);
        let bucket = encode_work_dir_key(Path::new("/Users/dev/current-proj"));
        let moved = tmp.path().join(&bucket).join("session-no-workdir");
        assert!(moved.join("state.json").exists());
        let bucketed = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        )
        .with_workspace_key(bucket);
        assert_eq!(
            bucketed
                .load_state(&SessionId::new("session-no-workdir"))
                .await
                .unwrap()
                .work_dir
                .as_deref(),
            Some("/Users/dev/current-proj")
        );
    }

    #[tokio::test]
    async fn should_find_nothing_to_migrate_when_running_the_migration_twice() {
        // Given an already-migrated (bucketed) sessions root
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        plant_flat_session(&tmp, "session-once", Some("/Users/dev/proj"), &[]).await;
        let first = persistence
            .migrate_legacy_flat_sessions("/Users/dev/proj")
            .await
            .unwrap();
        assert_eq!(first, 1);

        // When the migration runs a second time
        let second = persistence
            .migrate_legacy_flat_sessions("/Users/dev/proj")
            .await
            .unwrap();

        // Then it is a no-op (idempotent)
        assert_eq!(second, 0);
    }

    #[tokio::test]
    async fn should_skip_a_flat_session_when_the_bucket_target_already_exists() {
        // Given a flat legacy session AND a pre-existing session under the
        // same id in its target bucket
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        plant_flat_session(&tmp, "session-clash", Some("/Users/dev/proj"), &[]).await;
        let bucket = encode_work_dir_key(Path::new("/Users/dev/proj"));
        let target = tmp.path().join(&bucket).join("session-clash");
        tokio::fs::create_dir_all(&target).await.unwrap();
        tokio::fs::write(
            target.join("state.json"),
            r#"{"title":"Bucketed survivor","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();

        // When the legacy migration runs
        let migrated = persistence
            .migrate_legacy_flat_sessions("/Users/dev/proj")
            .await
            .unwrap();

        // Then the clash is skipped (never overwritten) and both sides
        // survive on disk
        assert_eq!(migrated, 0);
        assert!(tmp.path().join("session-clash").exists());
        let sessions = persistence.list_persisted_sessions(true).await;
        assert_eq!(
            sessions
                .iter()
                .filter(|s| s.session_id == "session-clash")
                .count(),
            2,
            "the listing sees both the flat and bucketed survivors"
        );
    }

    #[tokio::test]
    async fn should_leave_bucket_directories_untouched_when_migrating_legacy_layout() {
        // Given a sessions root mixing a TS bucket, a stray file, and a
        // directory that is neither a session nor a bucket
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let bucket = tmp.path().join("wd_abc123_def456");
        let ts_session = bucket.join("ts-session-1");
        tokio::fs::create_dir_all(&ts_session).await.unwrap();
        tokio::fs::write(
            ts_session.join("state.json"),
            r#"{"title":"TS Session","workDir":"/proj","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(tmp.path().join("notes.txt"), "stray")
            .await
            .unwrap();
        let not_a_session = tmp.path().join("unattributed-dir");
        tokio::fs::create_dir_all(&not_a_session).await.unwrap();

        // When the legacy migration runs
        let migrated = persistence
            .migrate_legacy_flat_sessions("/Users/dev/proj")
            .await
            .unwrap();

        // Then only structurally-flat sessions move — the bucket, the
        // file, and the stateless directory are left exactly where they
        // are (the same structural rule the listing uses)
        assert_eq!(migrated, 0);
        assert!(ts_session.exists());
        assert!(tmp.path().join("notes.txt").exists());
        assert!(not_a_session.exists());
    }

    // -----------------------------------------------------------------------
    // T-13b2-2 — listing through an injected iterdir provider (zero-IO T13b)
    // -----------------------------------------------------------------------

    /// [`DynFileSystem`] double that serves a **fixed** directory
    /// enumeration: `iterdir` returns the advertised entries rather than
    /// scanning the host filesystem, while `exists` / `stat` consult the real
    /// tempdir backing the advertised paths (state.json reads go through the
    /// shared `AtomicDocument`, which is outside this injection seam).
    struct AdvertisedFs {
        /// Advertised children per directory path.
        enumerations: std::collections::HashMap<PathBuf, Vec<mirri_os::fs::DirEntry>>,
    }

    impl AdvertisedFs {
        fn advertise_dir(&mut self, dir: &Path, children: &[(&str, bool)]) {
            let entries = children
                .iter()
                .map(|(name, is_dir)| mirri_os::fs::DirEntry {
                    name: (*name).to_string(),
                    path: dir.join(name).to_string_lossy().into_owned(),
                    is_dir: *is_dir,
                    is_file: !*is_dir,
                })
                .collect();
            self.enumerations.insert(dir.to_path_buf(), entries);
        }
    }

    impl mirri_os::DynFileSystem for AdvertisedFs {
        fn iterdir<'a>(
            &'a self,
            path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<Vec<mirri_os::fs::DirEntry>>> {
            Box::pin(async move {
                match self.enumerations.get(path) {
                    Some(entries) => Ok(entries.clone()),
                    None => Err(anyhow::anyhow!(
                        "directory not advertised: {}",
                        path.display()
                    )),
                }
            })
        }

        fn exists<'a>(&'a self, path: &'a Path) -> mirri_os::DynFsFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move { Ok(tokio::fs::metadata(path).await.is_ok()) })
        }

        fn stat<'a>(
            &'a self,
            path: &'a Path,
        ) -> mirri_os::DynFsFuture<'a, anyhow::Result<mirri_os::fs::FileStat>> {
            Box::pin(async move {
                let metadata = tokio::fs::metadata(path).await?;
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Ok(mirri_os::fs::FileStat {
                    size: metadata.len(),
                    is_dir: metadata.is_dir(),
                    is_file: metadata.is_file(),
                    is_symlink: false,
                    mtime,
                    atime: 0,
                    mode: 0o644,
                })
            })
        }
    }

    // -----------------------------------------------------------------------
    // ADR-003-R1 — state.json three-way field merge on save_state
    // -----------------------------------------------------------------------

    /// Overwrite the on-disk `state.json` with a foreign (other-instance)
    /// version, bypassing the persistence layer's own bookkeeping — the
    /// stand-in for "another entry point wrote the file between our last
    /// write and now".
    async fn plant_foreign_state(
        persistence: &SessionPersistence,
        session_id: &SessionId,
        state: &str,
    ) {
        let state_path = persistence.state_path(session_id);
        tokio::fs::create_dir_all(state_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&state_path, state).await.unwrap();
    }

    #[tokio::test]
    async fn should_merge_disjoint_field_updates_from_both_sides() {
        // Given instance A saved a session and its last-written snapshot is
        // the current disk content (no foreign write yet)
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("merge-disjoint");
        let base = SessionMetadata {
            title: "Base Title".to_string(),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &base).await.unwrap();

        // And another entry point (instance B) archived the session, writing
        // every other field back unchanged
        let loaded = persistence.load_state(&session_id).await.unwrap();
        let b_metadata_json = r#"{
  "title": "Base Title",
  "archived": true,
  "isCustomTitle": false,
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-02T00:00:00Z",
  "tags": []
}"#;
        plant_foreign_state(&persistence, &session_id, b_metadata_json).await;

        // When instance A saves a rename (its own title change) afterwards —
        // the last write, so A is the merging side
        let mut ours = loaded;
        ours.title = "Renamed by A".to_string();
        persistence.save_state(&session_id, &ours).await.unwrap();

        // Then the three-way merge keeps BOTH: A's title (ours changed,
        // theirs did not) and B's archive flag (theirs changed, ours did not)
        let merged = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(merged.title, "Renamed by A");
        assert!(merged.archived, "B's archive flag must survive A's save");
    }

    #[tokio::test]
    async fn should_let_last_writer_win_on_same_field() {
        // Given instance A saved a session titled "A v0"
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("merge-same-field");
        let base = SessionMetadata {
            title: "A v0".to_string(),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &base).await.unwrap();
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // And another entry point (B) renamed the title — the SAME field —
        // and archived the session (a disjoint field)
        plant_foreign_state(
            &persistence,
            &session_id,
            r#"{
  "title": "Renamed by B",
  "archived": true,
  "isCustomTitle": false,
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-02T00:00:00Z",
  "tags": []
}"#,
        )
        .await;

        // When instance A also renames the title and saves afterwards (A is
        // the last writer)
        let mut ours = loaded;
        ours.title = "Renamed by A".to_string();
        persistence.save_state(&session_id, &ours).await.unwrap();

        // Then the last writer (A) wins the same-field conflict
        let merged = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(merged.title, "Renamed by A");
        // And the disjoint foreign change still survives the conflict
        assert!(merged.archived);
    }

    #[tokio::test]
    async fn should_pass_through_uncontested_writes() {
        // Given a session saved twice by the same instance with NO foreign
        // write in between (the single-instance normal path — regression:
        // the merge machinery must not change plain save behaviour)
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("merge-uncontested");
        let first = SessionMetadata {
            title: "First Title".to_string(),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &first).await.unwrap();

        // When the instance saves a second time (a fresh title and a tag)
        let second = SessionMetadata {
            title: "Second Title".to_string(),
            last_prompt: Some("hello".to_string()),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &second).await.unwrap();

        // Then the plain write behaves exactly as before: the second state
        // fully replaces the first, no stale fields linger
        let loaded = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(loaded.title, "Second Title");
        assert_eq!(loaded.last_prompt.as_deref(), Some("hello"));
        assert_eq!(loaded.tags, first.tags);
    }

    #[tokio::test]
    async fn should_take_the_disk_value_when_a_foreign_write_changed_a_field_we_did_not_touch() {
        // Given a session saved once by the instance, then renamed AND
        // archived by a foreign writer, and the instance's save carries
        // unchanged values for both fields (ours == base everywhere)
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("merge-foreign-only");
        let base = SessionMetadata {
            title: "Stale Title".to_string(),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &base).await.unwrap();
        let loaded = persistence.load_state(&session_id).await.unwrap();

        // And a foreign write changed the title, archived the session, AND
        // stamped its own last_prompt (the send path's SDR-004 field — the
        // pin that the merge covers it, not just title/archived)
        plant_foreign_state(
            &persistence,
            &session_id,
            r#"{
  "title": "Fresh Title",
  "archived": true,
  "isCustomTitle": false,
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-02T00:00:00Z",
  "lastPrompt": "foreign last prompt",
  "tags": []
}"#,
        )
        .await;

        // When the instance saves its unchanged metadata back
        persistence.save_state(&session_id, &loaded).await.unwrap();

        // Then every foreign change passes through: ours == base everywhere,
        // so the disk values win without any merge interference (the parked
        // F2 window: an uncontested full-document save must not clobber the
        // foreign last_prompt / archived / … fields)
        let merged = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(merged.title, "Fresh Title");
        assert!(merged.archived);
        assert_eq!(
            merged.last_prompt.as_deref(),
            Some("foreign last prompt"),
            "the foreign last_prompt must survive the uncontested save (the F2 window)"
        );
    }

    #[tokio::test]
    async fn should_not_clobber_foreign_metadata_when_archiving_concurrently() {
        // Given instance A saved a session (its merge ancestor is now the
        // current disk content) and a foreign writer renamed the title in
        // its own save before A got to archive
        let tmp = tempfile::tempdir().unwrap();
        let persistence = SessionPersistence::new(
            tmp.path().to_path_buf(),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session_id = SessionId::new("merge-archive-race");
        let base = SessionMetadata {
            title: "Base Title".to_string(),
            ..SessionMetadata::default()
        };
        persistence.save_state(&session_id, &base).await.unwrap();

        plant_foreign_state(
            &persistence,
            &session_id,
            r#"{
  "title": "Renamed by B",
  "archived": false,
  "isCustomTitle": false,
  "createdAt": "2024-01-01T00:00:00Z",
  "updatedAt": "2024-01-02T00:00:00Z",
  "tags": []
}"#,
        )
        .await;

        // When instance A archives the session — the direct load-mutate-save
        // write that must register what it READ (not the archived-tweaked
        // document it wrote) as the merge ancestor
        persistence.set_archived(&session_id, true).await.unwrap();

        // Then the foreign title survives the archive round-trip itself
        let archived = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(archived.title, "Renamed by B");
        assert!(archived.archived);

        // And a subsequent save_state still sees the disk as a foreign
        // document (its `archived` flag differs from the registered
        // ancestor) and therefore MERGES — the plain-overwrite branch would
        // line the disk back up with this process's stale in-memory view
        // and clobber the foreign rename
        let mut ours = archived;
        ours.last_prompt = Some("hello".to_string());
        persistence.save_state(&session_id, &ours).await.unwrap();

        let merged = persistence.load_state(&session_id).await.unwrap();
        assert_eq!(merged.title, "Renamed by B");
        assert!(merged.archived);
        assert_eq!(merged.last_prompt.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn should_list_sessions_through_an_injected_iterdir_provider() {
        // Given a sessions directory on disk holding a flat session, a TS
        // bucket with two sessions, a stray file, and a session the injected
        // provider does NOT advertise
        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().to_path_buf();

        let state_json = r#"{"title":"T","workDir":"/proj","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-01T00:00:00Z","tags":[]}"#;
        let flat = sessions_dir.join("flat-session");
        let bucket = sessions_dir.join("wd_abc123_def456");
        let ts1 = bucket.join("ts-session-1");
        let ts2 = bucket.join("ts-session-2");
        let hidden = sessions_dir.join("hidden-session");
        for dir in [&flat, &ts1, &ts2, &hidden] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("state.json"), state_json).unwrap();
        }
        std::fs::write(ts1.join("wire.jsonl"), "{\"op\":\"test\"}\n").unwrap();

        // And a provider advertising only a curated view of that directory
        // (the stray file entry must be skipped; hidden-session is absent
        // from the enumeration, so a listing must not see it even though it
        // exists on disk)
        let mut io = AdvertisedFs {
            enumerations: std::collections::HashMap::new(),
        };
        io.advertise_dir(
            &sessions_dir,
            &[
                ("wd_abc123_def456", true),
                ("flat-session", true),
                ("notes.txt", false),
            ],
        );
        io.advertise_dir(&bucket, &[("ts-session-1", true), ("ts-session-2", true)]);

        let persistence = SessionPersistence::new(sessions_dir, std::sync::Arc::new(io));

        // When persisted sessions are listed
        let sessions = persistence.list_persisted_sessions(false).await;

        // Then exactly the advertised sessions are found — flat and bucketed
        // layouts merged, sorted by session id
        assert_eq!(sessions.len(), 3, "{sessions:?}");
        assert_eq!(sessions[0].session_id, "flat-session");
        assert_eq!(sessions[1].session_id, "ts-session-1");
        assert_eq!(sessions[2].session_id, "ts-session-2");

        // And the metadata comes from state.json with the wire log size
        assert_eq!(sessions[0].work_dir.as_deref(), Some("/proj"));
        assert!(sessions[1].wire_log_size > 0);
        assert_eq!(sessions[2].wire_log_size, 0);

        // And the on-disk-but-not-advertised session is invisible
        assert!(
            !sessions.iter().any(|s| s.session_id == "hidden-session"),
            "the listing must only see the injected enumeration"
        );
    }
}
