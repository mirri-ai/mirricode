//! Append-only log store backed by a `wire.jsonl` file.
//!
//! Each line of the file is a single JSON object tagged with a `type`
//! discriminator (see [`mirri_proto::WireOp`]). The store supports two
//! modes:
//!
//! - **Write mode** ([`JsonlAppendLog::create`]): opens (or truncates) the
//!   file for sequential appends. A session writes its ops here as they
//!   occur.
//! - **Append mode** ([`JsonlAppendLog::open_for_append`]): opens an existing
//!   file for sequential appends **without truncating** it. A resumed session
//!   re-opens its wire log this way after replay so later ops continue the
//!   same file.
//! - **Read mode** ([`JsonlAppendLog::open`]): opens the file read-only for
//!   replaying the recorded ops. This is how a session reconstructs state
//!   on startup or how an external tool inspects a session's history.
//!
//! ## Durability
//!
//! Every append is `sync_all`-ed before returning, and the parent directory
//! is fsynced once after the first append to a new file, so an acknowledged
//! append survives a crash.
//!
//! ## Resilience
//!
//! When reading, a **final line without a trailing newline** that fails to
//! parse is treated as a torn write (the process crashed mid-append) and is
//! silently dropped. Any other unparseable line is mid-file corruption and
//! fails the read with [`AppendLogError::Corrupted`] — the log is no longer
//! considered trustworthy. Unknown op `type` values fall through to
//! [`mirri_proto::WireOp::Unknown`], preserving forward compatibility with
//! future op types.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mirri_proto::WireOp;

use crate::compress::probe_compressed;
use crate::fs_utils::sync_directory;

/// Errors specific to append-log operations.
#[derive(Debug, thiserror::Error)]
pub enum AppendLogError {
    /// A line that is not the torn final write could not be parsed — the
    /// log is corrupted, not merely truncated.
    #[error("wire log corrupted at line {line_number}: {message}")]
    Corrupted {
        /// 1-based line number of the offending line.
        line_number: usize,
        /// The underlying parse error.
        message: String,
    },
}

/// The contract for an append-only op log.
///
/// Implementations must be safe to share across async tasks (`Send +
/// Sync`). The write handle (`&mut self`) is exclusive; reads take a
/// shared reference and re-open the underlying file so they never
/// interfere with an in-flight writer.
#[allow(async_fn_in_trait)]
pub trait AppendLogStore: Send + Sync {
    /// Append a single wire op as a new JSON line.
    ///
    /// Returns the number of bytes written to the file for this line,
    /// including the trailing newline — a per-append byte anchor that
    /// consumers use to track where the log ends.
    ///
    /// Fails if the log was opened in read-only mode (via `open`) rather
    /// than write mode (via `create`).
    async fn append(&mut self, op: &WireOp) -> Result<u64>;

    /// Append a batch of wire ops with ONE durability barrier.
    ///
    /// Returns the total number of bytes written to the file for the whole
    /// batch, including each line's trailing newline — the batch analog of
    /// [`append`](Self::append)'s per-op byte anchor, so callers can advance
    /// a version anchor by exactly their own bytes even when other writers
    /// share the file.
    ///
    /// The default impl delegates to [`append`](Self::append) per op (each
    /// with its own fsync) — semantically identical, just slower.
    /// `JsonlAppendLog` overrides this with a single buffered write + single
    /// fsync: bulk-history writers (the session fork pipeline) replay
    /// thousands of ops whose per-op fsync dominates the runtime, while the
    /// batch's atomicity contract is the same as the single append's — the
    /// whole batch either lands or a crash tears the final line (which the
    /// reader's torn-write policy drops).
    async fn append_batch(&mut self, ops: &[WireOp]) -> Result<u64> {
        let mut total = 0u64;
        for op in ops {
            total += self.append(op).await?;
        }
        Ok(total)
    }

    /// Read every op in the log, in file order.
    async fn read_all(&self) -> Result<Vec<WireOp>>;

    /// Read ops starting at the given line `offset` (0-based).
    ///
    /// Lines before `offset` are scanned but not returned. Malformed lines
    /// are skipped and do not count towards the returned slice, but they
    /// *do* advance the offset counter (i.e. offsets are line-based, not
    /// op-based).
    async fn read_from(&self, offset: usize) -> Result<Vec<WireOp>>;

    /// Count the number of non-empty lines in the log.
    async fn count(&self) -> Result<usize>;
}

/// A `wire.jsonl` append log backed by a single file on disk.
///
/// Construct with [`JsonlAppendLog::create`] for write mode or
/// [`JsonlAppendLog::open`] for read mode. The same struct type serves
/// both roles: in write mode it holds an open append-mode handle; in read
/// mode the handle is `None` and each read call re-opens the file so that
/// reads never block an active writer.
///
/// ## Write-path design (shutdown safety)
///
/// Appends go through a **std** `File` on the blocking pool
/// ([`tokio::task::spawn_blocking`]) rather than a `tokio::fs::File`.
/// tokio's async `File` is a single-owner `Idle`/`Busy` state machine
/// whose error path can strand the handle in `Idle(None)` — a second
/// append then panics inside tokio (`buf_cell.take().unwrap()`) instead
/// of returning an error. With a std handle, every append is an
/// independent blocking submission that runs to completion even when the
/// calling task is aborted at its await point (a graceful-shutdown
/// cancellation cannot interrupt an in-flight fsync), and a write failure
/// is reported as `Err` — never a panic. Once a write fails, the handle is
/// parked (a failed fd must not be retried) and later appends fail fast.
pub struct JsonlAppendLog {
    path: PathBuf,
    /// The shared append-mode std handle. `None` when closed, detached, or
    /// opened read-only — a closed/parked handle never accepts appends.
    file: Option<Arc<std::sync::Mutex<std::fs::File>>>,
    /// Whether the parent directory has been fsynced after the first append
    /// to this file. A one-time directory fsync makes the file's directory
    /// entry durable; appends after that only need the file fsync.
    dir_synced: bool,
}

impl JsonlAppendLog {
    /// Create a new `wire.jsonl` file for writing.
    ///
    /// Truncates the file if it already exists. Parent directories are
    /// created as needed. The returned handle is the only way to call
    /// [`append`](AppendLogStore::append); read methods also work on a
    /// write-mode handle.
    ///
    /// The handle opens in append mode even though it truncates on create:
    /// a plain write-mode fd records its own offset, and the moment any
    /// other handle appends to the same file (the fork pipeline's history
    /// batch), that stale offset turns the next write into an overwrite of
    /// the file's middle — mid-file corruption. `O_APPEND` keeps every
    /// write at the end of the file regardless of who grew it. (std forbids
    /// combining `truncate` with `append` in one open, so the create-time
    /// truncation is a `set_len(0)` after the open.)
    pub async fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path_owned = path.to_path_buf();
        // Reproduce `truncate(true)` semantics on the blocking pool (see the
        // doc comment above): a no-op on a fresh file, a reset for a path
        // that predates this handle. std forbids combining `truncate` with
        // `append` in one open, so the create-time truncation is a
        // `set_len(0)` after the open.
        let file = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_owned)?;
            file.set_len(0)?;
            Ok::<_, std::io::Error>(file)
        })
        .await??;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(Arc::new(Mutex::new(file))),
            dir_synced: false,
        })
    }

    /// Open an existing `wire.jsonl` for reading only.
    ///
    /// The file handle is not held open; each read call re-opens the path.
    /// Calling [`append`](AppendLogStore::append) on a read-mode handle
    /// returns an error.
    pub async fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            file: None,
            dir_synced: false,
        })
    }

    /// Open a `wire.jsonl` for appending, creating it if missing.
    ///
    /// Unlike [`create`](Self::create), an existing file is **not**
    /// truncated: every append lands after the records already on disk.
    /// This is the mode a resumed session uses to continue writing its wire
    /// log after replay — truncating on resume would destroy the history
    /// the resume was built from.
    pub async fn open_for_append(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path_owned = path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path_owned)
        })
        .await??;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(Arc::new(Mutex::new(file))),
            // Like `create`, the directory entry is fsynced once on the
            // first append — harmless (and necessary when the file was just
            // created) for an already-existing file.
            dir_synced: false,
        })
    }

    /// The filesystem path of the backing `wire.jsonl` file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Close the write handle after flushing and fsyncing it — the
    /// graceful-shutdown drain point.
    ///
    /// The handle is taken (a closed log never accepts appends) and the
    /// final `sync_all` runs on the blocking pool, so an in-flight append
    /// serialized on the same handle mutex has already landed by the time
    /// this returns. Idempotent: closing a read-only or already-closed log
    /// is a no-op.
    pub async fn close(&mut self) {
        if let Some(handle) = self.file.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let guard = handle.lock().unwrap_or_else(|e| e.into_inner());
                let _ = guard.sync_all();
            })
            .await;
        }
    }

    /// The CURRENT on-disk length of the backing file, in bytes.
    ///
    /// The install/materialize-time anchor base of a session's wire log: the
    /// byte count the version anchor starts from when a writer takes over a
    /// file it did not (fully) write. Statting the file — not summing this
    /// handle's own writes — means a foreign handle's appends (the fork
    /// pipeline's history batch) are counted too. Best-effort on a missing
    /// file: `0` (the first successful append re-calibrates from the real
    /// end; see the anchor semantics in `SessionWireLog::install`).
    pub async fn current_len(&self) -> Result<u64> {
        let path = self.path.clone();
        let len = tokio::task::spawn_blocking(move || {
            std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
        })
        .await?;
        Ok(len)
    }

    /// Read the raw bytes at and after `offset`, up to the file's CURRENT
    /// end — the incremental absorber's read window (see
    /// `SessionWireLog::drain_foreign_ops`).
    ///
    /// The offset is a floor, not an upper bound: foreign writers may have
    /// appended past it since. A missing file yields empty (a vanished
    /// journal has nothing to absorb); any other IO error propagates — the
    /// caller must not silently fold a broken journal. The read is BYTES, not
    /// a string: a foreign writer caught mid-encoding leaves an incomplete
    /// UTF-8 character at the window's end, which the caller's torn-tail
    /// policy handles (read_to_string would reject the whole read instead).
    pub async fn read_tail(&self, offset: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            };
            // Seek past the anchor even beyond EOF: the anchor of a fresh
            // (zeroed) journal may exceed a file that was rebuilt smaller —
            // the subsequent read then simply yields empty.
            file.seek(SeekFrom::Start(offset))?;
            let mut raw = Vec::new();
            file.read_to_end(&mut raw)?;
            Ok(raw)
        })
        .await?
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
///
/// Used to stamp appended wire records with their `time` field.
///
/// Best-effort: a clock that reads before the Unix epoch (a misconfigured
/// NTP step, a unit-test clock) cannot be reported as `time: 0` — that
/// would corrupt ordering against every real record — so it clamps to
/// 2001-09-09 (~1e12 ms): a timestamp any real clock is later than, kept
/// far below the present so the record still sorts oldest-first.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1_000_000_000_000) // 2001-09-09, any real clock is later
}

/// Read every line of a wire log in either on-disk shape (plain
/// `wire.jsonl` or compressed `wire.jsonl.gz`) — the unified dual-shape
/// read path (ADR-001) all read consumers go through.
///
/// Line semantics mirror the old whole-file read: lines come back without
/// their terminators, the trailing `ends_with_newline` flag drives the
/// torn-write policy, and an absent log yields no lines.
pub(crate) async fn read_raw_lines(path: &Path) -> Result<(Vec<String>, bool)> {
    let Some(dir) = path.parent() else {
        // A bare file name with no directory cannot be shape-probed; treat
        // it as a plain read (the old behavior).
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let ends_with_newline = content.ends_with('\n');
        let lines = content.lines().map(str::to_string).collect();
        return Ok((lines, ends_with_newline));
    };

    // Match the compressed shape only when the probed directory actually
    // refers to THIS path (e.g. a caller reading `agents/x/wire.jsonl`
    // whose sibling `.gz` is probed via the same dir → same name, fine; but
    // a caller reading an unrelated file inside a compressed session dir
    // must not be hijacked to the gz of a different name).
    let shape = probe_compressed(dir);
    // Owned: the closure below must be 'static.
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());

    let dir_owned = dir.to_path_buf();
    let path_owned = path.to_path_buf();
    let (lines, ends_with_newline) = tokio::task::spawn_blocking(move || {
        if shape == crate::compress::CompressedShape::Compressed
            && name.as_deref() == Some(crate::compress::WIRE_LOG_FILE)
        {
            let mut reader = crate::compress::open_wire_log_lines(&dir_owned)?;
            let mut lines = Vec::new();
            let mut buffer = String::new();
            while reader.read_line(&mut buffer)? {
                lines.push(std::mem::take(&mut buffer));
            }
            // The REAL terminator probe (M7): the decompressed stream's
            // last byte decides — a torn tail that was compressed before
            // its `\n` landed stays a torn tail here (the old hardcoded
            // `true` turned that tolerated tail into a hard Corrupted
            // error).
            let ends = reader.source_ends_with_newline();
            Ok::<_, anyhow::Error>((lines, ends))
        } else {
            let content = std::fs::read_to_string(&path_owned)
                .with_context(|| format!("failed to read {}", path_owned.display()))?;
            let ends_with_newline = content.ends_with('\n');
            let lines = content.lines().map(str::to_string).collect();
            Ok((lines, ends_with_newline))
        }
    })
    .await??;
    Ok((lines, ends_with_newline))
}

/// Whether a line at `line_index` (0-based, among `line_count` lines) that
/// failed to parse is a torn final write.
///
/// A torn write is the last line of the file and the file has no trailing
/// newline — the process crashed before the append completed, so the line is
/// dropped. Any other unparseable line is genuine mid-file corruption.
pub(crate) fn is_torn_final_line(
    line_index: usize,
    line_count: usize,
    file_ends_with_newline: bool,
) -> bool {
    line_index + 1 == line_count && !file_ends_with_newline
}

/// One durable append on the blocking pool: write + fsync while holding
/// the handle's mutex, so appends serialize at the OS handle level and a
/// task aborted at this await point cannot interrupt the fsync (the
/// blocking submission runs to completion regardless of the caller).
///
/// A failed write parks the caller's copy of the handle by returning the
/// error — the struct-level `file` slot is the caller's responsibility.
async fn write_durable(handle: Arc<Mutex<std::fs::File>>, bytes: Vec<u8>) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = handle.lock().unwrap_or_else(|e| e.into_inner());
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    })
    .await?
}

impl JsonlAppendLog {
    /// The one-time parent-directory fsync after the first append to a
    /// (new) file — the file's directory entry must be durable too.
    fn sync_dir_once(&mut self) -> Result<()> {
        if !self.dir_synced {
            if let Some(parent) = self.path.parent() {
                sync_directory(parent).with_context(|| {
                    format!(
                        "failed to fsync directory {} after first append",
                        parent.display()
                    )
                })?;
            }
            self.dir_synced = true;
        }
        Ok(())
    }
}

impl AppendLogStore for JsonlAppendLog {
    async fn append(&mut self, op: &WireOp) -> Result<u64> {
        let handle = self
            .file
            .as_mut()
            .context("log not opened for writing — use create() not open()")?;
        let mut value = serde_json::to_value(op)?;
        // Wire-log timestamps (TS parity): every record except the metadata
        // envelope carries a top-level `time` epoch-ms value. The Rust
        // `WireOp` variants do not model `time` (see `mirri_proto::wire`),
        // so it is injected here, at the persistence layer. An already
        // present `time` value (e.g. an op replayed/rewritten from a TS log)
        // is preserved, and replay ignores the field either way.
        if value.get("type").and_then(serde_json::Value::as_str) != Some("metadata")
            && !value.as_object().is_some_and(|o| o.contains_key("time"))
            && let Some(object) = value.as_object_mut()
        {
            object.insert("time".to_string(), now_epoch_ms().into());
        }
        let json = serde_json::to_string(&value)?;
        let line = format!("{json}\n");
        let written = line.len() as u64;
        write_durable(Arc::clone(handle), line.into_bytes()).await?;
        // One-time directory fsync: after the first append to a (new) file,
        // the file's directory entry must be durable too.
        self.sync_dir_once()?;
        Ok(written)
    }

    async fn append_batch(&mut self, ops: &[WireOp]) -> Result<u64> {
        let handle = self
            .file
            .as_mut()
            .context("log not opened for writing — use create() not open()")?;
        if ops.is_empty() {
            return Ok(0);
        }
        // Serialize the whole batch into one buffer: one blocking write, one
        // fsync. Each record gets the same `time` injection rule the single
        // append applies (preserved timestamps stay preserved; the metadata
        // envelope stays unstamped).
        let mut buffer = String::with_capacity(ops.len() * 256);
        for op in ops {
            let mut value = serde_json::to_value(op)?;
            if value.get("type").and_then(serde_json::Value::as_str) != Some("metadata")
                && !value.as_object().is_some_and(|o| o.contains_key("time"))
                && let Some(object) = value.as_object_mut()
            {
                object.insert("time".to_string(), now_epoch_ms().into());
            }
            buffer.push_str(&serde_json::to_string(&value)?);
            buffer.push('\n');
        }
        let written = buffer.len() as u64;
        write_durable(Arc::clone(handle), buffer.into_bytes()).await?;
        self.sync_dir_once()?;
        Ok(written)
    }

    async fn read_all(&self) -> Result<Vec<WireOp>> {
        self.read_from(0).await
    }

    async fn read_from(&self, offset: usize) -> Result<Vec<WireOp>> {
        let (lines, ends_with_newline) = read_raw_lines(&self.path).await?;
        let mut ops = Vec::new();
        let mut index = 0usize;

        for (line_index, line) in lines.iter().enumerate() {
            if index < offset {
                index += 1;
                continue;
            }
            index += 1;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<WireOp>(line) {
                Ok(op) => ops.push(op),
                Err(error) if is_torn_final_line(line_index, lines.len(), ends_with_newline) => {
                    tracing::debug!(
                        line_number = line_index + 1,
                        error = %error,
                        "dropping torn final line of wire log (crash mid-append)"
                    );
                }
                Err(error) => {
                    return Err(AppendLogError::Corrupted {
                        line_number: line_index + 1,
                        message: error.to_string(),
                    }
                    .into());
                }
            }
        }

        Ok(ops)
    }

    async fn count(&self) -> Result<usize> {
        // Dual-shape aware (ADR-001): a compressed log's lines are counted
        // through the same unified read entry the other consumers use.
        let (lines, _) = read_raw_lines(&self.path).await?;
        Ok(lines.iter().filter(|line| !line.trim().is_empty()).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_return_written_byte_count_when_append_succeeds() {
        // Given a fresh log and a metadata op to record
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        let op = WireOp::Metadata {
            protocol_version: Some("1.6".to_string()),
            created_at: Some(1_728_800_000_000),
        };

        // When the op is appended
        let n = log.append(&op).await.unwrap();

        // Then the returned count matches the full line length on disk,
        // including the newline terminator
        let line = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            n as usize,
            line.len(),
            "append must report the full line length it wrote"
        );
        assert!(line.ends_with('\n'));
    }

    #[tokio::test]
    async fn should_return_total_batch_byte_count_when_batch_appended() {
        // Given a fresh log and a batch of two ops
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        let ops = [WireOp::ContextClear {}, WireOp::ContextClear {}];

        // When the batch is appended
        let n = log.append_batch(&ops).await.unwrap();

        // Then the returned count equals the file's growth (both lines,
        // each with its newline) — the batch total, not zero
        let file_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            n, file_len,
            "batch must report the full byte count it wrote"
        );
        assert!(n > 0);
    }

    #[tokio::test]
    async fn should_return_zero_when_batch_is_empty() {
        // Given a fresh log
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();

        // When an empty batch is appended
        let n = log.append_batch(&[]).await.unwrap();

        // Then no bytes were written and the count is zero
        assert_eq!(n, 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn should_fsync_parent_directory_once_when_appending_to_new_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        assert!(
            !log.dir_synced,
            "a fresh log has not synced its directory yet"
        );

        log.append(&WireOp::ContextClear {}).await.unwrap();
        assert!(
            log.dir_synced,
            "the first append must fsync the parent directory"
        );

        log.append(&WireOp::ContextClear {}).await.unwrap();
        assert!(
            log.dir_synced,
            "later appends keep the directory-synced flag set"
        );
    }

    #[tokio::test]
    async fn should_detect_torn_final_line_when_line_is_last_without_newline() {
        // 3 lines total, failing line is index 2 (the last), no trailing newline.
        assert!(is_torn_final_line(2, 3, false));
        // Not the last line → corruption, not torn.
        assert!(!is_torn_final_line(1, 3, false));
        // Last line but the file ends with a newline → the write completed.
        assert!(!is_torn_final_line(2, 3, true));
    }

    #[tokio::test]
    async fn should_stamp_every_appended_op_with_a_time_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // When a non-metadata op is appended
        log.append(&WireOp::ContextAppendMessage {
            message: serde_json::json!({"role": "user", "content": "hi"}),
        })
        .await
        .unwrap();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Then the written line parses as JSON and carries a "time" key
        // with a plausible epoch-ms value (TS parity: every wire record is
        // timestamped except the metadata envelope).
        let (lines, _) = read_raw_lines(&path).await.unwrap();
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let time = value
            .get("time")
            .and_then(serde_json::Value::as_u64)
            .expect("expected a top-level \"time\" field on the appended line");
        assert!(
            (before..=after).contains(&time),
            "expected time in [{before},{after}], got {time}"
        );
    }

    #[tokio::test]
    async fn should_not_stamp_the_metadata_line_with_time() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();

        // When the metadata envelope is appended
        log.append(&WireOp::Metadata {
            protocol_version: Some("2.0".to_string()),
            created_at: Some(1),
        })
        .await
        .unwrap();

        // Then the line has no "time" key (TS metadata carries created_at only)
        let (lines, _) = read_raw_lines(&path).await.unwrap();
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(
            value.get("time").is_none(),
            "metadata line must not carry a time field: {value}"
        );
    }

    #[tokio::test]
    async fn should_append_after_records_written_by_another_handle_when_the_log_grew_under_an_installed_writer()
     {
        // Given a log created (and header-written) by one handle — the
        // session's installed writer — then extended by a second,
        // append-mode handle (the fork pipeline's history batch)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut writer = JsonlAppendLog::create(&path).await.unwrap();
        writer
            .append(&WireOp::Metadata {
                protocol_version: Some("2.0".to_string()),
                created_at: Some(1),
            })
            .await
            .unwrap();
        let mut batch_handle = JsonlAppendLog::open_for_append(&path).await.unwrap();
        batch_handle
            .append_batch(&[WireOp::ContextClear {}, WireOp::ContextClear {}])
            .await
            .unwrap();
        drop(batch_handle);

        // When the first handle records another op — its fd offset still
        // points at the end of the header it wrote, before the batch
        writer.append(&WireOp::ContextClear {}).await.unwrap();

        // Then every line of the log parses and all four records survive:
        // the live op must land after the batch, never overwrite the
        // batch's head from the writer's stale offset.
        let ops = writer.read_all().await.unwrap_or_else(|e| {
            panic!("the log must stay parseable after a live append over a grown file: {e}")
        });
        assert_eq!(
            ops.len(),
            4,
            "header + 2 batch records + 1 live op must all survive, got {} ops",
            ops.len()
        );
    }

    #[tokio::test]
    async fn should_append_without_truncating_when_opening_existing_log_for_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");

        // Given a log with one recorded op
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(1),
        })
        .await
        .unwrap();
        drop(log);

        // When the log is re-opened for appending (not truncated)
        let mut log = JsonlAppendLog::open_for_append(&path).await.unwrap();
        log.append(&WireOp::ContextClear {}).await.unwrap();
        let ops = log.read_all().await.unwrap();

        // Then the original op is preserved and the new op follows it
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], WireOp::Metadata { .. }));
        assert!(matches!(&ops[1], WireOp::ContextClear { .. }));
    }

    #[tokio::test]
    async fn should_create_file_when_opening_for_append_without_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");

        // When a missing log is opened for appending and written
        let mut log = JsonlAppendLog::open_for_append(&path).await.unwrap();
        log.append(&WireOp::ContextClear {}).await.unwrap();

        // Then the file is created and readable back
        assert!(path.exists());
        let ops = log.read_all().await.unwrap();
        assert_eq!(ops.len(), 1);
    }

    #[tokio::test]
    async fn should_fail_appends_after_the_log_is_closed() {
        // Given a log with one recorded op that is then closed (the shutdown
        // drain path: flush, fsync, and release the handle)
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        log.append(&WireOp::ContextClear {}).await.unwrap();
        log.close().await;

        // Then the recorded op is durable and complete on disk
        let (lines, ends_with_newline) = read_raw_lines(&path).await.unwrap();
        assert!(ends_with_newline);
        assert_eq!(lines.len(), 1);

        // And every subsequent append fails instead of silently writing —
        // a closed journal must never accept new ops
        let outcome = log.append(&WireOp::ContextClear {}).await;
        assert!(
            outcome.is_err(),
            "an append on a closed log must fail, not silently drop"
        );
    }

    #[tokio::test]
    async fn should_report_current_file_length_when_statting_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        log.append(&WireOp::ContextClear {}).await.unwrap();

        // When the log's current length is probed (the install-time anchor
        // base), including after a foreign handle grew the file
        let mut foreign = JsonlAppendLog::open_for_append(&path).await.unwrap();
        foreign.append(&WireOp::ContextClear {}).await.unwrap();
        drop(foreign);

        // Then the length reflects the file's real on-disk size, not the
        // writer's own byte count
        let len = log.current_len().await.unwrap();
        let disk_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, disk_len);
        assert!(len > 0);
    }

    #[tokio::test]
    async fn should_report_zero_length_when_statting_a_missing_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let log = JsonlAppendLog::open(&path).await.unwrap();

        // When a log whose file has vanished is probed, the caller (the
        // install anchor) treats missing as zero — best-effort, never an
        // error
        assert_eq!(log.current_len().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn should_return_empty_when_reading_tail_of_missing_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let log = JsonlAppendLog::open(&path).await.unwrap();

        // When a drain is attempted on a journal that no longer exists on
        // disk, the caller folds nothing — NotFound maps to empty, other
        // errors propagate
        let tail = log.read_tail(0).await.unwrap();
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn should_return_bytes_after_offset_when_reading_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        let mut first_len = 0u64;
        for _ in 0..3 {
            first_len += log.append(&WireOp::ContextClear {}).await.unwrap();
        }
        let mut foreign = JsonlAppendLog::open_for_append(&path).await.unwrap();
        let foreign_len = foreign.append(&WireOp::ContextClear {}).await.unwrap();
        drop(foreign);

        // When the incremental absorber reads the tail from its anchor
        let tail = log.read_tail(first_len).await.unwrap();

        // Then exactly the foreign bytes come back, regardless of the
        // writer's own stale fd offset
        assert_eq!(tail.len() as u64, foreign_len);
        assert!(tail.last() == Some(&b'\n'));
    }

    #[tokio::test]
    async fn should_fail_when_reading_tail_errors_on_a_corrupted_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let log = JsonlAppendLog::open(&path).await.unwrap();

        // Given a directory where the log path should be — stat/seek fail
        // with something other than NotFound
        std::fs::create_dir_all(&path).unwrap();

        // When the tail is read, the error propagates (a drain must not
        // silently fold a broken journal)
        assert!(log.read_tail(0).await.is_err());
    }

    #[tokio::test]
    async fn should_keep_lines_intact_when_multiple_tasks_append_concurrently() {
        // Given a log shared by several concurrent writers
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: Some("2.0".to_string()),
            created_at: Some(1),
        })
        .await
        .unwrap();

        // When 8 tasks each append 16 ops through their own clone
        let log = std::sync::Arc::new(tokio::sync::Mutex::new(log));
        let mut tasks = Vec::new();
        for task_index in 0..8u32 {
            let log = std::sync::Arc::clone(&log);
            tasks.push(tokio::spawn(async move {
                for op_index in 0..16u32 {
                    log.lock()
                        .await
                        .append(&WireOp::ContextAppendMessage {
                            message: serde_json::json!({
                                "role": "user",
                                "content": format!("task-{task_index}-op-{op_index}")
                            }),
                        })
                        .await
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        // Then every line parses — no interleaved or torn writes — and
        // the total record count is exactly header + 8 × 16
        let (lines, ends_with_newline) = read_raw_lines(&path).await.unwrap();
        assert!(ends_with_newline, "the file must end with a newline");
        assert_eq!(lines.len(), 1 + 8 * 16);
        for line in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "every concurrent line must stay parseable: {line}"
            );
        }
    }

    #[tokio::test]
    async fn should_read_and_count_identically_when_log_is_compressed_on_disk() {
        use crate::compress::{CompressedShape, compress_wire_log, probe_compressed};

        // Given a log with five ops
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wire.jsonl");
        let mut log = JsonlAppendLog::create(&path).await.unwrap();
        log.append(&WireOp::Metadata {
            protocol_version: Some("2.0".to_string()),
            created_at: Some(1),
        })
        .await
        .unwrap();
        for _ in 0..4 {
            log.append(&WireOp::ContextClear {}).await.unwrap();
        }
        drop(log);
        let plain_ops = JsonlAppendLog::open(&path).await.unwrap();
        let expected: Vec<WireOp> = plain_ops.read_all().await.unwrap();
        let expected_count = plain_ops.count().await.unwrap();
        assert_eq!(expected_count, 5);

        // When the log is compressed on disk (read handle opens the dir)
        compress_wire_log(tmp.path()).unwrap();
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Compressed);
        let reader = JsonlAppendLog::open(&path).await.unwrap();

        // Then both read_all and count see the same lines through the
        // dual-shape entry — transparent to every read consumer
        let compressed_ops: Vec<WireOp> = reader.read_all().await.unwrap();
        assert_eq!(
            compressed_ops.len(),
            expected.len(),
            "compressed read must yield the same ops"
        );
        for (plain, compressed) in expected.iter().zip(&compressed_ops) {
            assert_eq!(
                serde_json::to_string(plain).unwrap(),
                serde_json::to_string(compressed).unwrap()
            );
        }
        assert_eq!(reader.count().await.unwrap(), expected_count);
    }
}
