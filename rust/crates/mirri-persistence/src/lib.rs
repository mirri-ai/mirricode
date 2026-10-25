//! Persistence layer for the Mirri Rust kernel.
//!
//! This crate provides the append-only log store that reads and writes
//! `wire.jsonl` files — the durable record of every wire protocol op
//! produced during an agent session — plus the atomic document store
//! and snapshot store for crash-safe state checkpointing, and an
//! indexed query store for efficient lookups over wire records.

pub mod append_log;
pub mod atomic_doc;
pub mod compress;
pub mod dir_lock;
pub mod file_store;
pub mod fs_utils;
pub mod migration;
pub mod provider_file_cache;
pub mod query_store;
pub mod snapshot;

pub use append_log::{AppendLogError, AppendLogStore, JsonlAppendLog};
pub use atomic_doc::AtomicDocument;
pub use compress::{
    CompressedShape, LineRead, WIRE_LOG_FILE, WIRE_LOG_GZ_FILE, open_wire_log_lines,
    probe_compressed, read_gz_to_vec,
};
pub use dir_lock::DirLockGuard;
pub use file_store::{FileMeta, FileStore, SaveOptions};
pub use fs_utils::{is_file, read_dir_entries, remove_file_if_exists, write_file_atomically};
pub use migration::{
    CURRENT_VERSION, WireMigration, detect_version, is_forward_version, migrate_log_file,
    migrate_record, migrate_records_in_memory, resolve_migrations,
};
pub use provider_file_cache::{CachedProviderFile, ProviderFileCache};
pub use query_store::{IQueryStore, InMemoryQueryStore, QueryCheckpoint, QueryEntry, WriteOp};
pub use snapshot::{Snapshot, SnapshotStore};
