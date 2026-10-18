//! Micro-compaction — cheap, local, non-LLM truncation of old tool results.
//!
//! Unlike [`FullCompaction`](super::FullCompaction) which calls the LLM to
//! generate a summary, micro-compaction is a **lossy, in-process** operation
//! that replaces the *content* of old tool-result messages with a short
//! placeholder. It never touches user, assistant, or recent messages.
//!
//! ## When it triggers
//!
//! Two conditions, **both** required:
//! 1. **Cold prompt cache** — the session has been idle long enough that the
//!    provider's prompt cache has expired (default: 1 hour). Old tool results
//!    are now being re-billed at full token price with no cache benefit.
//! 2. **Sufficient context usage** — context is at least `min_context_usage_ratio`
//!    (default: 0.5) of the max window. Don't bother trimming a nearly-empty
//!    context.
//!
//! ## What it does
//!
//! Computes a `cutoff` index = `history.len() - keep_recent_messages`. During
//! context projection, any `Tool` message **before the cutoff** whose content
//! is ≥ `min_content_tokens` (default: 100) has its content replaced with the
//! `truncated_marker` text (`[Old tool result content cleared]`).
//!
//! ## Current status
//!
//! **Disabled by default** — matching the TypeScript reference, where the
//! `micro_compaction` experimental flag was removed and `detect()`/`compact()`
//! are no-ops. The framework is in place so it can be re-enabled when ready.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Configuration for micro-compaction.
#[derive(Debug, Clone)]
pub struct MicroCompactionConfig {
    /// Number of recent messages to always keep untouched.
    pub keep_recent_messages: usize,
    /// Minimum token count of a tool result's content to be eligible for
    /// truncation. Smaller results are left alone.
    pub min_content_tokens: usize,
    /// How long the session must be idle before micro-compaction triggers
    /// (prompt cache cold threshold).
    pub cache_missed_threshold: Duration,
    /// Minimum context usage ratio (context_tokens / max_context_tokens)
    /// before micro-compaction is considered.
    pub min_context_usage_ratio: f64,
    /// The placeholder text that replaces cleared tool-result content.
    pub truncated_marker: &'static str,
}

impl Default for MicroCompactionConfig {
    fn default() -> Self {
        Self {
            keep_recent_messages: 20,
            min_content_tokens: 100,
            cache_missed_threshold: Duration::from_secs(3600), // 1 hour
            min_context_usage_ratio: 0.5,
            truncated_marker: "[Old tool result content cleared]",
        }
    }
}

/// State for micro-compaction.
///
/// Holds the current cutoff index and the timestamp of the last assistant
/// message (for cache-cold detection). Thread-safe via internal Mutex.
pub struct MicroCompaction {
    config: MicroCompactionConfig,
    /// The cutoff index; messages before this index are eligible for
    /// truncation. `None` means micro-compaction is not active.
    cutoff: Mutex<Option<usize>>,
    /// When the last assistant message was observed.
    last_assistant_at: Mutex<Option<Instant>>,
    /// Whether micro-compaction is enabled. Defaults to `false` (matching TS).
    enabled: bool,
}

impl MicroCompaction {
    /// Create a new micro-compaction instance with the given config.
    pub fn new(config: MicroCompactionConfig) -> Self {
        Self {
            config,
            cutoff: Mutex::new(None),
            last_assistant_at: Mutex::new(None),
            enabled: false,
        }
    }

    /// Create a disabled micro-compaction instance (no-op).
    pub fn disabled() -> Self {
        Self::new(MicroCompactionConfig::default())
    }

    /// Enable micro-compaction. Off by default.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Check if micro-compaction is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record that an assistant message was just produced.
    ///
    /// Called after each successful turn to track the prompt-cache freshness.
    pub fn observe_assistant_message(&self) {
        let mut ts = self
            .last_assistant_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *ts = Some(Instant::now());
    }

    /// Detect whether micro-compaction should trigger.
    ///
    /// Returns `Some(cutoff)` if both conditions are met (cold cache +
    /// sufficient context usage), or `None` otherwise. When it returns
    /// `Some`, the cutoff is stored and will be applied during projection.
    pub fn detect(
        &self,
        history_len: usize,
        context_tokens: usize,
        max_context_tokens: usize,
    ) -> Option<usize> {
        if !self.enabled || history_len == 0 || max_context_tokens == 0 {
            return None;
        }

        // Condition 1: cold prompt cache.
        let cache_cold = {
            let ts = self
                .last_assistant_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match *ts {
                Some(t) => t.elapsed() >= self.config.cache_missed_threshold,
                None => false, // No assistant message yet — no cache to expire.
            }
        };
        if !cache_cold {
            return None;
        }

        // Condition 2: sufficient context usage.
        let usage_ratio = context_tokens as f64 / max_context_tokens as f64;
        if usage_ratio < self.config.min_context_usage_ratio {
            return None;
        }

        // Compute cutoff: keep the last `keep_recent_messages` untouched.
        let cutoff = history_len.saturating_sub(self.config.keep_recent_messages);
        if cutoff == 0 {
            return None; // Not enough history to trim.
        }

        // Store the cutoff.
        let mut guard = self.cutoff.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(cutoff);
        Some(cutoff)
    }

    /// Get the current cutoff index, if micro-compaction is active.
    pub fn cutoff(&self) -> Option<usize> {
        let guard = self.cutoff.lock().unwrap_or_else(|e| e.into_inner());
        *guard
    }

    /// Get the truncated marker text.
    pub fn truncated_marker(&self) -> &'static str {
        self.config.truncated_marker
    }

    /// Get the minimum content tokens threshold.
    pub fn min_content_tokens(&self) -> usize {
        self.config.min_content_tokens
    }

    /// Reset micro-compaction state.
    ///
    /// Called on context clear, undo, or after a full compaction — the cutoff
    /// is invalidated because the history has been rewritten.
    pub fn reset(&self) {
        let mut guard = self.cutoff.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

impl Default for MicroCompaction {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// should return None when micro-compaction is disabled
    #[test]
    fn should_return_none_when_disabled() {
        let mc = MicroCompaction::disabled();
        let result = mc.detect(100, 50000, 100000);
        assert!(result.is_none());
    }

    /// should return None when history is empty
    #[test]
    fn should_return_none_when_history_empty() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        let result = mc.detect(0, 50000, 100000);
        assert!(result.is_none());
    }

    /// should return None when context usage is below threshold
    #[test]
    fn should_return_none_when_context_usage_low() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        // Set last_assistant_at to old time to satisfy cold-cache condition.
        {
            let mut ts = mc.last_assistant_at.lock().unwrap();
            *ts = Some(Instant::now() - Duration::from_secs(7200));
        }
        // Context usage = 10000/100000 = 0.1 < 0.5
        let result = mc.detect(100, 10000, 100000);
        assert!(result.is_none());
    }

    /// should return None when cache is still warm
    #[test]
    fn should_return_none_when_cache_warm() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        // Set last_assistant_at to now (warm cache).
        {
            let mut ts = mc.last_assistant_at.lock().unwrap();
            *ts = Some(Instant::now());
        }
        // Context usage = 50000/100000 = 0.5 >= 0.5
        let result = mc.detect(100, 50000, 100000);
        assert!(result.is_none());
    }

    /// should return cutoff when cache is cold and context usage is high
    #[test]
    fn should_return_cutoff_when_cache_cold_and_usage_high() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        // Set last_assistant_at to 2 hours ago (cold cache).
        {
            let mut ts = mc.last_assistant_at.lock().unwrap();
            *ts = Some(Instant::now() - Duration::from_secs(7200));
        }
        // Context usage = 60000/100000 = 0.6 >= 0.5
        let cutoff = mc.detect(100, 60000, 100000);
        assert!(cutoff.is_some());
        // cutoff = 100 - 20 = 80
        assert_eq!(cutoff.unwrap(), 80);
    }

    /// should return cutoff of 0 when history is shorter than keep_recent
    #[test]
    fn should_return_none_when_history_shorter_than_keep_recent() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        {
            let mut ts = mc.last_assistant_at.lock().unwrap();
            *ts = Some(Instant::now() - Duration::from_secs(7200));
        }
        // history_len=10, keep_recent=20 → cutoff = 0
        let result = mc.detect(10, 60000, 100000);
        assert!(result.is_none());
    }

    /// should not trigger when no assistant message has been observed
    #[test]
    fn should_not_trigger_when_no_assistant_message() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        // No observe_assistant_message call — last_assistant_at is None.
        // TS treats this as cache-not-missed (no cache to expire).
        let cutoff = mc.detect(100, 60000, 100000);
        assert!(cutoff.is_none());
    }

    /// should reset cutoff on reset()
    #[test]
    fn should_reset_cutoff_on_reset() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        {
            let mut ts = mc.last_assistant_at.lock().unwrap();
            *ts = Some(Instant::now() - Duration::from_secs(7200));
        }
        mc.detect(100, 60000, 100000);
        assert!(mc.cutoff().is_some());

        mc.reset();
        assert!(mc.cutoff().is_none());
    }

    /// should observe assistant message timestamp
    #[test]
    fn should_observe_assistant_message_timestamp() {
        let mut mc = MicroCompaction::disabled();
        mc.enable();
        mc.observe_assistant_message();
        // Cache is now warm.
        let result = mc.detect(100, 60000, 100000);
        assert!(result.is_none());
    }
}
