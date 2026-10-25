//! Blob reference ledger — the session-retention domain's attachment
//! reclamation bookkeeping (architecture §3.4, ADR-003).
//!
//! Content-addressed blobs (`{home}/blobs/{sha256}[.ext]`, see
//! [`crate::blob`]) are **shared across sessions by hash**: two sessions
//! that offloaded the same image reference one file. Deleting a session
//! must therefore never delete a blob another session still references.
//!
//! The ledger is the lazy reverse-lookup list the ADR-003 decision names:
//!
//! - [`extract_blob_refs`] scans **every** record stream of one session
//!   directory — the main `wire.jsonl` (either on-disk shape, via the
//!   persistence domain's unified dual-shape read), every
//!   `agents/<agent_id>/wire.jsonl` (also dual-shape), and `snapshot.json`
//!   (the folded checkpoint may carry `blobref:` state) — and returns the
//!   set of blob hashes the session references. Missing any one stream
//!   would under-count and delete a shared attachment.
//! - [`BlobLedger::build`] counts, per hash, how many live sessions
//!   reference it (the first full pass over all session roots).
//! - [`BlobLedger::decrement`] runs before a session directory is
//!   deleted: the session's references come off the counts, and hashes
//!   whose count just reached zero get the **zeroed-since anchor**
//!   stamped. The returned set is exactly the newly-zeroed hashes.
//! - [`BlobLedger::reclaim_aged`] runs from the tail segment of the
//!   retention compression check cycle (the recheck host, Task 8 wiring):
//!   hashes whose anchor is older than the aging window get their blob
//!   file deleted and their ledger row removed.
//!
//! ## Why the anchor is the zero-crossing moment (ADR-003, closing the
//! v1 TOCTOU)
//!
//! Content-addressed blobs are immutable: a file's creation time is
//! always earlier than any window, so an mtime anchor protects nothing.
//! Reuse of an old attachment necessarily writes a **new reference**
//! (`blobref:` in a record stream), which drives the count off zero and
//! resets the anchor — both cross-process (a foreign process's reference
//! lands within the hour-scale window and the next recheck sees it) and
//! in-process ([`BlobLedger::note_reference`], called from the
//! new-reference path, increments and clears the anchor).
//!
//! ## One-way consistency boundary
//!
//! "Blob lingers" (space wasted, nothing broken) is allowed; "blob
//! deleted while a reference survives" (US2.4) is not. Every design
//! choice above errs toward the former: a missing stream read fails the
//! extract loudly, and a zero crossing only ever *defers* deletion.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use mirri_persistence::{
    AtomicDocument, is_file, open_wire_log_lines, read_dir_entries, remove_file_if_exists,
};

use crate::blob::BLOBREF_PROTOCOL;

/// The default attachment aging window (§3.4: T, hour-scale constant) —
/// a hash whose reference count reached zero stays on disk at least this
/// long before [`BlobLedger::reclaim_aged`] may delete its file.
///
/// The recheck itself takes the window as a parameter (tests inject
/// shorter windows); this constant is the production default.
pub const BLOB_AGING_WINDOW: Duration = Duration::from_secs(6 * 3600);

/// The `snapshot.json` file name inside a session directory.
const SNAPSHOT_FILE: &str = "snapshot.json";

/// The agents sub-directory inside a session directory (`agents/`).
const AGENTS_DIR: &str = "agents";

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

/// Extract every blob hash a session directory references — the full
/// record-stream scan (§3.4 引用提取范围).
///
/// Scans, in order:
///
/// 1. the main `wire.jsonl` in either on-disk shape (plain or `.gz`,
///    through the persistence domain's dual-shape line reader —
///    [`mirri_persistence::open_wire_log_lines`]),
/// 2. every `agents/<agent_id>/wire.jsonl(.gz)` sub-stream (also
///    dual-shape),
/// 3. `snapshot.json`, parsed as JSON and walked recursively for
///    `blobref:` string values (a folded checkpoint can carry blob
///    references in its serialized state).
///
/// A stream that fails to read fails the whole extract — an unreadable
/// stream's references are unknown, and treating them as absent could
/// delete a shared attachment (the one-way boundary). A session
/// directory with no streams at all (already deleted, or a fresh
/// directory) simply yields an empty set: absent files are not errors.
///
/// Each line's scan is line-buffered regex-free: `blobref:` URIs are
/// located by prefix and their hash suffix taken after the last `;` or
/// `/` separator — the same acceptance shape [`crate::blob::parse_blob_ref`]
/// uses, kept string-level because the lines are heterogeneous JSON
/// records, not typed blob refs.
pub fn extract_blob_refs(session_dir: &Path) -> std::io::Result<HashSet<String>> {
    let mut refs = HashSet::new();

    // Main wire log — dual shape (plain or .gz).
    collect_refs_from_wire_log(session_dir, &mut refs)?;

    // Every agents/<agent_id>/ sub-stream — dual shape each.
    let agents_dir = session_dir.join(AGENTS_DIR);
    for entry in read_dir_entries(&agents_dir)? {
        if entry.is_dir {
            collect_refs_from_wire_log(&entry.path, &mut refs)?;
        }
    }

    // snapshot.json — recursive string walk for blobref: values.
    let snapshot_path = session_dir.join(SNAPSHOT_FILE);
    if let Some(value) =
        AtomicDocument::read_blocking::<serde_json::Value>(&snapshot_path).map_err(io_other)?
    {
        collect_refs_from_json(&value, &mut refs);
    }

    Ok(refs)
}

/// Scan one directory's `wire.jsonl` in either on-disk shape and add
/// every referenced blob hash to `refs`.
fn collect_refs_from_wire_log(dir: &Path, refs: &mut HashSet<String>) -> std::io::Result<()> {
    let mut reader = open_wire_log_lines(dir)?;
    let mut line = String::new();
    while reader.read_line(&mut line)? {
        collect_refs_from_text(&line, refs);
    }
    Ok(())
}

/// Scan one JSON value recursively for `blobref:` strings and add their
/// hashes to `refs`.
fn collect_refs_from_json(value: &serde_json::Value, refs: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(text) => collect_refs_from_text(text, refs),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_refs_from_json(item, refs);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_refs_from_json(item, refs);
            }
        }
        _ => {}
    }
}

/// Scan one line/string for embedded `blobref:` URIs and add their hash
/// components to `refs`.
///
/// A reference may sit anywhere inside a record's JSON text (a nested
/// content part's URL field), so every occurrence counts. The hash is the
/// suffix after the LAST `;` or `/` separator of the occurrence — the
/// [`crate::blob::parse_blob_ref`] acceptance shape; a trailing
/// separator (empty hash) is not a reference and is skipped. The scan
/// stops at the next `"` or whitespace so JSON structure delimiters end
/// the URI naturally.
fn collect_refs_from_text(text: &str, refs: &mut HashSet<String>) {
    let mut rest = text;
    while let Some(pos) = rest.find(BLOBREF_PROTOCOL) {
        let after = &rest[pos + BLOBREF_PROTOCOL.len()..];
        let Some(hash) = blob_hash_of(after) else {
            // No separator (or empty hash) after this prefix — not a
            // well-formed reference; continue scanning past it.
            rest = after;
            continue;
        };
        refs.insert(hash.to_string());
        // Continue after this occurrence.
        rest = after;
    }
}

/// The hash component of the text following a `blobref:` prefix: the
/// substring up to the first JSON delimiter (quote) or whitespace,
/// validated to hold a `;`/`/` separator with a non-empty hash after it.
fn blob_hash_of(after_prefix: &str) -> Option<&str> {
    let uri: &str = after_prefix
        .split(['"', ' ', '\t', '\n', '\r'])
        .next()
        .unwrap_or("");
    let separator_pos = match (uri.rfind(';'), uri.rfind('/')) {
        (Some(semi), Some(slash)) => semi.max(slash),
        (Some(semi), None) => semi,
        (None, Some(slash)) => slash,
        (None, None) => return None,
    };
    let hash = &uri[separator_pos + 1..];
    if hash.is_empty() {
        return None;
    }
    Some(hash)
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// The per-hash ledger row: how many live sessions reference the blob,
/// and — once that count crosses zero — when it crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefState {
    /// The live reference count (how many sessions currently reference
    /// the hash).
    refcount: usize,
    /// When the refcount last reached zero — the ADR-003 aging anchor.
    /// `None` while the count is positive.
    zeroed_since: Option<DateTime<Utc>>,
}

/// The in-process lazy reverse-lookup list: blob hash → reference
/// count, with zero-crossing anchors (ADR-003).
///
/// The ledger is a cache, not a correctness dependency: it can be
/// dropped and rebuilt ([`BlobLedger::build`]) from the session
/// directories on disk, which remain the only source of truth. Sharing
/// goes through internal `Arc<Mutex<..>>` mutability so the embedded
/// wiring (the delete path, the compression-cycle recheck) can hold one
/// handle from several call sites.
///
/// Time is taken from an injectable clock closure — production passes a
/// wall-clock reader, tests pass fixed times for deterministic
/// window-elapsed assertions.
pub struct BlobLedger {
    inner: Arc<Mutex<HashMap<String, RefState>>>,
    /// Where blob files live (`{home}/blobs`).
    blobs_dir: PathBuf,
    /// The session roots the ledger was built over — retained so the
    /// recheck pass's disk-truth re-scan (B2) re-walks the exact roots
    /// the build counted.
    sessions_roots: Vec<PathBuf>,
    /// The clock the anchor stamps read.
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl BlobLedger {
    /// Build the ledger's first full pass: enumerate every session
    /// directory under each root and count each hash's referencing
    /// sessions.
    ///
    /// A root entry is a session directory when it holds a
    /// `state.json` (the flat layout); when it does not, its
    /// sub-directories are probed as the TS workDir-bucketed layout
    /// (`<root>/<bucket>/<session_id>/`), mirroring the persistence
    /// domain's listing convention. A bucketed sub-directory is scanned
    /// without a `state.json` check — a session directory missing its
    /// `state.json` (a torn delete) still owns its blob references, and
    /// under-counting them could delete a shared attachment.
    ///
    /// A session whose extract fails (an unreadable stream) is an
    /// error, per [`extract_blob_refs`]'s loud-failure stance.
    pub fn build(blobs_dir: &Path, sessions_roots: &[PathBuf]) -> std::io::Result<Self> {
        let mut counts: HashMap<String, RefState> = HashMap::new();
        for root in sessions_roots {
            for entry in read_dir_entries(root)? {
                if !entry.is_dir {
                    continue;
                }
                if is_file(&entry.path.join("state.json")) {
                    for hash in extract_blob_refs(&entry.path)? {
                        increment(&mut counts, hash);
                    }
                } else {
                    // Bucketed layout: each sub-directory is a session.
                    for sub in read_dir_entries(&entry.path)? {
                        if sub.is_dir {
                            for hash in extract_blob_refs(&sub.path)? {
                                increment(&mut counts, hash);
                            }
                        }
                    }
                }
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(counts)),
            blobs_dir: blobs_dir.to_path_buf(),
            sessions_roots: sessions_roots.to_vec(),
            clock: Arc::new(Utc::now),
        })
    }

    /// Build with an injected clock (the test seam for deterministic
    /// window-elapsed assertions).
    pub fn build_with_clock(
        blobs_dir: &Path,
        sessions_roots: &[PathBuf],
        clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    ) -> std::io::Result<Self> {
        let mut ledger = Self::build(blobs_dir, sessions_roots)?;
        ledger.clock = clock;
        Ok(ledger)
    }

    /// A handle sharing this ledger's counts (the embedded wiring's
    /// multi-site sharing form).
    pub fn shared(&self) -> SharedBlobLedger {
        SharedBlobLedger {
            inner: Arc::clone(&self.inner),
            blobs_dir: self.blobs_dir.clone(),
            sessions_roots: self.sessions_roots.clone(),
            clock: Arc::clone(&self.clock),
        }
    }

    /// Remove a deleted session's references: decrement each hash the
    /// session referenced and stamp the zero-crossing anchor on hashes
    /// that just reached zero.
    ///
    /// Returns the set of hashes that **became zero with this call** —
    /// the caller's "newly orphaned" report. Hashes already at zero
    /// (double-decrement protection) or still positive are not returned.
    ///
    /// A decrement on a hash the ledger has no row for inserts a
    /// zero-anchored row — the hash was never counted (a ledger built
    /// before the session existed, or a foreign process's session):
    /// erring toward "already unclaimed" keeps the one-way boundary
    /// intact without inventing references.
    pub fn decrement(&self, session_dir: &Path) -> std::io::Result<HashSet<String>> {
        let refs = extract_blob_refs(session_dir)?;
        let now = (self.clock)();
        let mut newly_zeroed = HashSet::new();
        let mut inner = lock(&self.inner);
        for hash in refs {
            let state = inner.entry(hash.clone()).or_insert(RefState {
                refcount: 0,
                zeroed_since: None,
            });
            if state.refcount > 0 {
                state.refcount -= 1;
            }
            if state.refcount == 0 && state.zeroed_since.is_none() {
                state.zeroed_since = Some(now);
                newly_zeroed.insert(hash);
            }
        }
        Ok(newly_zeroed)
    }

    /// Note a new in-process reference (the reuse anchor reset): the
    /// new-reference write path calls this so a hash leaving zero clears
    /// its anchor (ADR-003's reuse-always-writes-a-new-reference rule,
    /// made visible to the ledger that cannot see the wire log append
    /// itself).
    ///
    /// The reference hook is process-wide and optional (see
    /// [`set_reference_hook`]): when the retention domain is assembled,
    /// the embedded layer parks a hook here so every blob write the
    /// engine performs resets the shared ledger's anchor — closing the
    /// in-process TOCTOU window (a zeroed hash re-referenced by a NEW
    /// session must not be reclaimed by a concurrent recheck pass that
    /// still sees the stale zero anchor).
    pub fn note_reference(&self, hash: &str) {
        let mut inner = lock(&self.inner);
        let state = inner.entry(hash.to_string()).or_insert(RefState {
            refcount: 0,
            zeroed_since: None,
        });
        state.refcount += 1;
        state.zeroed_since = None;
    }

    /// The recheck pass (§3.4 复核宿主): delete the blob files of every
    /// hash whose zero-crossing anchor is older than `older_than` and
    /// remove their ledger rows. Returns the reclaimed hashes.
    ///
    /// The window is a parameter so tests can inject shorter spans;
    /// production passes [`BLOB_AGING_WINDOW`]. A hash re-referenced
    /// after its zero crossing ([`BlobLedger::note_reference`]) has no
    /// anchor and is never reclaimed here.
    ///
    /// The recheck host is the compression check cycle's tail segment
    /// (Task 8 wiring): when the retention feature is off there is no
    /// cycle, no new zero crossings occur, and already-zeroed items
    /// simply stay put — the allowed side of the one-way boundary.
    ///
    /// ## The disk-truth recheck (B2: the in-memory table is a cache)
    ///
    /// A due hash is NEVER deleted on the memory table's word alone:
    /// before the file goes, the sessions roots are re-scanned for any
    /// still-live reference to the hash (磁盘是唯一事实源). This closes
    /// every window where the in-memory count went stale — a session
    /// created (or re-referenced) after the ledger was built, a foreign
    /// process's session the ledger never counted, or a missed
    /// [`BlobLedger::note_reference`] on a reuse. The one-way boundary
    /// ("blob deleted while a reference survives" is forbidden) outranks
    /// the reclamation goal: a hash found referenced on disk keeps a
    /// clean row (count restored, anchor cleared) and a later pass
    /// re-judges it.
    ///
    /// A blob file that is already gone (a foreign process reclaimed
    /// it) still drops its ledger row: the deletion goal is met either
    /// way. Hashes with no anchor (still referenced) are untouched.
    pub fn reclaim_aged(&self, older_than: Duration) -> Vec<String> {
        let now = (self.clock)();
        let mut inner = lock(&self.inner);
        let mut reclaimed = Vec::new();
        let mut due: Vec<String> = Vec::new();
        for (hash, state) in inner.iter() {
            if let Some(anchor) = state.zeroed_since
                && now
                    .signed_duration_since(anchor)
                    .to_std()
                    .unwrap_or_default()
                    >= older_than
            {
                due.push(hash.clone());
            }
        }
        for hash in &due {
            inner.remove(hash);
        }
        drop(inner);
        if due.is_empty() {
            return reclaimed;
        }
        // The disk-truth recheck (B2): one scan over the session roots
        // decides which due hashes still carry a live reference. A
        // FAILED scan reclaims nothing — the rows are restored with their
        // anchors so a later pass re-judges (deleting on an unknown
        // reference state is the forbidden side of the one-way boundary).
        match self.scan_live_references(&due) {
            Ok(verdicts) => {
                let referenced: HashSet<String> = verdicts
                    .iter()
                    .filter(|(_, live)| *live)
                    .map(|(hash, _)| hash.clone())
                    .collect();
                if !referenced.is_empty() {
                    self.restore_referenced_rows(&referenced);
                }
                for hash in &due {
                    if referenced.contains(hash) {
                        continue; // a live session still holds this blob
                    }
                    delete_blob_file(&self.blobs_dir, hash);
                    reclaimed.push(hash.clone());
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "blob recheck: the disk re-scan failed; nothing is reclaimed this \
                     pass (the rows keep their anchors for a later re-judgement)"
                );
                // Restore the due rows with fresh anchors so the next pass
                // re-judges from now (the original anchors are lost here —
                // the conservative direction: the window restarts, the blob
                // lingers, nothing is deleted on an unknown state).
                let mut inner = lock(&self.inner);
                for hash in &due {
                    inner.insert(
                        hash.clone(),
                        RefState {
                            refcount: 0,
                            zeroed_since: Some(now),
                        },
                    );
                }
            }
        }
        reclaimed
    }

    /// The disk-truth recheck (B2): re-extract every session directory's
    /// references under the ledger's roots and report, for each probed
    /// hash, whether any live session still references it.
    ///
    /// `Err` (a failed extract on any session) fails the whole scan —
    /// the caller must treat the verdict as "unknown" and reclaim
    /// nothing: an unreadable stream's references are unknown, and
    /// treating them as absent could delete a shared attachment.
    fn scan_live_references(&self, hashes: &[String]) -> std::io::Result<Vec<(String, bool)>> {
        let probed: HashSet<&str> = hashes.iter().map(String::as_str).collect();
        let mut live: Vec<(String, bool)> = hashes.iter().map(|h| (h.clone(), false)).collect();
        let mut referenced: HashSet<String> = HashSet::new();
        for root in &self.sessions_roots {
            for entry in read_dir_entries(root)? {
                if !entry.is_dir {
                    continue;
                }
                if is_file(&entry.path.join("state.json")) {
                    for hash in extract_blob_refs(&entry.path)? {
                        if probed.contains(hash.as_str()) {
                            referenced.insert(hash);
                        }
                    }
                } else {
                    // Bucketed layout: each sub-directory is a session.
                    for sub in read_dir_entries(&entry.path)? {
                        if sub.is_dir {
                            for hash in extract_blob_refs(&sub.path)? {
                                if probed.contains(hash.as_str()) {
                                    referenced.insert(hash);
                                }
                            }
                        }
                    }
                }
            }
        }
        for verdict in &mut live {
            verdict.1 = referenced.contains(verdict.0.as_str());
        }
        Ok(live)
    }

    /// Restore (rebuild from disk) the ledger rows for the recheck's
    /// refused hashes — a hash the disk re-scan found still referenced
    /// gets its count back and its anchor cleared, so a later pass
    /// re-judges it from a fresh zero crossing.
    fn restore_referenced_rows(&self, referenced: &HashSet<String>) {
        let mut inner = lock(&self.inner);
        for hash in referenced {
            inner.insert(
                hash.clone(),
                RefState {
                    refcount: 1,
                    zeroed_since: None,
                },
            );
        }
    }
}

/// The shared-handle form of [`BlobLedger`] — same operations, cloneable
/// across call sites (the embedded delete path and the cycle recheck).
#[derive(Clone)]
pub struct SharedBlobLedger {
    inner: Arc<Mutex<HashMap<String, RefState>>>,
    blobs_dir: PathBuf,
    sessions_roots: Vec<PathBuf>,
    clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl SharedBlobLedger {
    /// The [`BlobLedger`] reconstruction the forwarding methods share.
    fn as_ledger(&self) -> BlobLedger {
        BlobLedger {
            inner: Arc::clone(&self.inner),
            blobs_dir: self.blobs_dir.clone(),
            sessions_roots: self.sessions_roots.clone(),
            clock: Arc::clone(&self.clock),
        }
    }

    /// See [`BlobLedger::decrement`].
    pub fn decrement(&self, session_dir: &Path) -> std::io::Result<HashSet<String>> {
        self.as_ledger().decrement(session_dir)
    }

    /// See [`BlobLedger::note_reference`].
    pub fn note_reference(&self, hash: &str) {
        self.as_ledger().note_reference(hash);
    }

    /// See [`BlobLedger::reclaim_aged`].
    pub fn reclaim_aged(&self, older_than: Duration) -> Vec<String> {
        self.as_ledger().reclaim_aged(older_than)
    }
}

/// Count one referencing session: the build-pass increment (no anchor —
/// a positive count is never zeroed).
fn increment(counts: &mut HashMap<String, RefState>, hash: String) {
    let state = counts.entry(hash).or_insert(RefState {
        refcount: 0,
        zeroed_since: None,
    });
    state.refcount += 1;
    state.zeroed_since = None;
}

/// Lock a ledger map, healing a poisoned lock (the ledger is a
/// rebuildable cache — a panicked critical section must not take the
/// whole recheck cycle down with it).
fn lock(
    inner: &Mutex<HashMap<String, RefState>>,
) -> std::sync::MutexGuard<'_, HashMap<String, RefState>> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Delete a hash's blob file under `blobs_dir` — best-effort, probing
/// every extension spelling the blob store may have written (the
/// [`crate::blob`] naming parity: `{hash}`, `{hash}.png`, …). A
/// missing file is the goal already met; a failed deletion leaves the
/// file to the next pass (idempotent convergence — the row is already
/// gone, so the hash never re-enters the recheck set).
fn delete_blob_file(blobs_dir: &Path, hash: &str) {
    let mut candidates: Vec<PathBuf> = vec![blobs_dir.join(hash)];
    for ext in ["png", "jpg", "gif", "webp", "bmp", "tif"] {
        candidates.push(blobs_dir.join(format!("{hash}.{ext}")));
    }
    for candidate in candidates {
        let _ = remove_file_if_exists(&candidate);
    }
}

/// Convert an arbitrary error into the `std::io::Error` this module's
/// signatures surface.
fn io_other(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

// ---------------------------------------------------------------------------
// The process-wide new-reference hook (B2: the note_reference wiring)
// ---------------------------------------------------------------------------

/// The process-wide "a blob was just written" hook the blob write path
/// calls on every successful [`crate::blob::AgentBlobService::write_blob`].
///
/// The retention domain's assembly parks the shared ledger's
/// [`BlobLedger::note_reference`] here (see [`install_blob_reference_hook`]),
/// so a new blob reference resets the shared ledger's zero anchor without
/// either side holding a dependency on the other — the blob write path
/// stays ledger-free (and unit-testable without one), and the ledger's
/// reuse anchor rule (ADR-003: reuse always writes a new reference, which
/// drives the count off zero and clears the anchor) is honored in-process.
///
/// Unset (the default) the hook is a no-op: a process without the
/// retention domain assembled never pays the indirection beyond one
/// `RwLock` read.
/// The hook closure's shape (the type-alias form of the complex type the
/// static below holds).
type BlobReferenceHook = Arc<dyn Fn(&str) + Send + Sync>;

static BLOB_REFERENCE_HOOK: std::sync::RwLock<Option<BlobReferenceHook>> =
    std::sync::RwLock::new(None);

/// Install (or replace) the process-wide blob-reference hook.
///
/// The embedded assembly calls this once with a closure over the shared
/// ledger slot — resolving the ledger lazily so an engine that never
/// deletes and never runs the recheck never builds it.
pub fn install_blob_reference_hook(hook: BlobReferenceHook) {
    *BLOB_REFERENCE_HOOK
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(hook);
}

/// Report a just-written blob reference to the installed hook (the blob
/// write path's single call site — see [`install_blob_reference_hook`]).
///
/// Best-effort by design: a poisoned lock or an unset hook degrades to a
/// missed anchor reset, which the recheck pass's disk-truth re-scan
/// (B2) catches anyway before any file is deleted.
pub fn report_blob_reference(hash: &str) {
    let hook = match BLOB_REFERENCE_HOOK.read() {
        Ok(hook) => hook,
        Err(_) => return,
    };
    if let Some(hook) = hook.as_ref() {
        hook(hash);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mirri_persistence::{WIRE_LOG_FILE, WIRE_LOG_GZ_FILE};
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A test clock the window-elapsed tests advance by hand: starts at a
    /// fixed instant, steps forward in whole seconds.
    struct StepClock {
        elapsed_secs: AtomicU64,
    }

    impl StepClock {
        fn new() -> StdArc<Self> {
            StdArc::new(Self {
                elapsed_secs: AtomicU64::new(0),
            })
        }

        fn advance(&self, secs: u64) {
            self.elapsed_secs.fetch_add(secs, Ordering::SeqCst);
        }
    }

    /// Materialize the clock as the `Fn` closure the ledger takes.
    fn clock_of(clock: &StdArc<StepClock>) -> StdArc<dyn Fn() -> DateTime<Utc> + Send + Sync> {
        let handle = StdArc::clone(clock);
        StdArc::new(move || {
            Utc.timestamp_opt(1_700_000_000, 0).unwrap()
                + chrono::Duration::seconds(
                    i64::try_from(handle.elapsed_secs.load(Ordering::SeqCst)).unwrap(),
                )
        })
    }

    /// A record line carrying a blobref URI (any nesting the JSON allows
    /// — here an assistant message with a text content part).
    fn record_with_ref(hash: &str) -> String {
        format!(
            r#"{{"type":"context.append_message","message":{{"role":"user","content":"see blobref:image/png;{hash} inline"}}}}"#
        )
    }

    /// Write a plain wire log with the given lines into `dir`.
    fn write_wire_log(dir: &Path, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        let content = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        std::fs::write(dir.join(WIRE_LOG_FILE), content).unwrap();
    }

    /// Write a snapshot.json whose JSON contains the given blobref URI
    /// (as a folded-state string value would).
    fn write_snapshot_with_ref(dir: &Path, hash: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(SNAPSHOT_FILE),
            format!(r#"{{"state":{{"folded":true,"last_image":"blobref:image/png;{hash}"}}}}"#),
        )
        .unwrap();
    }

    /// Write a state.json (the session-presence marker of the flat layout).
    fn write_state_json(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("state.json"), r#"{"title":"T"}"#).unwrap();
    }

    // -- extract_blob_refs ----------------------------------------------------

    #[test]
    fn should_extract_refs_from_every_stream_of_a_session_directory() {
        // Given a session directory whose main log, one agents sub-stream,
        // and snapshot each reference a DIFFERENT hash
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(&dir, &[record_with_ref("mainhash")]);
        let agent_dir = dir.join(AGENTS_DIR).join("agent-1");
        write_wire_log(&agent_dir, &[record_with_ref("agenthash")]);
        write_snapshot_with_ref(&dir, "snaphash");

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then all three streams' references are present
        assert_eq!(
            refs,
            HashSet::from([
                "mainhash".to_string(),
                "agenthash".to_string(),
                "snaphash".to_string()
            ])
        );
    }

    #[test]
    fn should_extract_refs_from_multiple_agent_sub_streams() {
        // Given a session directory with two agents sub-streams, each with
        // its own reference
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(&dir, &[record_with_ref("mainhash")]);
        for (agent, hash) in [("agent-1", "first"), ("agent-2", "second")] {
            write_wire_log(&dir.join(AGENTS_DIR).join(agent), &[record_with_ref(hash)]);
        }

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then both sub-streams' references are present
        assert_eq!(
            refs,
            HashSet::from([
                "mainhash".to_string(),
                "first".to_string(),
                "second".to_string()
            ])
        );
    }

    #[test]
    fn should_extract_agent_stream_refs_even_when_main_log_is_empty() {
        // Given a session directory with NO main log and NO snapshot, but an
        // agents sub-stream carrying a reference (the regression shape: a
        // main-log-only extraction would miss it and under-count a shared
        // attachment)
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(
            &dir.join(AGENTS_DIR).join("agent-1"),
            &[record_with_ref("onlyagents")],
        );

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then the agents-stream-only reference IS extracted
        assert_eq!(refs, HashSet::from(["onlyagents".to_string()]));
    }

    #[test]
    fn should_return_empty_set_when_directory_has_no_streams() {
        // Given a session directory with no log, no agents, no snapshot
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        std::fs::create_dir_all(&dir).unwrap();

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then the set is empty (absent files are not errors)
        assert!(refs.is_empty());
    }

    #[test]
    fn should_deduplicate_repeated_references_to_the_same_hash() {
        // Given a session directory whose main log references one hash
        // three times across lines
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(
            &dir,
            &[
                record_with_ref("shared"),
                record_with_ref("shared"),
                record_with_ref("shared"),
            ],
        );

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then the hash appears once (a set, not a bag)
        assert_eq!(refs, HashSet::from(["shared".to_string()]));
    }

    #[test]
    fn should_accept_both_blobref_separator_spellings() {
        // Given a session log using both the TS semicolon spelling and the
        // legacy slash spelling
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(
            &dir,
            &[
                record_with_ref("semicolonhash"),
                r#"{"type":"context.append_message","message":{"role":"user","content":"legacy blobref:image/png/slashhash inline"}}"#.to_string(),
            ],
        );

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then both spellings' hashes are extracted
        assert_eq!(
            refs,
            HashSet::from(["semicolonhash".to_string(), "slashhash".to_string()])
        );
    }

    #[test]
    fn should_extract_refs_when_main_log_is_compressed_on_disk() {
        // Given a session directory whose main log was compressed (the .gz
        // replaced the .jsonl) and an agents sub-stream also compressed
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(&dir, &[record_with_ref("compressedmain")]);
        let agent_dir = dir.join(AGENTS_DIR).join("agent-1");
        write_wire_log(&agent_dir, &[record_with_ref("compressedagent")]);
        write_snapshot_with_ref(&dir, "snaphash");
        mirri_persistence::compress::compress_wire_log(&dir).unwrap();
        mirri_persistence::compress::compress_wire_log(&agent_dir).unwrap();
        // Compressed shape on disk for both.
        assert!(dir.join(WIRE_LOG_GZ_FILE).is_file());
        assert!(!dir.join(WIRE_LOG_FILE).exists());
        assert!(agent_dir.join(WIRE_LOG_GZ_FILE).is_file());

        // When the references are extracted (the dual-shape read path)
        let refs = extract_blob_refs(&dir).unwrap();

        // Then every stream's references are extracted through the .gz
        assert_eq!(
            refs,
            HashSet::from([
                "compressedmain".to_string(),
                "compressedagent".to_string(),
                "snaphash".to_string()
            ])
        );
    }

    #[test]
    fn should_not_extract_other_uri_schemes_or_empty_hashes() {
        // Given a session log with data: URIs, https URLs, and a trailing
        // separator blobref (empty hash — not a reference)
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("session-1");
        write_wire_log(
            &dir,
            &[r#"{"type":"context.append_message","message":{"role":"user","content":"data:image/png;base64,AAA https://example.com/a.png and blobref:image/png; and blobref:noseparator"}}"#.to_string()],
        );

        // When the references are extracted
        let refs = extract_blob_refs(&dir).unwrap();

        // Then none of the malformed candidates yields a hash
        assert!(refs.is_empty());
    }

    // -- BlobLedger::build ----------------------------------------------------

    #[test]
    fn should_accumulate_refcount_across_sessions_sharing_one_hash() {
        // Given two session directories (one flat, one bucketed) that all
        // reference the SAME hash, plus a third referencing another hash
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let flat = root.join("session-flat");
        write_state_json(&flat);
        write_wire_log(&flat, &[record_with_ref("shared")]);
        let bucketed = root.join("wd_abc").join("session-in-bucket");
        write_wire_log(&bucketed, &[record_with_ref("shared")]);
        let other = root.join("session-other");
        write_state_json(&other);
        write_wire_log(&other, &[record_with_ref("solo")]);
        let blobs_dir = tmp.path().join("blobs");

        // When the ledger is built over the session roots
        let ledger = BlobLedger::build(&blobs_dir, std::slice::from_ref(&root)).unwrap();

        // Then the shared hash counts two referencing sessions and the
        // solo hash one (observed through decrements)
        let newly_zeroed = ledger.decrement(&flat).unwrap();
        assert!(
            newly_zeroed.is_empty(),
            "the shared hash is still referenced by the bucketed session"
        );
        let newly_zeroed = ledger.decrement(&bucketed).unwrap();
        assert_eq!(
            newly_zeroed,
            HashSet::from(["shared".to_string()]),
            "the shared hash zeroes only after BOTH sessions decrement"
        );
        let newly_zeroed = ledger.decrement(&other).unwrap();
        assert_eq!(newly_zeroed, HashSet::from(["solo".to_string()]));
    }

    #[test]
    fn should_treat_bucket_layout_without_state_json_as_session_directory() {
        // Given a bucketed session directory MISSING its state.json (a torn
        // delete) that still references a hash
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let bucketed = root.join("wd_x").join("torn-session");
        write_wire_log(&bucketed, &[record_with_ref("tornhash")]);
        let blobs_dir = tmp.path().join("blobs");

        // When the ledger is built
        let ledger = BlobLedger::build(&blobs_dir, &[root]).unwrap();

        // Then the torn session's references are still counted (skipping
        // them could delete a shared attachment)
        let newly_zeroed = ledger.decrement(&bucketed).unwrap();
        assert_eq!(newly_zeroed, HashSet::from(["tornhash".to_string()]));
    }

    // -- BlobLedger::decrement ------------------------------------------------

    #[test]
    fn should_return_only_hashes_that_become_zero_on_this_decrement() {
        // Given a ledger over two sessions where hash "shared" is
        // referenced by both and hash "solo" by only one
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session_a = root.join("session-a");
        let session_b = root.join("session-b");
        write_state_json(&session_a);
        write_wire_log(
            &session_a,
            &[record_with_ref("shared"), record_with_ref("solo-a")],
        );
        write_state_json(&session_b);
        write_wire_log(
            &session_b,
            &[record_with_ref("shared"), record_with_ref("solo-b")],
        );
        let blobs_dir = tmp.path().join("blobs");
        let ledger = BlobLedger::build(&blobs_dir, &[root]).unwrap();

        // When the first session is decremented
        let newly_zeroed = ledger.decrement(&session_a).unwrap();

        // Then only ITS exclusive hash zeroed — the shared hash did not
        assert_eq!(newly_zeroed, HashSet::from(["solo-a".to_string()]));

        // And when the second session is decremented
        let newly_zeroed = ledger.decrement(&session_b).unwrap();

        // Then the shared hash AND its exclusive hash zeroed together
        assert_eq!(
            newly_zeroed,
            HashSet::from(["shared".to_string(), "solo-b".to_string()])
        );
    }

    #[test]
    fn should_not_return_already_zeroed_hash_on_second_decrement() {
        // Given a ledger over one session referencing one hash, already
        // decremented once (a double-decrement stand-in)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("once")]);
        let blobs_dir = tmp.path().join("blobs");
        let ledger = BlobLedger::build(&blobs_dir, &[root]).unwrap();
        let first = ledger.decrement(&session).unwrap();
        assert_eq!(first, HashSet::from(["once".to_string()]));

        // When the same session is decremented again
        let second = ledger.decrement(&session).unwrap();

        // Then the hash is NOT reported again (the zero-crossing is a
        // one-time event)
        assert!(second.is_empty());
    }

    #[test]
    fn should_stamp_anchor_on_zero_crossing_for_uncounted_hash() {
        // Given a ledger built over EMPTY roots and a session directory
        // it has never counted (the foreign-process shape)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let foreign = root.join("foreign-session");
        write_wire_log(&foreign, &[record_with_ref("foreignhash")]);
        let blobs_dir = tmp.path().join("blobs");
        let clock = StepClock::new();
        let ledger = BlobLedger::build_with_clock(&blobs_dir, &[], clock_of(&clock)).unwrap();
        // When the uncounted session is decremented
        let newly_zeroed = ledger.decrement(&foreign).unwrap();

        // Then the hash is reported zero (the ledger errs toward
        // "already unclaimed" rather than inventing a reference)
        assert_eq!(newly_zeroed, HashSet::from(["foreignhash".to_string()]));

        // And the anchor exists, so the aged recheck can reclaim it
        clock.advance(7 * 3600);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);
        assert_eq!(reclaimed, vec!["foreignhash".to_string()]);
    }

    // -- BlobLedger::reclaim_aged ---------------------------------------------

    #[test]
    fn should_not_reclaim_within_the_aging_window() {
        // Given a ledger whose sole hash just zeroed (anchor = now)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("young")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("young");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger = BlobLedger::build_with_clock(&blobs_dir, &[root], clock_of(&clock)).unwrap();
        let newly_zeroed = ledger.decrement(&session).unwrap();
        assert_eq!(newly_zeroed, HashSet::from(["young".to_string()]));

        // When the recheck runs with the window not yet elapsed
        clock.advance(BLOB_AGING_WINDOW.as_secs() - 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then nothing is reclaimed and the blob file survives
        assert!(reclaimed.is_empty());
        assert!(blob_file.is_file());
    }

    #[test]
    fn should_reclaim_after_the_window_deletes_the_file_and_drops_the_row() {
        // Given a zeroed hash whose window has elapsed, with its blob file
        // on disk
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("aged")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("aged.png");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger = BlobLedger::build_with_clock(&blobs_dir, &[root], clock_of(&clock)).unwrap();
        ledger.decrement(&session).unwrap();
        // The executor's storage order: the directory is gone before the
        // recheck runs (the disk-truth scan, B2, would otherwise find the
        // session's references still live and protect the hash).
        std::fs::remove_dir_all(&session).unwrap();

        // When the recheck runs past the window
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the hash is reclaimed, the file (extension spelling) is
        // deleted, and a second recheck has nothing left to do
        assert_eq!(reclaimed, vec!["aged".to_string()]);
        assert!(!blob_file.exists());
        assert!(ledger.reclaim_aged(BLOB_AGING_WINDOW).is_empty());
    }

    #[test]
    fn should_not_reclaim_a_hash_re_referenced_after_zeroing() {
        // Given a zeroed hash that was then re-referenced in-process
        // (note_reference — the reuse anchor reset)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("reused")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("reused");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger = BlobLedger::build_with_clock(&blobs_dir, &[root], clock_of(&clock)).unwrap();
        let newly_zeroed = ledger.decrement(&session).unwrap();
        assert_eq!(newly_zeroed, HashSet::from(["reused".to_string()]));

        // When a new reference notes the hash (a new session offloaded
        // the same content) and the window then elapses
        ledger.note_reference("reused");
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the hash is NOT reclaimed (the anchor was reset) and the
        // file survives
        assert!(reclaimed.is_empty());
        assert!(blob_file.is_file());
    }

    #[test]
    fn should_keep_referenced_hashes_out_of_the_recheck_entirely() {
        // Given a ledger where a hash is still referenced (never zeroed)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session_a = root.join("session-a");
        let session_b = root.join("session-b");
        for dir in [&session_a, &session_b] {
            write_state_json(dir);
            write_wire_log(dir, &[record_with_ref("live")]);
        }
        let blobs_dir = tmp.path().join("blobs");
        let clock = StepClock::new();
        let ledger = BlobLedger::build_with_clock(&blobs_dir, &[root], clock_of(&clock)).unwrap();

        // When one session decrements and the window elapses
        ledger.decrement(&session_a).unwrap();
        clock.advance(BLOB_AGING_WINDOW.as_secs() * 10);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the still-referenced hash is not reclaimed
        assert!(reclaimed.is_empty());
    }

    // -- BLOB_AGING_WINDOW ----------------------------------------------------

    #[test]
    fn should_default_the_aging_window_to_six_hours() {
        // Given the production constant
        // Then it is the §3.4 hour-scale window: 6 hours
        assert_eq!(BLOB_AGING_WINDOW, Duration::from_secs(6 * 3600));
    }

    // -- B2: the disk-truth recheck -------------------------------------------

    #[test]
    fn should_not_reclaim_a_due_hash_when_a_session_created_after_the_ledger_build_references_it() {
        // Given a ledger built over ONE session, a SECOND session created
        // AFTER the build referencing the SAME hash (the in-memory count
        // went stale — the exact TOCTOU the B2 disk recheck closes), the
        // old session then deleted (its decrement zeroes the hash), and
        // the aging window elapsed
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let old_session = root.join("session-old");
        write_state_json(&old_session);
        write_wire_log(&old_session, &[record_with_ref("shared")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("shared");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger =
            BlobLedger::build_with_clock(&blobs_dir, std::slice::from_ref(&root), clock_of(&clock))
                .unwrap();

        // The new session lands AFTER the build — the ledger never
        // counted it (a foreign process's session, or this process's
        // post-build reference the note hook missed).
        let new_session = root.join("session-new");
        write_state_json(&new_session);
        write_wire_log(&new_session, &[record_with_ref("shared")]);

        // The old session's delete: the decrement zeroes the hash (the
        // ledger, stale, does not know the new session holds it too).
        ledger.decrement(&old_session).unwrap();

        // When the window elapses and the recheck runs
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the hash is NOT reclaimed — the disk recheck found the new
        // session's live reference — and the blob file survives
        assert!(
            reclaimed.is_empty(),
            "the disk recheck must protect the hash"
        );
        assert!(blob_file.is_file());
    }

    #[test]
    fn should_reclaim_a_due_hash_with_no_live_reference_after_the_disk_recheck() {
        // Given a zeroed, aged hash whose session (and every other) is
        // gone from disk — the recheck's confirm-delete side
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("orphan")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("orphan");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger =
            BlobLedger::build_with_clock(&blobs_dir, std::slice::from_ref(&root), clock_of(&clock))
                .unwrap();
        ledger.decrement(&session).unwrap();
        // Every reference is gone from disk (the session directory
        // removed wholesale).
        std::fs::remove_dir_all(&session).unwrap();

        // When the window elapses and the recheck runs
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the hash IS reclaimed and the file is gone
        assert_eq!(reclaimed, vec!["orphan".to_string()]);
        assert!(!blob_file.exists());
    }

    #[test]
    fn should_not_reclaim_anything_when_a_foreign_session_directory_is_unreadable() {
        // Given a zeroed, aged hash and a session directory whose stream
        // CANNOT be read (an unreadable wire.jsonl — the loud-failure
        // stance): the recheck's scan must fail and reclaim nothing
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("protected")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("protected");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger =
            BlobLedger::build_with_clock(&blobs_dir, std::slice::from_ref(&root), clock_of(&clock))
                .unwrap();
        ledger.decrement(&session).unwrap();
        // The executor's storage order: the decrementing session's
        // directory is gone before the recheck (its references are dead).
        std::fs::remove_dir_all(&session).unwrap();
        // The other session's log becomes unreadable (invalid UTF-8
        // bytes in the wire log — the line reader fails the stream, the
        // loud-failure stance).
        let foreign = root.join("session-foreign");
        write_state_json(&foreign);
        std::fs::write(foreign.join(WIRE_LOG_FILE), [0xFF, 0xFE, 0x00]).unwrap();

        // When the window elapses and the recheck runs
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then nothing is reclaimed (an unknown reference state never
        // deletes) and a LATER pass — the unreadable session gone —
        // reclaims it
        assert!(reclaimed.is_empty(), "a failed scan must reclaim nothing");
        assert!(blob_file.is_file());
        std::fs::remove_dir_all(&foreign).unwrap();
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);
        assert_eq!(reclaimed, vec!["protected".to_string()]);
        assert!(!blob_file.exists());
    }

    #[test]
    fn should_note_a_reference_through_the_installed_hook_and_reset_the_anchor() {
        // Given a zeroed hash and the process-wide hook wired to the
        // ledger (the B2 blob-write-path integration's unit form)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("hooked")]);
        let blobs_dir = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let blob_file = blobs_dir.join("hooked");
        std::fs::write(&blob_file, b"payload").unwrap();
        let clock = StepClock::new();
        let ledger = StdArc::new(
            BlobLedger::build_with_clock(&blobs_dir, &[root], clock_of(&clock)).unwrap(),
        );
        ledger.decrement(&session).unwrap();

        // When a new blob write reports through the hook (the exact call
        // `AgentBlobService::write_blob` makes) and the window elapses
        install_blob_reference_hook({
            let ledger = StdArc::clone(&ledger);
            StdArc::new(move |hash: &str| ledger.note_reference(hash))
        });
        report_blob_reference("hooked");
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);

        // Then the hash is not reclaimed... UNLESS the disk holds no
        // reference: this ledger's roots still hold the session's wire
        // log, so even without the hook the disk recheck would protect
        // it. The hook's own effect is observable when the wire log is
        // gone: remove the session's records and re-run — the anchor
        // reset means no row is even due.
        assert!(reclaimed.is_empty());
        // The hook's direct effect: the row carries no anchor anymore
        // (note_reference cleared it), so even an empty-disk recheck has
        // nothing due for this hash.
        std::fs::remove_dir_all(&session).unwrap();
        clock.advance(BLOB_AGING_WINDOW.as_secs() + 1);
        assert!(ledger.reclaim_aged(BLOB_AGING_WINDOW).is_empty());
        assert!(blob_file.is_file());
    }

    // -- SharedBlobLedger ----------------------------------------------------

    #[test]
    fn should_share_state_between_a_ledger_and_its_shared_handle() {
        // Given a ledger and its shared handle (the embedded wiring's
        // multi-site form)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = root.join("session-a");
        write_state_json(&session);
        write_wire_log(&session, &[record_with_ref("sharedhandle")]);
        let blobs_dir = tmp.path().join("blobs");
        let ledger = BlobLedger::build(&blobs_dir, std::slice::from_ref(&root)).unwrap();
        let shared = ledger.shared();

        // When the shared handle decrements and the original rechecks
        let newly_zeroed = shared.decrement(&session).unwrap();
        assert_eq!(newly_zeroed, HashSet::from(["sharedhandle".to_string()]));
        // The executor's storage order: the decrement precedes the
        // DIRECTORY removal — the recheck (B2's disk-truth scan) only
        // reclaims hashes no live session on disk references, so the
        // directory must be gone before the recheck (the executor's
        // step-4→step-5 order).
        std::fs::remove_dir_all(&session).unwrap();
        let reclaimed = ledger.reclaim_aged(Duration::from_secs(0));

        // Then the original sees the shared handle's mutation
        assert_eq!(reclaimed, vec!["sharedhandle".to_string()]);
    }
}
