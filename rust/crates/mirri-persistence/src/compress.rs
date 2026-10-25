//! Gzip compression / decompression primitives for session wire logs
//! (session-retention domain, ADR-005: whole-file gzip).
//!
//! The on-disk shape of a session's `wire.jsonl` is the single source of
//! truth for its compressed state (ADR-007):
//!
//! - **Plain** — `wire.jsonl` present (`.gz` may linger as a crash leftover;
//!   the plain file wins).
//! - **Compressed** — `wire.jsonl.gz` present, `wire.jsonl` absent.
//! - **Missing** — neither file.
//!
//! Both transitions go through the crash-safe temp-file + fsync + rename
//! pattern (same as `write_file_atomically`): a crash mid-compression
//! leaves the original untouched; a crash between the `.gz` rename and
//! the `.jsonl` removal leaves both files, which the shape probe classifies
//! as Plain (the restore side of the crash matrix, §7).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// Fixed streaming buffer: memory usage is decoupled from log size (§8).
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// The `wire.jsonl` file name inside a session directory.
pub const WIRE_LOG_FILE: &str = "wire.jsonl";

/// The gzip form of [`WIRE_LOG_FILE`] inside a session directory.
pub const WIRE_LOG_GZ_FILE: &str = "wire.jsonl.gz";

/// How a session's wire log currently exists on disk (ADR-007 shape truth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressedShape {
    /// `wire.jsonl.gz` present and `wire.jsonl` absent.
    Compressed,
    /// `wire.jsonl` present — plain form wins even if a `.gz` leftover
    /// coexists (crash between the compress-side rename and source removal,
    /// or between restore-side creation and `.gz` removal).
    Plain,
    /// Neither form exists.
    Missing,
}

/// Probe the on-disk shape of `<dir>/wire.jsonl(.gz)` — the single
/// compression-state truth source every consumer (scanner, restore, read
/// path, listing) must consult.
pub fn probe_compressed(dir: &Path) -> CompressedShape {
    let plain = dir.join(WIRE_LOG_FILE);
    let gz = dir.join(WIRE_LOG_GZ_FILE);
    if plain.is_file() {
        CompressedShape::Plain
    } else if gz.is_file() {
        CompressedShape::Compressed
    } else {
        CompressedShape::Missing
    }
}

/// Stream `<dir>/wire.jsonl` through gzip into `wire.jsonl.gz`, replacing it
/// atomically (temp file → fsync → rename → remove source).
///
/// Returns the pre-compression byte size of the wire log — the
/// `preCompressedSize` value the retention domain records for its
/// "savings" computation (§3.1).
///
/// Crash safety (§7 matrix):
/// - During the temp-file write: the original `.jsonl` is untouched; the
///   leftover `.gz.tmp` is ignored by the shape probe (and the persistence
///   domain's "temp files are invisible" rule).
/// - After the rename, before the source removal: both files exist →
///   the probe reports Plain, i.e. "non-compressed + leftover".
pub fn compress_wire_log(dir: &Path) -> Result<u64> {
    let source_path = dir.join(WIRE_LOG_FILE);
    let gz_path = dir.join(WIRE_LOG_GZ_FILE);
    let temp_path = dir.join(format!("{WIRE_LOG_GZ_FILE}.tmp"));

    let pre_compressed_size = std::fs::metadata(&source_path)
        .with_context(|| format!("failed to stat {}", source_path.display()))?
        .len();

    let compress_one = || -> std::io::Result<()> {
        let mut input = std::fs::File::open(&source_path)?;
        let mut encoder = flate2::write::GzEncoder::new(
            std::io::BufWriter::new(std::fs::File::create(&temp_path)?),
            flate2::Compression::default(),
        );
        let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            encoder.write_all(&buffer[..read])?;
        }
        // Finish the gzip stream, flush the buffered writer, then fsync the
        // underlying raw file the encoder wrapped.
        let raw_file = encoder
            .finish()
            .and_then(|writer| writer.into_inner().map_err(std::io::Error::from))?;
        raw_file.sync_all()
    };

    let result =
        compress_one().with_context(|| format!("failed to compress {}", source_path.display()));
    if let Err(error) = result {
        // Best-effort cleanup: a failed compression must not litter the
        // session directory with a partial .gz.tmp.
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp_path, &gz_path) {
        // Same cleanup duty on a failed rename (the crash-between-write-
        // and-rename stand-in): the temp file must not linger either.
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("failed to rename {}", gz_path.display()));
    }
    // The .gz is now durable and in place — compression is complete. Only
    // now may the source go; a crash in between leaves the both-files
    // shape, which the probe classifies as Plain.
    if let Err(error) = std::fs::remove_file(&source_path) {
        // A failed source removal leaves the both-files shape (Plain) — the
        // domain's self-healing converges it on the next pass. Log and
        // continue: the compressed form IS on disk and the caller's next
        // probe sees the crash-matrix "plain + leftover" state.
        tracing::warn!(
            source = %source_path.display(),
            error = %error,
            "failed to remove wire.jsonl after compression; shape stays plain-with-leftover"
        );
    }
    sync_dir_best_effort(dir);

    Ok(pre_compressed_size)
}

/// Restore `<dir>/wire.jsonl.gz` back to `wire.jsonl` (temp file → fsync →
/// rename → remove `.gz`).
///
/// The restore completes before any marker is cleared, so a crash
/// mid-restore leaves either the pure compressed form or the both-files
/// form — both are idempotently re-restorable (§6).
pub fn decompress_wire_log(dir: &Path) -> Result<()> {
    let gz_path = dir.join(WIRE_LOG_GZ_FILE);
    let plain_path = dir.join(WIRE_LOG_FILE);
    let temp_path = dir.join(format!("{WIRE_LOG_FILE}.tmp"));

    let decompress_one = || -> std::io::Result<()> {
        let input = std::fs::File::open(&gz_path)?;
        let mut decoder = flate2::read::GzDecoder::new(input);
        let mut output = std::io::BufWriter::new(std::fs::File::create(&temp_path)?);
        let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
        loop {
            let read = decoder.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
        }
        let raw_file = output.into_inner().map_err(std::io::Error::from)?;
        raw_file.sync_all()
    };

    let result =
        decompress_one().with_context(|| format!("failed to decompress {}", gz_path.display()));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    std::fs::rename(&temp_path, &plain_path)
        .with_context(|| format!("failed to rename {}", plain_path.display()))?;
    std::fs::remove_file(&gz_path)
        .with_context(|| format!("failed to remove {}", gz_path.display()))?;
    sync_dir_best_effort(dir);

    Ok(())
}

/// Open a wire log for line reading in either on-disk shape — the unified
/// dual-shape read entry (ADR-001) every read consumer must go through.
///
/// Plain shape: the file is opened directly. Compressed shape: the file is
/// wrapped in a streaming `GzDecoder`, so memory stays decoupled from log
/// size. Missing shape: yields no lines (an absent log has nothing to say,
/// matching the read-tail NotFound-to-empty precedent).
///
/// The torn-final-line / corruption policy of `JsonlAppendLog` is the
/// caller's (the parse layer's) concern; this entry only supplies lines.
pub fn open_wire_log_lines(dir: &Path) -> std::io::Result<Box<dyn LineRead>> {
    match probe_compressed(dir) {
        CompressedShape::Plain => Ok(Box::new(FileLineRead::new(std::fs::File::open(
            dir.join(WIRE_LOG_FILE),
        )?))),
        CompressedShape::Compressed => Ok(Box::new(FileLineRead::new(
            flate2::read::GzDecoder::new(std::fs::File::open(dir.join(WIRE_LOG_GZ_FILE))?),
        ))),
        CompressedShape::Missing => Ok(Box::new(EmptyLineRead)),
    }
}

/// Read a gzip file fully into memory — the compressed wire log's
/// in-memory read path (the session export's M6 rule rides this: the
/// `.gz` rides archives as its decompressed `wire.jsonl` bytes).
/// Streamed through the fixed copy buffer, never a full intermediate
/// copy beyond the output itself.
pub fn read_gz_to_vec(path: &Path) -> Result<Vec<u8>> {
    let input =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut decoder = flate2::read::GzDecoder::new(input);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out)
        .with_context(|| format!("failed to decompress {}", path.display()))?;
    Ok(out)
}

/// Best-effort directory fsync after a rename, so the new directory entry
/// is durable (mirrors `write_file_atomically`'s tail).
fn sync_dir_best_effort(dir: &Path) {
    if let Err(error) = crate::fs_utils::sync_directory(dir) {
        tracing::warn!(
            directory = %dir.display(),
            error = %error,
            "best-effort directory fsync after wire log shape transition failed"
        );
    }
}

/// A forward-only line reader over some byte source.
pub trait LineRead {
    /// Read the next line (without its terminator) into `buffer`; Ok(false)
    /// means EOF. Lines longer than the internal buffer grow it as needed.
    fn read_line(&mut self, buffer: &mut String) -> std::io::Result<bool>;

    /// Whether the SOURCE stream's last read ended with a `\n` — the
    /// real-file terminator probe (M7: never hardcoded `true`).
    ///
    /// Meaningful only after EOF (`read_line` returned `Ok(false)`): the
    /// torn-final-line policy needs to know whether the LAST line of the
    /// stream carried its terminator before it was stripped. A
    /// fully-terminated stream answers `true`; a torn tail (the last
    /// line was appended without its `\n` — plain OR gzipped) answers
    /// `false`. A stream never read answers `true` (an empty file has
    /// no torn tail).
    fn source_ends_with_newline(&self) -> bool;
}

/// Line reading over any [`Read`] source via [`BufReader::read_line`].
struct FileLineRead<R: Read> {
    inner: BufReader<R>,
    /// The last raw byte the `read_line` calls observed — the real
    /// terminator probe backing `source_ends_with_newline` (M7). `None`
    /// before the first read (and stays `None` on an empty stream).
    last_byte: Option<u8>,
}

impl<R: Read> FileLineRead<R> {
    fn new(source: R) -> Self {
        Self {
            inner: BufReader::with_capacity(COPY_BUFFER_BYTES, source),
            last_byte: None,
        }
    }
}

impl<R: Read> LineRead for FileLineRead<R> {
    fn read_line(&mut self, buffer: &mut String) -> std::io::Result<bool> {
        // read_line appends; give the caller a fresh buffer each call.
        buffer.clear();
        let read = self.inner.read_line(buffer)?;
        if read == 0 {
            buffer.clear();
            return Ok(false);
        }
        // The trailing newline (if any) belongs to the terminator, not the
        // line — mirror `str::lines` semantics.
        if buffer.ends_with('\n') {
            buffer.pop();
            if buffer.ends_with('\r') {
                buffer.pop();
            }
            self.last_byte = Some(b'\n');
        } else {
            self.last_byte = buffer.as_bytes().last().copied();
        }
        Ok(true)
    }

    fn source_ends_with_newline(&self) -> bool {
        // Default true: a never-read or empty stream has no torn tail.
        self.last_byte.is_none_or(|byte| byte == b'\n')
    }
}

/// The Missing-shape reader: zero lines, no IO.
struct EmptyLineRead;

impl LineRead for EmptyLineRead {
    fn read_line(&mut self, _buffer: &mut String) -> std::io::Result<bool> {
        Ok(false)
    }

    fn source_ends_with_newline(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Write a plain wire log with deterministic content into `dir`.
    fn write_plain(dir: &Path, lines: &[&str]) -> PathBuf {
        let path = dir.join(WIRE_LOG_FILE);
        let content = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        std::fs::write(&path, &content).unwrap();
        path
    }

    #[test]
    fn should_report_plain_when_only_wire_jsonl_exists() {
        let tmp = tempfile::tempdir().unwrap();
        write_plain(tmp.path(), &["{}"]);
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Plain);
    }

    #[test]
    fn should_report_compressed_when_only_gz_exists() {
        let tmp = tempfile::tempdir().unwrap();
        write_plain(tmp.path(), &["{}"]);
        compress_wire_log(tmp.path()).unwrap();
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Compressed);
    }

    #[test]
    fn should_report_missing_when_neither_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Missing);
    }

    #[test]
    fn should_report_plain_when_both_files_coexist() {
        let tmp = tempfile::tempdir().unwrap();
        write_plain(tmp.path(), &["{}"]);
        // Crash between the .gz rename and the source removal: both files.
        std::fs::write(tmp.path().join(WIRE_LOG_GZ_FILE), "not even gzip").unwrap();
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Plain);
    }

    #[test]
    fn should_return_byte_identical_content_when_roundtripping_compress_decompress() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..500)
            .map(|i| {
                format!(
                    r#"{{"type":"context.append_message","message":{{"role":"user","content":"record {i} with some padding text to give the compressor something to chew on"}}}}"#
                )
            })
            .collect();
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let original = std::fs::read(write_plain(tmp.path(), &line_refs)).unwrap();

        // When the log is compressed then decompressed
        let pre_size = compress_wire_log(tmp.path()).unwrap();
        decompress_wire_log(tmp.path()).unwrap();

        // Then the restored file is byte-for-byte identical to the original
        let restored = std::fs::read(tmp.path().join(WIRE_LOG_FILE)).unwrap();
        assert_eq!(restored, original);
        // And the reported pre-compression size matches the original
        assert_eq!(pre_size, original.len() as u64);
        // And the .gz is gone and no temp files linger
        assert!(!tmp.path().join(WIRE_LOG_GZ_FILE).exists());
        assert!(!tmp.path().join(format!("{WIRE_LOG_GZ_FILE}.tmp")).exists());
        assert!(!tmp.path().join(format!("{WIRE_LOG_FILE}.tmp")).exists());
    }

    #[test]
    fn should_produce_smaller_gz_when_compressing_textual_log() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..1000)
            .map(|i| {
                format!(
                    r#"{{"type":"context.append_message","message":{{"role":"user","content":"repetitive record number {i}"}}}}"#
                )
            })
            .collect();
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_plain(tmp.path(), &line_refs);

        let pre_size = compress_wire_log(tmp.path()).unwrap();
        let gz_size = std::fs::metadata(tmp.path().join(WIRE_LOG_GZ_FILE))
            .unwrap()
            .len();

        // §3.3: compressed form must be a small fraction of the original
        // for textual JSONL logs (a "small fraction" gate, not a fixed ratio).
        assert!(
            gz_size < pre_size / 2,
            "expected gz ({gz_size}) < 50% of original ({pre_size})"
        );
    }

    #[test]
    fn should_keep_plain_file_intact_when_compression_fails_midway() {
        let tmp = tempfile::tempdir().unwrap();
        let original = std::fs::read(write_plain(
            tmp.path(),
            &[r#"{"type":"context.clear","time":1}"#],
        ))
        .unwrap();

        // Block the final rename: a non-empty directory at the destination
        // path makes the rename fail AFTER the temp .gz was fully written —
        // the crash-between-write-and-rename stand-in.
        let gz_destination = tmp.path().join(WIRE_LOG_GZ_FILE);
        std::fs::create_dir(&gz_destination).unwrap();
        std::fs::write(gz_destination.join("blocker"), b"x").unwrap();

        let outcome = compress_wire_log(tmp.path());

        // Then the compression fails
        assert!(outcome.is_err(), "expected compression to fail");
        // And the plain file is byte-for-byte intact
        let after = std::fs::read(tmp.path().join(WIRE_LOG_FILE)).unwrap();
        assert_eq!(after, original);
        // And no wire.jsonl.gz FILE exists (the blocker directory is not a
        // gz file; is_file() — the probe's criterion — sees no gz)
        assert!(!gz_destination.is_file());
        // And the shape still reads as Plain
        assert_eq!(probe_compressed(tmp.path()), CompressedShape::Plain);
        // And the leftover .gz.tmp is cleaned up by the failure path
        assert!(!tmp.path().join(format!("{WIRE_LOG_GZ_FILE}.tmp")).exists());
    }

    #[test]
    fn should_read_same_lines_in_same_order_when_log_is_compressed() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = [
            r#"{"type":"metadata","protocol_version":"1.5","created_at":123}"#,
            r#"{"type":"context.clear","time":124}"#,
            r#"{"type":"context.append_message","message":{"role":"user"},"time":125}"#,
        ];
        write_plain(tmp.path(), &lines);
        let plain_read: Vec<String> = read_all_lines(tmp.path());

        compress_wire_log(tmp.path()).unwrap();
        let compressed_read: Vec<String> = read_all_lines(tmp.path());

        assert_eq!(plain_read, compressed_read);
        assert_eq!(compressed_read, lines);
    }

    #[test]
    fn should_read_no_lines_when_log_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_all_lines(tmp.path()).is_empty());
    }

    #[test]
    fn should_yield_no_lines_when_gz_is_truncated_garbage() {
        // A corrupted .gz fails the read loudly rather than yielding partial
        // lines — the same never-silently-fold-a-broken-journal stance as
        // read_tail.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(WIRE_LOG_GZ_FILE), "definitely not gzip").unwrap();
        let mut line = String::new();
        let mut reader = open_wire_log_lines(tmp.path()).unwrap();
        assert!(reader.read_line(&mut line).is_err());
    }

    fn read_all_lines(dir: &Path) -> Vec<String> {
        let mut reader = open_wire_log_lines(dir).unwrap();
        let mut lines = Vec::new();
        let mut buffer = String::new();
        while reader.read_line(&mut buffer).unwrap() {
            lines.push(buffer.clone());
        }
        lines
    }
}
