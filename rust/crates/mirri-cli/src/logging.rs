//! Server logging initialization: console + rotating-file dual sink.
//!
//! `mirri serve` / `mirri web` initialize a layered subscriber before
//! anything else emits a trace:
//!
//! - a **console layer** following the operator's `--log-level` (the
//!   historical behavior — a loopback terminal gets exactly what it asked
//!   for), and
//! - a **file layer** at a fixed `info` floor, appended to
//!   `<mirri-home>/logs/mirri-server.log.<date>` (daily rotation) so the
//!   events that matter for post-hoc debugging (MCP reconcile failures,
//!   restore-on-demand errors, hot-reload failures, graceful shutdown)
//!   survive the process exit — the packaged Desktop server runs as a
//!   background process whose console output is otherwise lost.
//!
//! The file name deliberately differs from the TypeScript engine's
//! `mirri-code.log` so both engines can coexist under one home without
//! interleaving. File logging is best-effort: a logs directory that cannot
//! be created (read-only home, permissions) downgrades to console-only
//! with a stderr warning — it never prevents the server from booting.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// The fixed filter of the file layer: everything at `info` and above is
/// persisted regardless of the operator's console verbosity.
const FILE_LOG_FILTER: &str = "info";

/// What a redacted secret is replaced with in the file layer.
const REDACTION_PLACEHOLDER: &[u8] = b"[REDACTED]";

/// The file-layer log directory under the Mirri home.
pub(crate) fn server_log_dir(mirri_home: &Path) -> PathBuf {
    mirri_home.join("logs")
}

/// Initialize the dual-sink subscriber.
///
/// `secret` (the bearer token) is redacted from every byte the file layer
/// writes, so the one-shot console banner can keep printing it while the
/// rotating file never persists it — a token that already lives in
/// plaintext at `<mirri-home>/server.token` must not additionally linger
/// in dated log files that survive rotation and get attached to bug
/// reports.
///
/// Returns the appender's [`WorkerGuard`] when file logging is active —
/// the caller must keep it alive for as long as the server runs (dropping
/// it flushes and closes the non-blocking writer; dropping it early
/// silently discards every buffered event after that point).
pub(crate) fn init(
    log_level: &str,
    mirri_home: &Path,
    secret: Option<&str>,
) -> Option<WorkerGuard> {
    match prepare_file_appender(mirri_home) {
        Ok((writer, guard)) => {
            let registry = tracing_subscriber::registry().with(console_layer(log_level));
            match secret.filter(|secret| !secret.is_empty()) {
                Some(secret) => registry
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_writer(RedactingWriter::new(writer, secret))
                            .with_ansi(false)
                            .with_filter(EnvFilter::new(FILE_LOG_FILTER)),
                    )
                    .init(),
                None => registry
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_writer(writer)
                            .with_ansi(false)
                            .with_filter(EnvFilter::new(FILE_LOG_FILTER)),
                    )
                    .init(),
            }
            Some(guard)
        }
        Err(error) => {
            // Pre-subscriber stderr: no logger exists yet to carry this.
            eprintln!(
                "warning: file logging disabled ({}); continuing with console only",
                error
            );
            tracing_subscriber::registry()
                .with(console_layer(log_level))
                .init();
            None
        }
    }
}

/// The console layer, following the operator's `--log-level`.
fn console_layer<S>(log_level: &str) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer().with_filter(EnvFilter::new(log_level))
}

/// How many dated log files tracing-appender keeps before deleting the
/// oldest — a bounded retention so a long-lived background server cannot
/// fill the disk with logs.
const MAX_LOG_FILES: usize = 14;

/// Create the logs directory and build the daily-rotating non-blocking
/// appender the file layer writes through.
///
/// The channel is non-lossy: under a burst the writer blocks (bounded) and
/// drops nothing — the whole point of the file sink is that the
/// reconcile/restore events survive for post-hoc debugging, so silently
/// discarding them at capacity would defeat it.
fn prepare_file_appender(mirri_home: &Path) -> std::io::Result<(NonBlocking, WorkerGuard)> {
    let log_dir = server_log_dir(mirri_home);
    std::fs::create_dir_all(&log_dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("mirri-server.log")
        .max_log_files(MAX_LOG_FILES)
        .build(log_dir)
        .map_err(std::io::Error::other)?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .finish(appender);
    Ok((writer, guard))
}

/// A `MakeWriter` that erases every occurrence of a secret from the bytes
/// it writes before passing them to the inner non-blocking writer.
///
/// [`NonBlocking`] is `Clone` (it forwards through an internal channel), so
/// cloning one per event keeps the fmt layer's clone-and-make-writer
/// ergonomics intact while the `needle` is shared immutably.
#[derive(Clone)]
struct RedactingWriter {
    inner: NonBlocking,
    /// Bytes of the secret, shared by every per-event clone.
    needle: std::sync::Arc<Vec<u8>>,
}

impl RedactingWriter {
    fn new(inner: NonBlocking, secret: &str) -> Self {
        Self {
            inner,
            needle: std::sync::Arc::new(secret.as_bytes().to_vec()),
        }
    }
}

impl std::io::Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match redact(buf, &self.needle) {
            Some(redacted) => {
                // Report the ORIGINAL length: the fmt layer writes an event
                // as one buffered blob, and a `write` that reports fewer
                // bytes than handed to it makes the caller retry the
                // un-redacted tail.
                self.inner.write_all(&redacted)?;
                Ok(buf.len())
            }
            None => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Replace every occurrence of `needle` in `haystack` with the redaction
/// placeholder, returning `None` when nothing matched (the common fast
/// path — no allocation, the original bytes are forwarded untouched).
fn redact(haystack: &[u8], needle: &[u8]) -> Option<Vec<u8>> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut redacted = false;
    let mut consumed = 0;
    while let Some(at) = find_subslice(&haystack[consumed..], needle) {
        let at = consumed + at;
        out.extend_from_slice(&haystack[consumed..at]);
        out.extend_from_slice(REDACTION_PLACEHOLDER);
        consumed = at + needle.len();
        redacted = true;
    }
    if !redacted {
        return None;
    }
    out.extend_from_slice(&haystack[consumed..]);
    Some(out)
}

/// Find `needle` in `haystack`, returning its offset.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_place_the_server_log_directory_under_the_mirri_home() {
        // Given a Mirri home
        let home = Path::new("/tmp/mirri-home");

        // When the server log directory is derived
        let dir = server_log_dir(home);

        // Then it lives directly under the home's `logs/`
        assert_eq!(dir, Path::new("/tmp/mirri-home/logs"));
    }

    #[test]
    fn should_create_the_logs_directory_when_preparing_the_file_appender() {
        // Given a fresh Mirri home with no logs directory
        let home = tempfile::tempdir().expect("home tempdir");

        // When the file appender is prepared
        let (_writer, _guard) =
            prepare_file_appender(home.path()).expect("the appender is prepared");

        // Then the logs directory exists (the appender itself writes
        // lazily on the first event, so only the directory is asserted)
        assert!(home.path().join("logs").is_dir());
    }

    #[test]
    fn should_redact_every_occurrence_of_the_secret_from_a_log_line() {
        // Given a formatted log line embedding the bearer token twice
        let line = b"INFO auth token: tok-abc123  and again tok-abc123".as_slice();

        // When it is redacted against the token
        let redacted = redact(line, b"tok-abc123").expect("a redacted copy is produced");

        // Then every occurrence is replaced and the surrounding text kept
        assert_eq!(
            String::from_utf8(redacted).expect("the line stays valid UTF-8"),
            "INFO auth token: [REDACTED]  and again [REDACTED]"
        );
    }

    #[test]
    fn should_return_none_when_the_line_contains_no_secret() {
        // Given a formatted log line with no occurrence of the secret
        let line = b"INFO starting Mirri Code server http_addr=127.0.0.1:59914".as_slice();

        // When it is redacted against a token that is not present
        // Then no redacted copy is produced (the original is forwarded)
        assert!(redact(line, b"tok-abc123").is_none());
    }

    #[test]
    fn should_return_none_when_the_secret_is_empty_or_longer_than_the_line() {
        // Given a log line and edge-case needles
        let line = b"INFO event".as_slice();

        // When redaction is attempted with an empty needle or one longer
        // than the line
        // Then nothing matches in either case
        assert!(redact(line, b"").is_none());
        assert!(redact(line, b"a-needle-longer-than-the-line").is_none());
    }

    #[test]
    fn should_redact_a_secret_at_the_start_and_end_of_the_line() {
        // Given a line whose only content is the secret, and one with the
        // secret at both boundaries
        let secret = b"tok-xyz".as_slice();

        // When redaction runs on the bare secret
        let bare = redact(secret, secret).expect("the bare secret is redacted");

        // Then the whole line becomes the placeholder
        assert_eq!(bare.as_slice(), REDACTION_PLACEHOLDER);

        // And when the secret appears at both ends of surrounding text
        let sandwich = redact(b"tok-xyz mid tok-xyz".as_slice(), secret)
            .expect("both occurrences are redacted");

        // Then both boundary matches are replaced and the middle kept
        assert_eq!(
            String::from_utf8(sandwich).expect("the line stays valid UTF-8"),
            "[REDACTED] mid [REDACTED]"
        );
    }

    #[test]
    fn should_report_the_original_length_when_the_redacted_buffer_is_longer() {
        use std::io::Write as _;

        // Given a shared in-memory sink and a writer whose redaction
        // placeholder is longer than the secret
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (writer, guard) = tracing_appender::non_blocking(SharedSink(sink.clone()));
        let mut redacting = RedactingWriter::new(writer, "tok-xyz");

        // When a line containing the secret is written
        let reported = redacting
            .write(b"INFO auth token: tok-xyz".as_slice())
            .expect("the write succeeds");

        // Then the reported count matches the ORIGINAL input length (the
        // fmt layer's write_all contract)
        assert_eq!(reported, "INFO auth token: tok-xyz".len());

        // And once the channel drains (dropping the guard joins the worker
        // after it consumes every buffered message), the sink holds the
        // redacted bytes
        drop(redacting);
        drop(guard);
        let written = std::mem::take(&mut *sink.lock().expect("the sink lock is live"));
        assert_eq!(
            String::from_utf8(written).expect("the sink holds valid UTF-8"),
            "INFO auth token: [REDACTED]"
        );
    }
    /// An in-memory writer the non-blocking channel can forward to.
    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the sink lock is live")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
