//! Indexed query store over wire records.
//!
//! This is a **derived read model**, not a source of truth. The source of
//! truth is the [`crate::AppendLogStore`]; the query store mirrors selected
//! records into an index so that debugging, analytics, and session browsing
//! can do efficient lookups without rescanning the append log.
//!
//! Implementations:
//! - [`InMemoryQueryStore`] — for testing and small datasets.
//! - (Future) MiniDb-backed store — for production with large datasets.
//!
//! The trait also tracks per-source [`QueryCheckpoint`]s: lightweight cursor
//! offsets into the append log that let a consumer ask "give me everything
//! since checkpoint X" without re-reading the whole log.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A lightweight cursor position for incremental query over wire records.
///
/// Unlike a state snapshot (handled by [`crate::SnapshotStore`]), this is
/// just an offset into the append log — consumers can ask "give me
/// everything since checkpoint X".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryCheckpoint {
    /// The sequence number of the last record consumed.
    pub seq: u64,
}

/// Query result entry — a wire record with its sequence number.
#[derive(Debug, Clone)]
pub struct QueryEntry {
    /// The sequence number of the record in the append log.
    pub seq: u64,
    /// The wire record payload.
    pub record: serde_json::Value,
}

/// Write operation for batch writes.
#[derive(Debug, Clone)]
pub enum WriteOp {
    /// Upsert a record under `collection` + `key`.
    Put {
        collection: String,
        key: String,
        value: serde_json::Value,
    },
    /// Remove a record identified by `collection` + `key`.
    Delete { collection: String, key: String },
}

/// Indexed query store for wire records.
///
/// This is a derived read model — not the source of truth. The source of
/// truth is the [`crate::AppendLogStore`]. The query store provides
/// efficient indexed lookups for debugging, analytics, and session
/// browsing.
///
/// Implementations:
/// - [`InMemoryQueryStore`] — for testing and small datasets
/// - (Future) MiniDb-backed store — for production with large datasets
pub trait IQueryStore: Send + Sync {
    /// Put a record into the store with an optional collection tag.
    ///
    /// Overwrites any existing record stored under the same
    /// `collection` + `key` pair.
    fn put(&self, collection: &str, key: &str, value: serde_json::Value) -> Result<()>;

    /// Batch write multiple operations atomically.
    ///
    /// All operations in `ops` are applied under a single lock acquisition,
    /// so a concurrent reader observes either all of them or none of them.
    fn batch(&self, ops: &[WriteOp]) -> Result<()>;

    /// Delete a record by collection + key.
    ///
    /// Deleting a key that does not exist is a no-op, not an error.
    fn delete(&self, collection: &str, key: &str) -> Result<()>;

    /// Get a single record by collection + key.
    ///
    /// Returns `Ok(None)` when the key (or the whole collection) does not
    /// exist, so callers can treat "not present" uniformly.
    fn get(&self, collection: &str, key: &str) -> Result<Option<serde_json::Value>>;

    /// List all keys in a collection, optionally filtered by prefix.
    ///
    /// Keys are returned in lexicographic order for deterministic output.
    /// An unknown collection yields an empty list.
    fn list(&self, collection: &str, prefix: Option<&str>) -> Result<Vec<String>>;

    /// Get the current checkpoint for a given source (e.g., `"wire-log"`).
    fn get_checkpoint(&self, source: &str) -> Result<Option<QueryCheckpoint>>;

    /// Set the checkpoint for a given source after consuming records.
    ///
    /// Overwrites any previous checkpoint for the same source.
    fn set_checkpoint(&self, source: &str, checkpoint: QueryCheckpoint) -> Result<()>;

    /// Close the store, flushing any pending writes.
    ///
    /// The in-memory implementation has nothing to flush; this exists so
    /// disk-backed implementations can persist pending writes.
    fn close(&self) -> Result<()>;
}

/// An [`IQueryStore`] backed entirely by process memory.
///
/// Suitable for testing and small datasets. All mutation methods recover
/// from a poisoned lock by continuing with the tainted inner value
/// ([`RwLock::into_inner`]), the same pattern used elsewhere in the
/// codebase.
pub struct InMemoryQueryStore {
    /// `collection -> (key -> value)`.
    data: RwLock<HashMap<String, HashMap<String, serde_json::Value>>>,
    /// `source -> last-consumed checkpoint`.
    checkpoints: RwLock<HashMap<String, QueryCheckpoint>>,
}

impl InMemoryQueryStore {
    /// Create an empty in-memory query store.
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
            checkpoints: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryQueryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl IQueryStore for InMemoryQueryStore {
    fn put(&self, collection: &str, key: &str, value: serde_json::Value) -> Result<()> {
        let mut data = self
            .data
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        data.entry(collection.to_string())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    fn batch(&self, ops: &[WriteOp]) -> Result<()> {
        let mut data = self
            .data
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for op in ops {
            match op {
                WriteOp::Put {
                    collection,
                    key,
                    value,
                } => {
                    data.entry(collection.clone())
                        .or_default()
                        .insert(key.clone(), value.clone());
                }
                WriteOp::Delete { collection, key } => {
                    if let Some(collection_map) = data.get_mut(collection) {
                        collection_map.remove(key);
                    }
                }
            }
        }
        Ok(())
    }

    fn delete(&self, collection: &str, key: &str) -> Result<()> {
        let mut data = self
            .data
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(collection_map) = data.get_mut(collection) {
            collection_map.remove(key);
        }
        Ok(())
    }

    fn get(&self, collection: &str, key: &str) -> Result<Option<serde_json::Value>> {
        let data = self
            .data
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(data
            .get(collection)
            .and_then(|collection_map| collection_map.get(key))
            .cloned())
    }

    fn list(&self, collection: &str, prefix: Option<&str>) -> Result<Vec<String>> {
        let data = self
            .data
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut keys: Vec<String> = match data.get(collection) {
            Some(collection_map) => match prefix {
                Some(prefix) => collection_map
                    .keys()
                    .filter(|key| key.starts_with(prefix))
                    .cloned()
                    .collect(),
                None => collection_map.keys().cloned().collect(),
            },
            None => Vec::new(),
        };
        keys.sort();
        Ok(keys)
    }

    fn get_checkpoint(&self, source: &str) -> Result<Option<QueryCheckpoint>> {
        let checkpoints = self
            .checkpoints
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(checkpoints.get(source).cloned())
    }

    fn set_checkpoint(&self, source: &str, checkpoint: QueryCheckpoint) -> Result<()> {
        let mut checkpoints = self
            .checkpoints
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        checkpoints.insert(source.to_string(), checkpoint);
        Ok(())
    }

    fn close(&self) -> Result<()> {
        // Nothing to flush: all state lives in memory.
        Ok(())
    }
}
