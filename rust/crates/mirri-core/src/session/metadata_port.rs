//! The metadata store port — the engine's field-level session metadata write
//! face (ADR-006: the SQLite row is the metadata source of truth).
//!
//! ## Why a port
//!
//! Session metadata used to live in `state.json`: every writer read the whole
//! document, mutated its own field, and wrote the whole document back. Two
//! writers touching *different* fields therefore raced on the document, and the
//! last full-document write silently reverted the other's field. ADR-006 moves
//! the truth to the session index row and replaces the read-modify-write cycle
//! with [`MetadataPatch`]: a writer states **only the fields it touched**, so
//! disjoint writers can never clobber each other. The port is the seam that
//! keeps that invariant out of the callers' hands — there is no "write the
//! whole row" method to reach for.
//!
//! The production implementation is the SQLite session index
//! (`mirri_session_index::SessionIndexPort`); [`InMemoryMetadataStore`] is the
//! test fixture and, just as importantly, the trait's executable specification.
//!
//! ## Existence
//!
//! Row existence is the **only** judge of session existence (ADR-006): the
//! eleven former `state.json` / directory probes answer through [`exists`] and
//! [`list_ids`] instead.
//!
//! ## Read shape vs. write shape
//!
//! [`SessionMetadataRecord`] is the whole-row read-back — the rebuilder's and
//! the cold path's only legal full-field read. It carries `created_at` even
//! though no patch can write it: creation time is stamped by the store on the
//! inserting `apply`, so it is an output-only field.
//!
//! [`exists`]: MetadataStorePort::exists
//! [`list_ids`]: MetadataStorePort::list_ids

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::scope::SessionId;

/// A storage-layer failure from the metadata store.
///
/// The port is deliberately **fallible on writes**: a full disk, a `SQLITE_BUSY`
/// timeout, or a corrupt page must surface, never be swallowed into a silent
/// "the metadata says the old thing" (ADR-006's write-through callers catch
/// this and degrade to a best-effort `warn`; the port itself never pretends).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetadataStoreError {
    /// The underlying storage refused or failed the write. The string is the
    /// storage engine's own diagnosis (kept verbatim — the caller logs it).
    #[error("metadata store error: {0}")]
    Storage(String),
}

/// The field-level write face: **only** the fields named here are written.
///
/// Every field is optional, and the nullable ones are **three-valued**:
///
/// | value | meaning |
/// |---|---|
/// | `None` | leave the column exactly as it is |
/// | `Some(Some(v))` | write `v` |
/// | `Some(None)` | clear the column to NULL |
///
/// `None` is emphatically **not** "write NULL" — that is the invariant the whole
/// port exists to enforce. Two writers touching disjoint fields can never revert
/// each other, because neither ever names the other's field. The columns that
/// cannot hold NULL (`title`, `tags`, `archived`, `compressed`, `updated_at`,
/// `pinned`) stay two-valued: their "empty" is a value (`""` / `[]` / `false`),
/// so no third state exists to express.
///
/// A write point therefore states its own intent and nothing else:
///
/// ```
/// # use mirri_core::session::metadata_port::MetadataPatch;
/// // A turn settling bumps recency only — archived, title, and tags are
/// // untouched by construction, not by remembering to re-read them.
/// let settle = MetadataPatch {
///     updated_at: Some(chrono::Utc::now()),
///     ..Default::default()
/// };
///
/// // A restore clears the compression cache: three columns go to NULL.
/// let restore = MetadataPatch {
///     compressed: Some(false),
///     compressed_at: Some(None),
///     pre_compressed_size: Some(None),
///     ..Default::default()
/// };
/// # let _ = (settle, restore);
/// ```
///
/// The pin trio (`pinned` / `pinned_at` / `pin_color`) is the session
/// management domain's SDR-016/017 carrier: a pin writes exactly those three
/// fields, and unpin writes `pinned: Some(false)` with `pinned_at` / `pin_color`
/// cleared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataPatch {
    /// The display title (empty string = untitled).
    pub title: Option<String>,
    /// The free-form tag set (replaces the whole set — a set is one field;
    /// `Some(vec![])` = untagged).
    pub tags: Option<Vec<String>>,
    /// Whether the session is archived.
    pub archived: Option<bool>,
    /// When the session was archived (`Some(None)` = not archived).
    pub archived_at: Option<Option<DateTime<Utc>>>,
    /// Whether the wire log is gzip-compressed on disk (ADR-007's cache flag).
    pub compressed: Option<bool>,
    /// When the wire log was compressed (`Some(None)` = never compressed).
    pub compressed_at: Option<Option<DateTime<Utc>>>,
    /// The wire log's pre-compression byte size, the savings basis
    /// (`Some(None)` = unknown).
    pub pre_compressed_size: Option<Option<u64>>,
    /// When the session was last modified.
    pub updated_at: Option<DateTime<Utc>>,
    /// The session's creation stamp, honoured **only** by the inserting
    /// `apply` — the one that creates the row. Without it the row's stamp is
    /// the store's own `Utc::now()`, which is a second clock reading taken
    /// after the scope already fixed its `created_at`; the two differ by the
    /// creation path's duration (hundreds of microseconds), so a live read
    /// face and a cold-restored one would derive different message
    /// `createdAt` bases from the same session. Passing the scope's stamp
    /// makes the row agree with what the session actually recorded; every
    /// later patch still cannot move it.
    pub created_at: Option<DateTime<Utc>>,
    /// When the session was last *used* (SDR-004's recency key — bumped only by
    /// a user send or an explicit touch, never by a turn settle;
    /// `Some(None)` = fall back to `updated_at`).
    pub last_used_at: Option<Option<DateTime<Utc>>>,
    /// The most recent user prompt (`Some(None)` = never conversed).
    pub last_prompt: Option<Option<String>>,
    /// The working directory the session was rooted at
    /// (`Some(None)` = unknown).
    pub cwd: Option<Option<String>>,
    /// Whether the session is pinned to the top of its workspace listing
    /// (SDR-016: the row is the pin's single source of truth).
    pub pinned: Option<bool>,
    /// When the session was pinned, the pinned section's sort key
    /// (`Some(None)` = unpinned).
    pub pinned_at: Option<Option<DateTime<Utc>>>,
    /// The pin's semantic color name (SDR-017's seven names; storage passes the
    /// text through verbatim — the enumeration is enforced above this layer;
    /// `Some(None)` = no color chosen).
    pub pin_color: Option<Option<String>>,
}

/// The whole-row read-back shape — what [`MetadataStorePort::get`] returns.
///
/// This is the rebuilder's and the cold path's only legal full-field read. Note
/// the asymmetry with [`MetadataPatch`]: `created_at` is here but not there,
/// because creation time is stamped by the store on the inserting `apply` and
/// no writer may move it afterwards.
///
/// Two fields read back as their own defaults rather than as "unset": `title`
/// (a session without a title has an empty one) and `tags` (an untagged
/// session has the empty set). Everything genuinely nullable is an `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadataRecord {
    /// The display title (empty when never set).
    pub title: String,
    /// When the session was created — stamped by the store, never patched.
    pub created_at: DateTime<Utc>,
    /// When the session was last modified.
    pub updated_at: DateTime<Utc>,
    /// When the session was last *used*; `None` for a row that predates the
    /// field, in which case readers fall back to `updated_at`.
    pub last_used_at: Option<DateTime<Utc>>,
    /// Whether the session is archived.
    pub archived: bool,
    /// When the session was archived.
    pub archived_at: Option<DateTime<Utc>>,
    /// Whether the wire log is gzip-compressed on disk.
    pub compressed: bool,
    /// When the wire log was compressed.
    pub compressed_at: Option<DateTime<Utc>>,
    /// The wire log's pre-compression byte size.
    pub pre_compressed_size: Option<u64>,
    /// The free-form tag set.
    pub tags: Vec<String>,
    /// The most recent user prompt.
    pub last_prompt: Option<String>,
    /// The working directory the session was rooted at.
    pub cwd: Option<String>,
    /// Whether the session is pinned.
    pub pinned: bool,
    /// When the session was pinned.
    pub pinned_at: Option<DateTime<Utc>>,
    /// The pin's semantic color name.
    pub pin_color: Option<String>,
}

/// The metadata store seam: row existence plus field-level writes.
///
/// Implementations are shared as `Arc<dyn MetadataStorePort>` and called from
/// async handlers, hence the `Send + Sync` bound and the `#[async_trait]`
/// boxing (the production implementation holds a SQLite connection behind a
/// mutex; the async face keeps callers free to `.await` it inside handlers).
#[async_trait::async_trait]
pub trait MetadataStorePort: Send + Sync {
    /// Whether the session exists — **the** existence judgement (ADR-006: row
    /// existence, not a `state.json` probe, is what "the session is there"
    /// means).
    async fn exists(&self, session_id: &SessionId) -> bool;

    /// Read the whole row. The rebuilder's and the cold path's only legal
    /// full-field read point.
    async fn get(&self, session_id: &SessionId) -> Option<SessionMetadataRecord>;

    /// Create or update **only** the fields named in `touched`.
    ///
    /// A patch against a row that does not exist **inserts** it — session
    /// creation is the first `apply`, so the create path needs no separate
    /// method (Task 3's SQLite implementation depends on exactly this). The
    /// insert stamps `created_at`; later patches never move it.
    async fn apply(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        touched: MetadataPatch,
    ) -> Result<(), MetadataStoreError>;

    /// Delete the row and record its tombstone in **one** call (the production
    /// implementation guarantees both inside one transaction, so a crash can
    /// never leave the row gone without the deletion ledger knowing).
    async fn delete(&self, session_id: &SessionId) -> Result<(), MetadataStoreError>;

    /// Seed the whole row, **including the caller-supplied `created_at`** —
    /// the one-time migration's dedicated entry (ADR-006: `apply`'s insert
    /// stamps `created_at = now`, which would rewrite every legacy session's
    /// birthday to the migration moment and jump it to the top of every
    /// created-descending listing; seeding is the only write that may carry
    /// a historical creation time).
    ///
    /// The record is written as-is — a whole-row replacement, never a patch
    /// overlay. **Contract: the replace drops the pin columns (`pinned` /
    /// `pinned_at` / `pin_color`) and every column the record does not
    /// model** — callers must check `exists()` first and skip existing rows
    /// (`mirri migrate` does); any second caller must either preserve those
    /// columns in the record it passes or read-merge before seeding.
    /// Seeding an id that already has a row replaces it with the seeded
    /// record wholesale.
    async fn apply_seed(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        record: SessionMetadataRecord,
    ) -> Result<(), MetadataStoreError>;

    /// Every row id in a workspace — the listing, migration, and reaper
    /// judgement input.
    async fn list_ids(&self, workspace_id: &str) -> Vec<String>;
}

/// One in-memory row: the owning workspace plus the read-back record.
type Row = (String, SessionMetadataRecord);

/// The in-memory [`MetadataStorePort`] — the test fixture, and the trait's
/// executable specification.
///
/// It is a genuine implementation of the write semantics, not a stub: the
/// field-by-field overlay in [`MetadataStorePort::apply`] is the same rule the
/// SQLite implementation follows, so a test that passes here describes the
/// invariant the production store must keep.
///
/// Tombstones are retained in [`Self::tombstoned_ids`] (the port deliberately
/// does not expose them — the reaper reads the production store's own ledger).
/// Deleted rows never resurrect: a later `apply` for a tombstoned id inserts a
/// **new** row, which is the correct semantics for a genuinely new session
/// reusing an id only in tests.
pub struct InMemoryMetadataStore {
    rows: Mutex<HashMap<String, Row>>,
    tombstones: Mutex<HashSet<String>>,
}

impl InMemoryMetadataStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
            tombstones: Mutex::new(HashSet::new()),
        }
    }

    /// The ids deleted through this store — the port-semantics tests' window
    /// into the tombstone half of `delete` (the production ledger lives in the
    /// SQLite `tombstones` table).
    pub fn tombstoned_ids(&self) -> HashSet<String> {
        self.lock(&self.tombstones).clone()
    }

    /// Lock without propagating poisoning: a panic in another thread must not
    /// turn every later metadata read into a panic of its own (the store holds
    /// no invariant that a panicking writer could have half-broken).
    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InMemoryMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl MetadataStorePort for InMemoryMetadataStore {
    async fn exists(&self, session_id: &SessionId) -> bool {
        self.lock(&self.rows).contains_key(session_id.as_str())
    }

    async fn get(&self, session_id: &SessionId) -> Option<SessionMetadataRecord> {
        self.lock(&self.rows)
            .get(session_id.as_str())
            .map(|(_, record)| record.clone())
    }

    async fn apply(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        touched: MetadataPatch,
    ) -> Result<(), MetadataStoreError> {
        let mut rows = self.lock(&self.rows);
        let created_at = touched.created_at;
        let (_, record) = rows
            .entry(session_id.as_str().to_string())
            .or_insert_with(|| {
                let now = Utc::now();
                let now = created_at.unwrap_or(now);
                (
                    workspace_id.to_string(),
                    SessionMetadataRecord {
                        title: String::new(),
                        created_at: now,
                        updated_at: now,
                        last_used_at: None,
                        archived: false,
                        archived_at: None,
                        compressed: false,
                        compressed_at: None,
                        pre_compressed_size: None,
                        tags: Vec::new(),
                        last_prompt: None,
                        cwd: None,
                        pinned: false,
                        pinned_at: None,
                        pin_color: None,
                    },
                )
            });

        // The field-by-field overlay. Two rules, one per field kind:
        //
        // - two-valued columns: `Some(v)` writes `v`;
        // - nullable columns: `None` skips, `Some(None)` clears to NULL,
        //   `Some(Some(v))` writes `v`.
        //
        // Spelled out one field at a time rather than as a whole-record
        // replacement — that IS the invariant this port exists to enforce.
        if let Some(title) = touched.title {
            record.title = title;
        }
        if let Some(tags) = touched.tags {
            record.tags = tags;
        }
        if let Some(archived) = touched.archived {
            record.archived = archived;
        }
        if let Some(archived_at) = touched.archived_at {
            record.archived_at = archived_at;
        }
        if let Some(compressed) = touched.compressed {
            record.compressed = compressed;
        }
        if let Some(compressed_at) = touched.compressed_at {
            record.compressed_at = compressed_at;
        }
        if let Some(pre_compressed_size) = touched.pre_compressed_size {
            record.pre_compressed_size = pre_compressed_size;
        }
        if let Some(updated_at) = touched.updated_at {
            record.updated_at = updated_at;
        }
        if let Some(last_used_at) = touched.last_used_at {
            record.last_used_at = last_used_at;
        }
        if let Some(last_prompt) = touched.last_prompt {
            record.last_prompt = last_prompt;
        }
        if let Some(cwd) = touched.cwd {
            record.cwd = cwd;
        }
        if let Some(pinned) = touched.pinned {
            record.pinned = pinned;
        }
        if let Some(pinned_at) = touched.pinned_at {
            record.pinned_at = pinned_at;
        }
        if let Some(pin_color) = touched.pin_color {
            record.pin_color = pin_color;
        }
        Ok(())
    }

    async fn delete(&self, session_id: &SessionId) -> Result<(), MetadataStoreError> {
        let id = session_id.as_str().to_string();
        // Remove-then-tombstone reads as two critical sections; the port's
        // transaction guarantee is the production store's promise (one
        // `unchecked_transaction`), and the in-memory fixture has no crash
        // window to leave half-applied.
        self.lock(&self.rows).remove(&id);
        self.lock(&self.tombstones).insert(id);
        Ok(())
    }

    async fn apply_seed(
        &self,
        session_id: &SessionId,
        workspace_id: &str,
        record: SessionMetadataRecord,
    ) -> Result<(), MetadataStoreError> {
        // The whole record lands as-is — the caller-supplied created_at is
        // the point of the entry (the migration replays the legacy
        // document's historical birthday, never a fresh stamp).
        self.lock(&self.rows).insert(
            session_id.as_str().to_string(),
            (workspace_id.to_string(), record),
        );
        Ok(())
    }

    async fn list_ids(&self, workspace_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .lock(&self.rows)
            .iter()
            .filter(|(_, (workspace, _))| workspace == workspace_id)
            .map(|(id, _)| id.clone())
            .collect();
        // Deterministic order: the map's iteration order is unspecified, and a
        // listing consumer must not see a different answer per call.
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};

    use super::*;

    fn session_id(id: &str) -> SessionId {
        SessionId::new(id)
    }

    fn title_only(title: &str) -> MetadataPatch {
        MetadataPatch {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    /// Two writers, disjoint fields, interleaved arrival: neither may revert
    /// the other. This is ADR-006's whole reason for existing.
    #[tokio::test]
    async fn should_keep_both_fields_when_concurrent_patches_touch_different_fields() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");

        store
            .apply(&id, "ws-1", title_only("Renamed"))
            .await
            .expect("the first patch applies");
        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    archived: Some(true),
                    ..Default::default()
                },
            )
            .await
            .expect("the second patch applies");

        let record = store.get(&id).await.expect("the row exists");
        assert_eq!(record.title, "Renamed");
        assert!(record.archived);
    }

    #[tokio::test]
    async fn should_not_list_after_delete_when_tombstoned() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        store
            .apply(&id, "ws-1", title_only("Doomed"))
            .await
            .expect("the row is created");
        assert!(store.exists(&id).await);

        store.delete(&id).await.expect("the delete succeeds");

        assert!(!store.exists(&id).await, "a deleted row no longer exists");
        assert!(
            store.list_ids("ws-1").await.is_empty(),
            "the workspace listing no longer carries the deleted row"
        );
        assert!(
            store.tombstoned_ids().contains("sess-1"),
            "the delete leaves a tombstone for the reaper"
        );
    }

    /// The create path depends on this: a session's first write is an `apply`
    /// against a row that does not exist yet (Task 5's materialize).
    #[tokio::test]
    async fn should_insert_a_row_when_apply_targets_a_missing_session() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        assert!(!store.exists(&id).await);

        store
            .apply(&id, "ws-1", title_only("Fresh"))
            .await
            .expect("the insert succeeds");

        assert!(store.exists(&id).await, "apply creates the missing row");
        assert_eq!(store.list_ids("ws-1").await, vec!["sess-1".to_string()]);
    }

    /// `created_at` is stamped once, by the inserting write; later patches must
    /// never move it (it is the session's birthday, not its last write).
    #[tokio::test]
    async fn should_preserve_created_at_when_a_later_patch_applies() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        store
            .apply(&id, "ws-1", title_only("First"))
            .await
            .expect("the row is created");
        let created_at = store.get(&id).await.expect("the row exists").created_at;

        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    last_used_at: Some(Some(Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap())),
                    ..Default::default()
                },
            )
            .await
            .expect("the patch applies");

        let record = store.get(&id).await.expect("the row exists");
        assert_eq!(record.created_at, created_at);
        assert_eq!(
            record.last_used_at,
            Some(Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap())
        );
    }

    /// The seed entry is the ONE write that may carry a historical
    /// `created_at`: the migration replays the legacy document's birthday
    /// verbatim, never a fresh stamp (the whole reason the entry exists —
    /// `apply`'s insert would stamp now and jump every legacy session to
    /// the top of the created-descending listing).
    #[tokio::test]
    async fn should_seed_the_callers_created_at_verbatim() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-legacy");
        let birthday = Utc.with_ymd_and_hms(2020, 5, 4, 3, 2, 1).unwrap();
        store
            .apply_seed(
                &id,
                "ws-1",
                SessionMetadataRecord {
                    title: "legacy title".to_string(),
                    created_at: birthday,
                    updated_at: birthday,
                    last_used_at: None,
                    archived: false,
                    archived_at: None,
                    compressed: false,
                    compressed_at: None,
                    pre_compressed_size: None,
                    tags: vec!["legacy".to_string()],
                    last_prompt: None,
                    cwd: Some("/w/legacy".to_string()),
                    pinned: false,
                    pinned_at: None,
                    pin_color: None,
                },
            )
            .await
            .expect("the seed succeeds");

        let record = store.get(&id).await.expect("the seeded row exists");
        assert_eq!(
            record.created_at, birthday,
            "the seed carries the caller's historical created_at verbatim"
        );
        assert_eq!(record.title, "legacy title");
        assert_eq!(record.cwd.as_deref(), Some("/w/legacy"));
        assert_eq!(record.tags, vec!["legacy".to_string()]);

        // And a LATER patch still never moves the seeded birthday (the
        // seeded value is exactly as protected as a store-stamped one).
        store
            .apply(&id, "ws-1", title_only("Renamed after seed"))
            .await
            .expect("the post-seed patch applies");
        let record = store.get(&id).await.expect("the row exists");
        assert_eq!(record.created_at, birthday);
        assert_eq!(record.title, "Renamed after seed");
    }

    #[tokio::test]
    async fn should_list_only_the_workspace_rows_when_ids_span_workspaces() {
        let store = InMemoryMetadataStore::new();
        for (id, workspace) in [("sess-1", "ws-1"), ("sess-2", "ws-2"), ("sess-3", "ws-1")] {
            store
                .apply(&session_id(id), workspace, title_only(id))
                .await
                .expect("the row is created");
        }

        assert_eq!(
            store.list_ids("ws-1").await,
            vec!["sess-1".to_string(), "sess-3".to_string()]
        );
        assert_eq!(store.list_ids("ws-2").await, vec!["sess-2".to_string()]);
    }

    /// Every field the patch can carry must survive a round-trip: the record is
    /// the read-back face the rebuilder and the cold listing consume.
    #[tokio::test]
    async fn should_round_trip_every_patch_field_when_reading_the_record_back() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        let stamp = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();

        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    title: Some("Title".to_string()),
                    tags: Some(vec!["alpha".to_string(), "beta".to_string()]),
                    archived: Some(true),
                    archived_at: Some(Some(stamp)),
                    compressed: Some(true),
                    compressed_at: Some(Some(stamp)),
                    pre_compressed_size: Some(Some(4096)),
                    updated_at: Some(stamp),
                    created_at: Some(stamp),
                    last_used_at: Some(Some(stamp)),
                    last_prompt: Some(Some("hello".to_string())),
                    cwd: Some(Some("/Users/dev/project".to_string())),
                    pinned: Some(true),
                    pinned_at: Some(Some(stamp)),
                    pin_color: Some(Some("red".to_string())),
                },
            )
            .await
            .expect("the full patch applies");

        let record = store.get(&id).await.expect("the row exists");
        assert_eq!(record.title, "Title");
        assert_eq!(record.tags, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(record.archived);
        assert_eq!(record.archived_at, Some(stamp));
        assert!(record.compressed);
        assert_eq!(record.compressed_at, Some(stamp));
        assert_eq!(record.pre_compressed_size, Some(4096));
        assert_eq!(record.updated_at, stamp);
        assert_eq!(record.last_used_at, Some(stamp));
        assert_eq!(record.last_prompt.as_deref(), Some("hello"));
        assert_eq!(record.cwd.as_deref(), Some("/Users/dev/project"));
        assert!(record.pinned);
        assert_eq!(record.pinned_at, Some(stamp));
        assert_eq!(record.pin_color.as_deref(), Some("red"));
    }

    /// The tri-state clear: `Some(None)` writes NULL. Every nullable column is
    /// exercised in one patch, so a field that keeps its two-state overlay (and
    /// silently ignores the clear) fails here rather than at the first retention
    /// restore.
    #[tokio::test]
    async fn should_clear_a_nullable_column_when_patch_carries_some_none() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        let stamp = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();

        // Given a row whose nullable columns all hold a value.
        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    archived_at: Some(Some(stamp)),
                    compressed_at: Some(Some(stamp)),
                    pre_compressed_size: Some(Some(4096)),
                    last_used_at: Some(Some(stamp)),
                    last_prompt: Some(Some("hello".to_string())),
                    cwd: Some(Some("/Users/dev/project".to_string())),
                    pinned_at: Some(Some(stamp)),
                    pin_color: Some(Some("red".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("the seeding patch applies");

        // When every nullable column is named with `Some(None)`.
        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    archived_at: Some(None),
                    compressed_at: Some(None),
                    pre_compressed_size: Some(None),
                    last_used_at: Some(None),
                    last_prompt: Some(None),
                    cwd: Some(None),
                    pinned_at: Some(None),
                    pin_color: Some(None),
                    ..Default::default()
                },
            )
            .await
            .expect("the clearing patch applies");

        // Then every one of them reads back as NULL.
        let record = store.get(&id).await.expect("the row exists");
        assert_eq!(record.archived_at, None);
        assert_eq!(record.compressed_at, None);
        assert_eq!(record.pre_compressed_size, None);
        assert_eq!(record.last_used_at, None);
        assert_eq!(record.last_prompt, None);
        assert_eq!(record.cwd, None);
        assert_eq!(record.pinned_at, None);
        assert_eq!(record.pin_color, None);
    }

    /// The other half of the tri-state contract: an absent field (`None`) never
    /// writes — not even when every nullable column happens to be NULL already,
    /// where a sloppy "write the patch's value unconditionally" overlay would
    /// look identical.
    #[tokio::test]
    async fn should_not_touch_column_when_patch_carries_none() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        let stamp = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();

        // Given every column holds a value.
        store
            .apply(
                &id,
                "ws-1",
                MetadataPatch {
                    title: Some("Title".to_string()),
                    tags: Some(vec!["alpha".to_string()]),
                    archived: Some(true),
                    archived_at: Some(Some(stamp)),
                    compressed: Some(true),
                    compressed_at: Some(Some(stamp)),
                    pre_compressed_size: Some(Some(4096)),
                    updated_at: Some(stamp),
                    created_at: Some(stamp),
                    last_used_at: Some(Some(stamp)),
                    last_prompt: Some(Some("hello".to_string())),
                    cwd: Some(Some("/Users/dev/project".to_string())),
                    pinned: Some(true),
                    pinned_at: Some(Some(stamp)),
                    pin_color: Some(Some("red".to_string())),
                },
            )
            .await
            .expect("the seeding patch applies");
        let before = store.get(&id).await.expect("the row exists");

        // When a patch names one field and omits all the others.
        store
            .apply(&id, "ws-1", title_only("Renamed"))
            .await
            .expect("the narrow patch applies");

        // Then only that field moved; everything else is byte-identical.
        let after = store.get(&id).await.expect("the row exists");
        assert_eq!(after.title, "Renamed");
        assert_eq!(after.tags, before.tags);
        assert_eq!(after.archived, before.archived);
        assert_eq!(after.archived_at, before.archived_at);
        assert_eq!(after.compressed, before.compressed);
        assert_eq!(after.compressed_at, before.compressed_at);
        assert_eq!(after.pre_compressed_size, before.pre_compressed_size);
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(after.last_used_at, before.last_used_at);
        assert_eq!(after.last_prompt, before.last_prompt);
        assert_eq!(after.cwd, before.cwd);
        assert_eq!(after.pinned, before.pinned);
        assert_eq!(after.pinned_at, before.pinned_at);
        assert_eq!(after.pin_color, before.pin_color);
        assert_eq!(after.created_at, before.created_at);
    }

    /// Delete is idempotent (the reaper and the delete path may both fire) and
    /// a repeat delete must not resurrect the row.
    #[tokio::test]
    async fn should_stay_deleted_when_delete_is_called_twice() {
        let store = InMemoryMetadataStore::new();
        let id = session_id("sess-1");
        store
            .apply(&id, "ws-1", title_only("Doomed"))
            .await
            .expect("the row is created");

        store.delete(&id).await.expect("the first delete succeeds");
        store.delete(&id).await.expect("the second delete succeeds");

        assert!(!store.exists(&id).await);
        assert!(store.get(&id).await.is_none());
    }

    /// The port object is injected as `Arc<dyn MetadataStorePort>`; it must be
    /// usable across a thread boundary (hence `Send + Sync`).
    #[tokio::test]
    async fn should_be_usable_behind_a_trait_object_when_shared_across_tasks() {
        let store: Arc<dyn MetadataStorePort> = Arc::new(InMemoryMetadataStore::new());
        let id = session_id("sess-1");

        let writer = {
            let store = Arc::clone(&store);
            let id = id.clone();
            tokio::spawn(async move { store.apply(&id, "ws-1", title_only("Shared")).await })
        };
        writer
            .await
            .expect("the writer task joins")
            .expect("the patch applies");

        assert!(store.exists(&id).await);
        assert_eq!(
            store.get(&id).await.expect("the row exists").title,
            "Shared"
        );
    }
}
