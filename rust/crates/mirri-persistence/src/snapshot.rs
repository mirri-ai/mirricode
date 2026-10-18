//! Snapshot store: checkpoint an [`AgentState`] to disk atomically and
//! restore it later, optionally replaying remaining wire ops from the
//! append log for incremental recovery.
//!
//! The snapshot is written via [`AtomicDocument`] (write-to-temp + rename),
//! so a crash at any point leaves the file either in its previous state or
//! fully in its new state — never partially written.

use std::path::Path;

use anyhow::Result;
use mirri_proto::AgentState;
use serde::{Deserialize, Serialize};

use crate::atomic_doc::AtomicDocument;

/// A serialisable snapshot of agent state at a given op index.
///
/// The `op_index` records how many wire ops have been folded into `state`,
/// so the caller knows where to start incremental replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot format version (currently 1).
    pub version: u32,
    /// Number of wire ops folded into this snapshot's state.
    pub op_index: u64,
    /// The full agent state at this checkpoint.
    pub state: AgentState,
    /// RFC 3339 timestamp of when the snapshot was created.
    pub created_at: String,
}

/// Write and read [`AgentState`] snapshots atomically.
///
/// Usage pattern:
/// 1. Periodically call [`SnapshotStore::write`] after N ops.
/// 2. On restart, call [`SnapshotStore::read`]. If it returns `Some`,
///    replay ops from `op_index` onward using
///    [`AppendLogStore::read_from`](crate::AppendLogStore::read_from).
/// 3. If it returns `None` (no snapshot exists), do a full replay from
///    op 0.
pub struct SnapshotStore;

impl SnapshotStore {
    /// Atomically write a snapshot of `state` at `op_index` to `path`.
    ///
    /// The write is durable: the temp file is synced to disk before the
    /// rename, so once this returns successfully the snapshot is on stable
    /// storage.
    pub async fn write(path: &Path, state: &AgentState, op_index: u64) -> Result<()> {
        let snapshot = Snapshot {
            version: 1,
            op_index,
            state: state.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        AtomicDocument::write(path, &snapshot).await
    }

    /// Read a snapshot from `path`.
    ///
    /// Returns `Ok(None)` when the file does not exist or is empty, so
    /// callers can treat "first run" uniformly without special error
    /// handling.
    ///
    /// Returns `Ok(Some((state, op_index)))` on success.
    pub async fn read(path: &Path) -> Result<Option<(AgentState, u64)>> {
        match AtomicDocument::read::<Snapshot>(path).await? {
            Some(snapshot) => Ok(Some((snapshot.state, snapshot.op_index))),
            None => Ok(None),
        }
    }

    /// The blocking-pool twin of [`SnapshotStore::read`] for callers
    /// already running inside `spawn_blocking` (the wire-log replay core):
    /// same semantics — `Ok(None)` on missing/empty, `Ok(Some((state,
    /// op_index)))` on success — without an await point.
    pub fn read_blocking(path: &Path) -> Result<Option<(AgentState, u64)>> {
        match AtomicDocument::read_blocking::<Snapshot>(path)? {
            Some(snapshot) => Ok(Some((snapshot.state, snapshot.op_index))),
            None => Ok(None),
        }
    }
}
