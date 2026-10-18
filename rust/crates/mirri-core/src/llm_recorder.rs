//! LLM request/response recorder for debugging and replay.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use mirri_os::SyncFs;

use crate::usage::TokenUsage;

/// Environment variable to enable LLM recording.
pub const LLM_RECORD_ENV: &str = "MIRRICODE_LLM_RECORD";
/// Default log file name.
pub const LLM_RECORD_FILE_NAME: &str = "llm_log.jsonl";

/// Check if LLM recording is enabled via environment variable.
pub fn llm_recording_enabled_from_env() -> bool {
    std::env::var(LLM_RECORD_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// A single LLM request/response record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRecord {
    pub timestamp: String,
    pub model: String,
    pub provider: String,
    pub request_body: serde_json::Value,
    pub response_body: Option<serde_json::Value>,
    pub latency_ms: u64,
    pub token_usage: Option<TokenUsage>,
}

/// Handle returned by `record_request` for matching with `record_response`.
#[derive(Debug, Clone)]
pub struct RecordingHandle {
    timestamp: String,
    model: String,
    provider: String,
    request_body: serde_json::Value,
}

/// LLM recorder that writes request/response pairs to a JSONL file.
///
/// File IO goes through the injected [`SyncFs`] provider (zero-IO T13b;
/// `new` defaults to `LocalSyncFs`): each record is appended (with fsync)
/// as one line, preserving the original append-only JSONL semantics.
pub struct LlmRecorder {
    enabled: bool,
    log_path: Option<PathBuf>,
    fs: Arc<dyn SyncFs>,
    /// Serializes appends — the original recorder held a single shared
    /// writer handle, so concurrent `record_response` calls append one at
    /// a time.
    write_lock: Mutex<()>,
}

impl LlmRecorder {
    /// Create a new recorder. When `enabled`, opens `<session_dir>/llm_log.jsonl` for appending.
    pub fn new(session_dir: &Path, enabled: bool) -> Self {
        Self::with_fs(session_dir, enabled, Arc::new(mirri_os::LocalSyncFs::new()))
    }

    /// Create a new recorder with an injected filesystem provider
    /// (zero-IO T13b).
    ///
    /// When `enabled`, the parent directory is created and the log file is
    /// created eagerly (an empty append), matching the original
    /// constructor's open-for-append: a failure to open disables recording.
    pub fn with_fs(session_dir: &Path, enabled: bool, fs: Arc<dyn SyncFs>) -> Self {
        let write_lock = Mutex::new(());
        if !enabled {
            return Self {
                enabled: false,
                log_path: None,
                fs,
                write_lock,
            };
        }
        let log_path = session_dir.join(LLM_RECORD_FILE_NAME);
        if let Some(parent) = log_path.parent() {
            let _ = fs.mkdir(parent, true);
        }
        let enabled = match fs.append(&log_path, "") {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("failed to open LLM log file {:?}: {}", log_path, e);
                false
            }
        };
        Self {
            enabled,
            log_path: Some(log_path),
            fs,
            write_lock,
        }
    }

    /// Whether recording is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Path to the log file, if any.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// Record a request. Returns a handle to match with the response.
    pub async fn record_request(
        &self,
        model: &str,
        provider: &str,
        body: &serde_json::Value,
    ) -> RecordingHandle {
        let handle = RecordingHandle {
            timestamp: Utc::now().to_rfc3339(),
            model: model.to_string(),
            provider: provider.to_string(),
            request_body: body.clone(),
        };
        if !self.enabled {
            return handle;
        }
        // Request is not written until response arrives (append-only file).
        handle
    }

    /// Record a response, completing the record.
    pub async fn record_response(
        &self,
        handle: RecordingHandle,
        body: &serde_json::Value,
        usage: Option<TokenUsage>,
        latency_ms: u64,
    ) {
        if !self.enabled {
            return;
        }
        let record = LlmRecord {
            timestamp: handle.timestamp,
            model: handle.model,
            provider: handle.provider,
            request_body: handle.request_body,
            response_body: Some(body.clone()),
            latency_ms,
            token_usage: usage,
        };
        let Some(log_path) = &self.log_path else {
            return;
        };
        // One writer at a time, like the original shared handle.
        let _guard = self.write_lock.lock().await;
        match serde_json::to_string(&record) {
            Ok(line) => {
                // `writeln!` semantics: the JSON line plus a trailing
                // newline, appended through the injected provider.
                let mut line = line;
                line.push('\n');
                if let Err(e) = self.fs.append(log_path, &line) {
                    tracing::warn!("failed to write LLM log line: {}", e);
                }
            }
            Err(e) => tracing::warn!("failed to serialize LLM record: {}", e),
        }
    }

    /// Replay all records from a JSONL file.
    pub fn replay_from_jsonl(path: &Path) -> anyhow::Result<Vec<LlmRecord>> {
        Self::replay_from_jsonl_with(&mirri_os::LocalSyncFs, path)
    }

    /// Replay all records from a JSONL file read through the injected
    /// filesystem provider (zero-IO T13b).
    pub fn replay_from_jsonl_with(fs: &dyn SyncFs, path: &Path) -> anyhow::Result<Vec<LlmRecord>> {
        let content = fs
            .read_text(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", path.display(), e))?;
        let mut records = Vec::new();
        for (i, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: LlmRecord = serde_json::from_str(line)
                .map_err(|e| anyhow::anyhow!("failed to parse line {}: {}", i + 1, e))?;
            records.push(record);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        // tempfile::tempdir guarantees a unique directory (SystemTime-based
        // ids collided under parallel test threads).
        tempfile::tempdir().expect("tempdir should succeed").keep()
    }

    #[tokio::test]
    async fn should_record_llm_request_and_response_to_jsonl() {
        let dir = tmp_dir();
        let recorder = LlmRecorder::new(&dir, true);
        assert!(recorder.is_enabled());

        let handle = recorder
            .record_request(
                "claude-sonnet-4",
                "anthropic",
                &serde_json::json!({"messages": []}),
            )
            .await;
        recorder
            .record_response(handle, &serde_json::json!({"content": "hello"}), None, 100)
            .await;

        let log_path = dir.join(LLM_RECORD_FILE_NAME);
        let records = LlmRecorder::replay_from_jsonl(&log_path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "claude-sonnet-4");
        assert_eq!(records[0].provider, "anthropic");
        assert_eq!(records[0].latency_ms, 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_include_timestamp_and_token_usage_in_record() {
        let dir = tmp_dir();
        let recorder = LlmRecorder::new(&dir, true);

        let handle = recorder
            .record_request("gpt-4", "openai", &serde_json::json!({}))
            .await;
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        recorder
            .record_response(handle, &serde_json::json!({}), Some(usage), 200)
            .await;

        let log_path = dir.join(LLM_RECORD_FILE_NAME);
        let records = LlmRecorder::replay_from_jsonl(&log_path).unwrap();
        assert!(!records[0].timestamp.is_empty());
        assert!(records[0].token_usage.is_some());
        assert_eq!(records[0].token_usage.as_ref().unwrap().total(), 150);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_not_record_when_recording_is_disabled() {
        let dir = tmp_dir();
        let recorder = LlmRecorder::new(&dir, false);
        assert!(!recorder.is_enabled());

        let handle = recorder
            .record_request("model", "provider", &serde_json::json!({}))
            .await;
        recorder
            .record_response(handle, &serde_json::json!({}), None, 0)
            .await;

        let log_path = dir.join(LLM_RECORD_FILE_NAME);
        assert!(!log_path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn should_replay_requests_from_recorded_jsonl() {
        let dir = tmp_dir();
        let log_path = dir.join(LLM_RECORD_FILE_NAME);
        std::fs::write(&log_path, "").unwrap();

        // Write two records manually
        let r1 = LlmRecord {
            timestamp: "2024-01-01T00:00:00Z".into(),
            model: "m1".into(),
            provider: "p1".into(),
            request_body: serde_json::json!({}),
            response_body: None,
            latency_ms: 10,
            token_usage: None,
        };
        let r2 = LlmRecord {
            timestamp: "2024-01-01T00:00:01Z".into(),
            model: "m2".into(),
            provider: "p2".into(),
            request_body: serde_json::json!({}),
            response_body: None,
            latency_ms: 20,
            token_usage: None,
        };
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(&r1).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&r2).unwrap()).unwrap();
        writeln!(f).unwrap(); // blank line should be skipped
        drop(f);

        let records = LlmRecorder::replay_from_jsonl(&log_path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].model, "m1");
        assert_eq!(records[1].model, "m2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn should_handle_multiple_records_in_sequence() {
        let dir = tmp_dir();
        let recorder = LlmRecorder::new(&dir, true);

        for i in 0..5 {
            let handle = recorder
                .record_request(
                    &format!("model-{}", i),
                    "provider",
                    &serde_json::json!({"i": i}),
                )
                .await;
            recorder
                .record_response(handle, &serde_json::json!({"i": i}), None, i as u64 * 10)
                .await;
        }

        let log_path = dir.join(LLM_RECORD_FILE_NAME);
        let records = LlmRecorder::replay_from_jsonl(&log_path).unwrap();
        assert_eq!(records.len(), 5);
        for (i, r) in records.iter().enumerate() {
            assert_eq!(r.model, format!("model-{}", i));
            assert_eq!(r.latency_ms, i as u64 * 10);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // T-13b2-3 — record and replay through an injected SyncFs double
    // -----------------------------------------------------------------------

    /// [`SyncFs`] double that records every append and replays the recorded
    /// appends as the current file contents.
    #[derive(Default)]
    struct RecordingSyncFs {
        appends: std::sync::Mutex<Vec<(PathBuf, String)>>,
    }

    impl RecordingSyncFs {
        fn append_calls(&self) -> Vec<(PathBuf, String)> {
            self.appends.lock().unwrap().clone()
        }
    }

    impl mirri_os::SyncFs for RecordingSyncFs {
        fn mkdir(&self, _path: &Path, _recursive: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn append(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
            self.appends
                .lock()
                .unwrap()
                .push((path.to_path_buf(), contents.to_string()));
            Ok(())
        }

        fn read_text(&self, path: &Path) -> anyhow::Result<String> {
            let appends = self.appends.lock().unwrap();
            let mut content = String::new();
            let mut exists = false;
            for (appended_path, chunk) in appends.iter() {
                if appended_path == path {
                    exists = true;
                    content.push_str(chunk);
                }
            }
            if exists {
                Ok(content)
            } else {
                Err(anyhow::anyhow!("no such file: {}", path.display()))
            }
        }
    }

    #[tokio::test]
    async fn should_record_and_replay_through_an_injected_sync_fs_double() {
        // Given an enabled recorder whose filesystem is an in-memory double
        let fs = Arc::new(RecordingSyncFs::default());
        let recorder = LlmRecorder::with_fs(
            Path::new("/virtual/session"),
            true,
            Arc::clone(&fs) as Arc<dyn mirri_os::SyncFs>,
        );
        assert!(recorder.is_enabled());

        // The log file is created eagerly through the provider (the
        // original constructor's open-for-append)
        let log_path = PathBuf::from("/virtual/session").join(LLM_RECORD_FILE_NAME);
        assert_eq!(
            fs.append_calls(),
            vec![(log_path.clone(), String::new())],
            "constructing an enabled recorder must create the log through the provider"
        );

        // When a request/response pair is recorded
        let handle = recorder
            .record_request(
                "claude-sonnet-4",
                "anthropic",
                &serde_json::json!({"messages": []}),
            )
            .await;
        recorder
            .record_response(handle, &serde_json::json!({"content": "hello"}), None, 100)
            .await;

        // Then one JSONL line lands through the provider's append
        let appends = fs.append_calls();
        assert_eq!(appends.len(), 2, "exactly the eager open plus one record");
        let line = appends[1].1.trim_end();
        let record: LlmRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.model, "claude-sonnet-4");
        assert_eq!(record.provider, "anthropic");
        assert_eq!(record.latency_ms, 100);

        // And the recorded lines replay back through the provider
        let records = LlmRecorder::replay_from_jsonl_with(&*fs, &log_path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "claude-sonnet-4");
        assert_eq!(records[0].latency_ms, 100);
    }

    #[tokio::test]
    async fn should_not_touch_the_provider_when_recording_is_disabled() {
        // Given a disabled recorder over an in-memory double
        let fs = Arc::new(RecordingSyncFs::default());
        let recorder = LlmRecorder::with_fs(
            Path::new("/virtual/session"),
            false,
            Arc::clone(&fs) as Arc<dyn mirri_os::SyncFs>,
        );
        assert!(!recorder.is_enabled());

        // When a request/response pair is recorded
        let handle = recorder
            .record_request("model", "provider", &serde_json::json!({}))
            .await;
        recorder
            .record_response(handle, &serde_json::json!({}), None, 0)
            .await;

        // Then the provider saw no appends at all
        assert!(fs.append_calls().is_empty());
    }
}
