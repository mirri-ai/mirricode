//! Step-level retry — re-enqueue a failed step/driver with per-step and
//! per-driver retry counters.
//!
//! The lower-level [`chat_with_retry`](super::retry::chat_with_retry) handles
//! LLM-request-level retry (exponential backoff around a single `Llm::chat()`
//! call). This module implements the higher-level **step-level retry**
//! mechanism described in the TypeScript `stepRetryService.ts`: when an entire
//! step or driver fails with a retryable error, the failed step is re-enqueued
//! at the head of the step queue with per-step and per-driver retry counters.
//!
//! ## Counters
//!
//! Two independent budgets limit retries:
//!
//! - **`max_retries_per_step`** — total retry attempts allowed for a single
//!   step number. Once exhausted, the step is never retried again (the error
//!   is surfaced to the caller).
//! - **`max_retries_per_driver`** — consecutive failures allowed for a single
//!   driver ID. A success resets the driver's counter. This prevents a single
//!   flaky driver from consuming the entire step retry budget.
//!
//! ## Error matching
//!
//! An error is retryable when its display message contains any of the
//! configured `retryable_error_patterns` substrings (case-insensitive). Common
//! patterns include `"timeout"`, `"connection reset"`, `"503"`, `"429"`, and
//! `"rate limit"`.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::EngineError;

// ---------------------------------------------------------------------------
// StepRetryConfig
// ---------------------------------------------------------------------------

/// Configuration for the step-level retry service.
///
/// Controls how many times a failed step or driver may be retried before the
/// error is surfaced to the caller, and which error messages are considered
/// retryable.
#[derive(Debug, Clone)]
pub struct StepRetryConfig {
    /// Maximum retry attempts per step number (default: 2).
    ///
    /// A "retry" is an additional attempt after the first failure, so the
    /// total attempts for a step is `1 + max_retries_per_step`.
    pub max_retries_per_step: u32,
    /// Maximum consecutive retry attempts per driver ID (default: 3).
    ///
    /// A success resets the driver's consecutive-failure counter to zero.
    pub max_retries_per_driver: u32,
    /// Error message substrings that are considered retryable
    /// (case-insensitive). Common patterns: `"timeout"`,
    /// `"connection reset"`, `"503"`, `"429"`, `"rate limit"`.
    pub retryable_error_patterns: Vec<String>,
}

impl Default for StepRetryConfig {
    fn default() -> Self {
        Self {
            max_retries_per_step: 2,
            max_retries_per_driver: 3,
            retryable_error_patterns: vec![
                "timeout".to_string(),
                "connection reset".to_string(),
                "503".to_string(),
                "429".to_string(),
                "rate limit".to_string(),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// StepRetryDecision
// ---------------------------------------------------------------------------

/// The decision returned by [`StepRetryService::should_retry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepRetryDecision {
    /// Retry the step. `attempt` is the upcoming attempt number (1-based);
    /// `max_retries` is the per-step limit that applies.
    Retry { attempt: u32, max_retries: u32 },
    /// Give up — the error must be surfaced to the caller.
    GiveUp,
}

// ---------------------------------------------------------------------------
// StepRetryState
// ---------------------------------------------------------------------------

/// Per-agent, persistable retry state.
///
/// Tracks how many retry attempts have been consumed for each step number and
/// each driver ID. This state can be serialized and restored across turns so
/// that retry budgets are not reset when a turn boundary is crossed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepRetryState {
    /// Step number → number of retry attempts already consumed.
    step_attempts: HashMap<u32, u32>,
    /// The driver ID that most recently failed (for diagnostics).
    last_failed_driver_id: Option<String>,
    /// Driver ID → consecutive failure count.
    driver_attempts: HashMap<String, u32>,
}

impl StepRetryState {
    /// Create empty retry state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a failure for the given step and driver.
    ///
    /// Increments both the step and driver counters, then checks both budgets
    /// against the provided limits. Returns `true` if neither budget has been
    /// exceeded after recording this failure, `false` if either budget is now
    /// exhausted (i.e. its counter strictly exceeds the configured maximum).
    ///
    /// `max_retries_per_step` and `max_retries_per_driver` are passed in rather
    /// than stored on the state so that [`StepRetryState`] remains a pure,
    /// config-free, serializable value. They mirror the fields of
    /// [`StepRetryConfig`].
    ///
    /// Note: [`StepRetryService::should_retry`] performs its own `>=` budget
    /// check *before* calling this method, so in the normal service flow this
    /// return value is always `true`. The check here is a defensive guarantee
    /// for callers that invoke `record_failure` directly (e.g. replaying
    /// persisted state) without the service guard.
    pub fn record_failure(
        &mut self,
        step: u32,
        driver_id: &str,
        max_retries_per_step: u32,
        max_retries_per_driver: u32,
    ) -> bool {
        let step_count = self.step_attempts.entry(step).or_insert(0);
        *step_count = step_count.saturating_add(1);

        let driver_count = self
            .driver_attempts
            .entry(driver_id.to_string())
            .or_insert(0);
        *driver_count = driver_count.saturating_add(1);

        self.last_failed_driver_id = Some(driver_id.to_string());

        // A budget is exhausted once its counter strictly exceeds the
        // configured maximum number of retries.
        let step_budget_exhausted = *step_count > max_retries_per_step;
        let driver_budget_exhausted = *driver_count > max_retries_per_driver;

        !(step_budget_exhausted || driver_budget_exhausted)
    }

    /// Record a success for the given step and driver.
    ///
    /// Resets the per-step counter for this step and the consecutive-failure
    /// counter for this driver, since a success breaks the failure streak.
    pub fn record_success(&mut self, step: u32, driver_id: &str) {
        self.step_attempts.remove(&step);
        self.driver_attempts.remove(driver_id);
        if self.last_failed_driver_id.as_deref() == Some(driver_id) {
            self.last_failed_driver_id = None;
        }
    }

    /// Reset all retry state for a new turn.
    ///
    /// Step-level retry budgets are scoped to a turn: a new turn starts with
    /// fresh counters so that transient failures in one turn do not penalize
    /// the next.
    pub fn reset_for_turn(&mut self) {
        self.step_attempts.clear();
        self.driver_attempts.clear();
        self.last_failed_driver_id = None;
    }

    /// Reset retry state for a specific step.
    ///
    /// Called when a step completes (whether retried or not) to free its
    /// budget slot. The driver counter is left untouched — driver failure
    /// streaks persist across steps within a turn.
    pub fn reset_for_step(&mut self, step: u32) {
        self.step_attempts.remove(&step);
    }

    /// Current retry attempt count for a step (0 = no retries yet).
    pub fn step_attempt_count(&self, step: u32) -> u32 {
        self.step_attempts.get(&step).copied().unwrap_or(0)
    }

    /// Current consecutive failure count for a driver (0 = no failures or
    /// last call succeeded).
    pub fn driver_attempt_count(&self, driver_id: &str) -> u32 {
        self.driver_attempts.get(driver_id).copied().unwrap_or(0)
    }

    /// The driver ID that most recently failed, if any.
    pub fn last_failed_driver_id(&self) -> Option<&str> {
        self.last_failed_driver_id.as_deref()
    }
}

// ---------------------------------------------------------------------------
// StepRetrySnapshot
// ---------------------------------------------------------------------------

/// Read-only snapshot of the retry service's state for telemetry / activity
/// view.
#[derive(Debug, Clone, Default)]
pub struct StepRetrySnapshot {
    /// Copy of the per-step attempt counters.
    pub step_attempts: HashMap<u32, u32>,
    /// Copy of the per-driver consecutive-failure counters.
    pub driver_attempts: HashMap<String, u32>,
    /// The driver that most recently failed.
    pub last_failed_driver_id: Option<String>,
}

// ---------------------------------------------------------------------------
// StepRetryService
// ---------------------------------------------------------------------------

/// Step-level retry service.
///
/// Wraps [`StepRetryConfig`] and [`StepRetryState`] behind a [`Mutex`] so it
/// can be shared across async tasks (the loop engine, tool scheduler, hooks)
/// without lifetime juggling.
///
/// **NOTE**: The service is wired into the loop engine's step execution path:
/// `run_turn` consults [`StepRetryService::should_retry`] when a step fails
/// and re-enqueues the failed step (same step number) when a retry is
/// granted, or surfaces the error on `GiveUp`. It is activated per turn via
/// [`RunTurnConfig::step_retry`](crate::loop_engine::RunTurnConfig::step_retry),
/// which `TurnFlow` populates with a per-agent service instance. Request-level
/// retry (`chat_with_retry`) still applies independently inside each step.
///
/// # Example
///
/// ```
/// use mirri_core::error::EngineError;
/// use mirri_core::loop_engine::step_retry::{
///     StepRetryConfig, StepRetryDecision, StepRetryService,
/// };
///
/// let service = StepRetryService::new(StepRetryConfig::default());
///
/// let error = EngineError::ProviderError("connection reset by peer".to_string());
/// let decision = service.should_retry(1, "bash-driver", &error);
/// assert_eq!(
///     decision,
///     Some(StepRetryDecision::Retry {
///         attempt: 1,
///         max_retries: 2,
///     })
/// );
/// ```
pub struct StepRetryService {
    config: StepRetryConfig,
    state: Mutex<StepRetryState>,
}

impl StepRetryService {
    /// Create a new service with the given config and fresh state.
    pub fn new(config: StepRetryConfig) -> Self {
        Self {
            config,
            state: Mutex::new(StepRetryState::new()),
        }
    }

    /// Create a new service with default config.
    pub fn with_defaults() -> Self {
        Self::new(StepRetryConfig::default())
    }

    /// Determine whether a failed step should be retried.
    ///
    /// Returns:
    /// - `Some(StepRetryDecision::Retry { attempt, max_retries })` when the
    ///   error is retryable and both the per-step and per-driver budgets have
    ///   not been exhausted. `attempt` is the upcoming attempt number
    ///   (1-based: 1 = first retry, 2 = second retry, …).
    /// - `Some(StepRetryDecision::GiveUp)` when the error is retryable but a
    ///   budget is exhausted.
    /// - `None` when the error is not retryable (no pattern matches).
    pub fn should_retry(
        &self,
        step: u32,
        driver_id: &str,
        error: &EngineError,
    ) -> Option<StepRetryDecision> {
        // 0. Quota exhaustion is terminal (P8b-1): the account is out of
        // balance and no retry can succeed until it is topped up. The
        // substring patterns below would otherwise match the "429" in the
        // provider's message and re-send a request that is guaranteed to
        // fail the same way.
        if matches!(error, EngineError::ProviderQuotaExhausted { .. }) {
            return None;
        }

        // 1. Check if the error is retryable at all. An empty EndTurn
        //    response is retryable BY CLASSIFICATION, not by message
        //    pattern — user-configured patterns must not be able to
        //    silently disable the empty-response guard (SDR-002/ADR-002);
        //    the structural-terminal quota case above is its mirror
        //    image.
        let retryable = matches!(error, EngineError::EmptyLlmResponse)
            || is_retryable_error(error, &self.config.retryable_error_patterns);
        if !retryable {
            return None;
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // 2. Check per-step budget.
        let step_retries_used = state.step_attempt_count(step);
        if step_retries_used >= self.config.max_retries_per_step {
            return Some(StepRetryDecision::GiveUp);
        }

        // 3. Check per-driver budget.
        let driver_retries_used = state.driver_attempt_count(driver_id);
        if driver_retries_used >= self.config.max_retries_per_driver {
            return Some(StepRetryDecision::GiveUp);
        }

        // 4. Record the failure and compute the upcoming attempt number.
        //    `record_failure` increments both counters; the upcoming attempt
        //    is the new step counter value (1-based).
        state.record_failure(
            step,
            driver_id,
            self.config.max_retries_per_step,
            self.config.max_retries_per_driver,
        );
        let attempt = state.step_attempt_count(step);

        Some(StepRetryDecision::Retry {
            attempt,
            max_retries: self.config.max_retries_per_step,
        })
    }

    /// Record a successful step/driver execution.
    ///
    /// Resets the per-step counter for this step and the consecutive-failure
    /// counter for this driver.
    pub fn record_success(&self, step: u32, driver_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.record_success(step, driver_id);
    }

    /// Reset all retry state for a new turn.
    pub fn reset_for_turn(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reset_for_turn();
    }

    /// Reset retry state for a specific step.
    pub fn reset_for_step(&self, step: u32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reset_for_step(step);
    }

    /// Take a read-only snapshot of the retry state for telemetry / activity
    /// view.
    pub fn snapshot(&self) -> StepRetrySnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        StepRetrySnapshot {
            step_attempts: state.step_attempts.clone(),
            driver_attempts: state.driver_attempts.clone(),
            last_failed_driver_id: state.last_failed_driver_id.clone(),
        }
    }

    /// Access the configuration (read-only).
    pub fn config(&self) -> &StepRetryConfig {
        &self.config
    }
}

impl std::fmt::Debug for StepRetryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepRetryService")
            .field("config", &self.config)
            .field(
                "state",
                &self.state.lock().unwrap_or_else(|e| e.into_inner()),
            )
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// is_retryable_error
// ---------------------------------------------------------------------------

/// Check whether an [`EngineError`] is retryable by matching its display
/// message against the given patterns (case-insensitive substring match).
///
/// An empty patterns list means no error is retryable.
///
/// # Example
///
/// ```
/// use mirri_core::error::EngineError;
/// use mirri_core::loop_engine::step_retry::is_retryable_error;
///
/// let patterns = vec!["timeout".to_string(), "503".to_string()];
///
/// let retryable = EngineError::ProviderError("request timeout after 30s".to_string());
/// assert!(is_retryable_error(&retryable, &patterns));
///
/// let not_retryable = EngineError::PermissionDenied("not allowed".to_string());
/// assert!(!is_retryable_error(&not_retryable, &patterns));
/// ```
pub fn is_retryable_error(error: &EngineError, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let message = error.to_string().to_lowercase();
    patterns
        .iter()
        .any(|pattern| message.contains(&pattern.to_lowercase()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    // -----------------------------------------------------------------------
    // is_retryable_error
    // -----------------------------------------------------------------------

    #[test]
    fn should_match_timeout_error_when_checking_retryability() {
        // Given a timeout error and a pattern list containing "timeout"
        let error = EngineError::ProviderError("request timeout after 30s".to_string());
        let patterns = vec!["timeout".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is retryable
        assert!(retryable);
    }

    #[test]
    fn should_match_connection_reset_error_when_checking_retryability() {
        // Given a connection reset error
        let error = EngineError::ProviderError("connection reset by peer".to_string());
        let patterns = vec!["connection reset".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is retryable
        assert!(retryable);
    }

    #[test]
    fn should_match_503_error_when_checking_retryability() {
        // Given a 503 error
        let error =
            EngineError::ProviderError("server returned 503 service unavailable".to_string());
        let patterns = vec!["503".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is retryable
        assert!(retryable);
    }

    #[test]
    fn should_match_rate_limit_error_when_checking_retryability() {
        // Given a rate limit error
        let error = EngineError::ProviderError("429 rate limit exceeded".to_string());
        let patterns = vec!["429".to_string(), "rate limit".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is retryable
        assert!(retryable);
    }

    #[test]
    fn should_not_match_when_checking_non_retryable_error() {
        // Given a permission denied error (not in patterns)
        let error = EngineError::PermissionDenied("tool not allowed".to_string());
        let patterns = vec!["timeout".to_string(), "503".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is not retryable
        assert!(!retryable);
    }

    #[test]
    fn should_match_case_insensitively_when_checking_retryability() {
        // Given an error with uppercase "TIMEOUT" and a lowercase pattern
        let error = EngineError::ProviderError("TIMEOUT occurred".to_string());
        let patterns = vec!["timeout".to_string()];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it matches case-insensitively
        assert!(retryable);
    }

    #[test]
    fn should_never_be_retryable_when_pattern_list_is_empty() {
        // Given any error and an empty pattern list
        let error = EngineError::ProviderError("timeout".to_string());
        let patterns: Vec<String> = vec![];

        // When checking retryability
        let retryable = is_retryable_error(&error, &patterns);

        // Then it is not retryable
        assert!(!retryable);
    }

    // -----------------------------------------------------------------------
    // StepRetryService::should_retry — step retry succeeds on second attempt
    // -----------------------------------------------------------------------

    #[test]
    fn should_allow_first_retry_when_step_retry_budget_is_available() {
        // Given a service with default config (max_retries_per_step = 2)
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());

        // When the first failure occurs
        let decision = service.should_retry(1, "driver-a", &error);

        // Then a retry is allowed with attempt = 1
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    #[test]
    fn should_not_retry_when_the_error_is_quota_exhausted_even_though_the_message_carries_429() {
        // Given a service with default config (whose substring patterns
        // match "429") and a quota-exhausted error whose message carries one
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderQuotaExhausted {
            message: "HTTP 429: exceeded your current quota".to_string(),
            retry_after_ms: Some(3000),
        };

        // When the failure is submitted for a step retry
        let decision = service.should_retry(1, "llm", &error);

        // Then no retry is granted — quota exhaustion is terminal (P8b-1)
        // and must not ride the substring patterns into a doomed re-send.
        assert_eq!(
            decision, None,
            "quota-exhausted errors must never be step-retried"
        );
    }

    #[test]
    fn should_retry_again_when_second_attempt_fails() {
        // Given a service after one retry has been consumed
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());

        service.should_retry(1, "driver-a", &error); // consumes 1st retry

        // When the second failure occurs
        let decision = service.should_retry(1, "driver-a", &error);

        // Then a second retry is allowed with attempt = 2
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 2,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // Step retry exhausted after max_retries_per_step
    // -----------------------------------------------------------------------

    #[test]
    fn should_give_up_when_step_retries_are_exhausted() {
        // Given a service with max_retries_per_step = 2 and both retries consumed
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());

        service.should_retry(1, "driver-a", &error); // attempt 1
        service.should_retry(1, "driver-a", &error); // attempt 2

        // When a third failure occurs
        let decision = service.should_retry(1, "driver-a", &error);

        // Then the service gives up
        assert_eq!(decision, Some(StepRetryDecision::GiveUp));
    }

    #[test]
    fn should_retry_different_step_when_one_step_budget_is_exhausted() {
        // Given a service where step 1 has exhausted its retries
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());

        service.should_retry(1, "driver-a", &error); // step 1, attempt 1
        service.should_retry(1, "driver-a", &error); // step 1, attempt 2

        // When step 2 fails
        let decision = service.should_retry(2, "driver-a", &error);

        // Then step 2 gets its own retry budget
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // Driver retry exhausted after max_retries_per_driver
    // -----------------------------------------------------------------------

    #[test]
    fn should_give_up_when_driver_retries_are_exhausted() {
        // Given a service with max_retries_per_driver = 1 and the driver has
        // already used its one retry
        let config = StepRetryConfig {
            max_retries_per_step: 10,
            max_retries_per_driver: 1,
            retryable_error_patterns: vec!["timeout".to_string()],
        };
        let service = StepRetryService::new(config);
        let error = EngineError::ProviderError("connection timeout".to_string());

        service.should_retry(1, "flaky-driver", &error); // driver retry 1

        // When the same driver fails again (on a different step)
        let decision = service.should_retry(2, "flaky-driver", &error);

        // Then the service gives up because the driver budget is exhausted
        assert_eq!(decision, Some(StepRetryDecision::GiveUp));
    }

    #[test]
    fn should_retry_different_driver_when_one_driver_budget_is_exhausted() {
        // Given a service where "driver-a" has exhausted its driver budget
        let config = StepRetryConfig {
            max_retries_per_step: 10,
            max_retries_per_driver: 1,
            retryable_error_patterns: vec!["timeout".to_string()],
        };
        let service = StepRetryService::new(config);
        let error = EngineError::ProviderError("connection timeout".to_string());

        service.should_retry(1, "driver-a", &error); // driver-a retry 1

        // When a different driver fails
        let decision = service.should_retry(2, "driver-b", &error);

        // Then driver-b gets its own retry budget
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 10,
            })
        );
    }

    // -----------------------------------------------------------------------
    // Non-retryable errors don't trigger retry
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_none_when_error_is_not_retryable() {
        // Given a service and a non-retryable error
        let service = StepRetryService::with_defaults();
        let error = EngineError::PermissionDenied("not allowed".to_string());

        // When checking if the step should be retried
        let decision = service.should_retry(1, "driver-a", &error);

        // Then no decision is returned (the error is not retryable)
        assert_eq!(decision, None);
    }

    #[test]
    fn should_leave_state_unchanged_when_error_is_not_retryable() {
        // Given a service with fresh state
        let service = StepRetryService::with_defaults();
        let error = EngineError::PermissionDenied("not allowed".to_string());

        // When a non-retryable error is checked
        service.should_retry(1, "driver-a", &error);

        // Then no state was recorded
        let snapshot = service.snapshot();
        assert!(snapshot.step_attempts.is_empty());
        assert!(snapshot.driver_attempts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reset on success
    // -----------------------------------------------------------------------

    #[test]
    fn should_reset_counters_when_success_is_recorded_after_retries() {
        // Given a service with one retry consumed for step 1 / driver-a
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error);

        // When a success is recorded
        service.record_success(1, "driver-a");

        // Then both counters are reset
        let snapshot = service.snapshot();
        assert!(!snapshot.step_attempts.contains_key(&1));
        assert!(!snapshot.driver_attempts.contains_key("driver-a"));
        assert!(snapshot.last_failed_driver_id.is_none());
    }

    #[test]
    fn should_restore_full_driver_budget_when_driver_succeeds_after_failure() {
        // Given a service where driver-a used one retry then succeeded
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error); // driver-a: 1 failure
        service.record_success(1, "driver-a"); // reset

        // When driver-a fails again on a new step
        let decision = service.should_retry(2, "driver-a", &error);

        // Then the driver has its full budget back
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // Reset for new turn
    // -----------------------------------------------------------------------

    #[test]
    fn should_clear_all_counters_when_service_resets_for_turn() {
        // Given a service with retries consumed across multiple steps/drivers
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error);
        service.should_retry(2, "driver-b", &error);

        // When the turn is reset
        service.reset_for_turn();

        // Then all counters are cleared
        let snapshot = service.snapshot();
        assert!(snapshot.step_attempts.is_empty());
        assert!(snapshot.driver_attempts.is_empty());
        assert!(snapshot.last_failed_driver_id.is_none());
    }

    #[test]
    fn should_restore_full_budget_when_step_fails_after_turn_reset() {
        // Given a service that was reset for a new turn
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error);
        service.should_retry(1, "driver-a", &error);
        service.reset_for_turn();

        // When a step fails in the new turn
        let decision = service.should_retry(1, "driver-a", &error);

        // Then the full retry budget is available again
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn should_reflect_consumed_retries_when_snapshot_is_taken() {
        // Given a service with known retry state
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error);
        service.should_retry(1, "driver-a", &error);

        // When taking a snapshot
        let snapshot = service.snapshot();

        // Then it reflects the current counters
        assert_eq!(snapshot.step_attempts.get(&1), Some(&2));
        assert_eq!(snapshot.driver_attempts.get("driver-a"), Some(&2));
        assert_eq!(snapshot.last_failed_driver_id.as_deref(), Some("driver-a"));
    }

    // -----------------------------------------------------------------------
    // StepRetryState direct tests
    // -----------------------------------------------------------------------

    #[test]
    fn should_report_zero_counters_when_checking_fresh_state() {
        // Given fresh state
        let state = StepRetryState::new();

        // When checking counters
        // Then they are all zero / empty
        assert_eq!(state.step_attempt_count(1), 0);
        assert_eq!(state.driver_attempt_count("driver-a"), 0);
        assert!(state.last_failed_driver_id().is_none());
    }

    #[test]
    fn should_preserve_other_steps_retries_when_resetting_one_step() {
        // Given state with retries on two steps
        let mut state = StepRetryState::new();
        state.record_failure(1, "driver-a", 2, 3);
        state.record_failure(2, "driver-a", 2, 3);

        // When resetting only step 1
        state.reset_for_step(1);

        // Then step 2's counter is preserved
        assert_eq!(state.step_attempt_count(1), 0);
        assert_eq!(state.step_attempt_count(2), 1);
    }

    // -----------------------------------------------------------------------
    // record_failure return value (budget exhaustion)
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_false_when_record_failure_exceeds_step_budget() {
        // Given state where the step counter is exactly at the limit
        let mut state = StepRetryState::new();
        let max_retries_per_step = 2;
        let max_retries_per_driver = 10;

        // Consume all allowed retries for step 1 (2 failures).
        assert!(
            state.record_failure(1, "driver-a", max_retries_per_step, max_retries_per_driver),
            "first failure should still be within budget"
        );
        assert!(
            state.record_failure(1, "driver-a", max_retries_per_step, max_retries_per_driver),
            "second failure should still be within budget"
        );

        // When a third failure is recorded (exceeding max_retries_per_step = 2)
        let allowed =
            state.record_failure(1, "driver-a", max_retries_per_step, max_retries_per_driver);

        // Then the return value is false — step budget exhausted
        assert!(
            !allowed,
            "record_failure should return false when step budget is exhausted"
        );
    }

    #[test]
    fn should_return_false_when_record_failure_exceeds_driver_budget() {
        // Given state where the driver counter is exactly at the limit
        let mut state = StepRetryState::new();
        let max_retries_per_step = 10;
        let max_retries_per_driver = 1;

        // Consume the single allowed driver retry.
        assert!(
            state.record_failure(
                1,
                "flaky-driver",
                max_retries_per_step,
                max_retries_per_driver
            ),
            "first failure should still be within budget"
        );

        // When a second failure is recorded for the same driver
        let allowed = state.record_failure(
            2,
            "flaky-driver",
            max_retries_per_step,
            max_retries_per_driver,
        );

        // Then the return value is false — driver budget exhausted
        assert!(
            !allowed,
            "record_failure should return false when driver budget is exhausted"
        );
    }

    #[test]
    fn should_return_true_when_record_failure_is_within_both_budgets() {
        // Given fresh state
        let mut state = StepRetryState::new();

        // When recording a failure within both budgets
        let allowed = state.record_failure(1, "driver-a", 2, 3);

        // Then the return value is true
        assert!(
            allowed,
            "record_failure should return true when both budgets are available"
        );
    }

    // -----------------------------------------------------------------------
    // reset_for_step (service level)
    // -----------------------------------------------------------------------

    #[test]
    fn should_reset_counter_and_restore_budget_when_resetting_step() {
        // Given a service where step 1 has consumed one retry
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(1, "driver-a", &error); // step 1, attempt 1

        // When resetting step 1
        service.reset_for_step(1);

        // Then step 1's counter is back to 0
        let snapshot = service.snapshot();
        assert!(!snapshot.step_attempts.contains_key(&1));

        // And a new failure gets the full budget again (attempt = 1)
        let decision = service.should_retry(1, "driver-a", &error);
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // config() accessor
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_custom_config_when_accessing_config() {
        // Given a service created with a specific config
        let config = StepRetryConfig {
            max_retries_per_step: 5,
            max_retries_per_driver: 7,
            retryable_error_patterns: vec!["timeout".to_string(), "503".to_string()],
        };
        let service = StepRetryService::new(config);

        // When accessing the config
        let returned = service.config();

        // Then it returns the config the service was created with
        assert_eq!(returned.max_retries_per_step, 5);
        assert_eq!(returned.max_retries_per_driver, 7);
        assert_eq!(
            returned.retryable_error_patterns,
            vec!["timeout".to_string(), "503".to_string()]
        );
    }

    #[test]
    fn should_match_defaults_when_accessing_config() {
        // Given a service with default config
        let service = StepRetryService::with_defaults();
        let default_config = StepRetryConfig::default();

        // When accessing the config
        let returned = service.config();

        // Then it matches the defaults
        assert_eq!(
            returned.max_retries_per_step,
            default_config.max_retries_per_step
        );
        assert_eq!(
            returned.max_retries_per_driver,
            default_config.max_retries_per_driver
        );
        assert_eq!(
            returned.retryable_error_patterns,
            default_config.retryable_error_patterns
        );
    }

    // -----------------------------------------------------------------------
    // Concurrency
    // -----------------------------------------------------------------------

    #[test]
    fn should_stay_consistent_when_service_is_accessed_concurrently() {
        // Given a shared service accessed by multiple concurrent tasks
        let service = Arc::new(StepRetryService::with_defaults());
        let num_tasks = 4;
        let iterations = 50;

        // When spawning multiple tasks that each call should_retry,
        // record_success, and snapshot in a loop
        let rt = tokio::runtime::Runtime::new().expect("failed to create runtime");
        rt.block_on(async {
            let mut handles = Vec::new();
            for task_id in 0..num_tasks {
                let svc = service.clone();
                handles.push(tokio::spawn(async move {
                    let driver = format!("driver-{task_id}");
                    // EngineError does not implement Clone, so create a fresh
                    // instance inside each task.
                    let err = EngineError::ProviderError("connection timeout".to_string());
                    for i in 0..iterations {
                        let step = (i % 3) as u32;
                        let _ = svc.should_retry(step, &driver, &err);
                        if i % 5 == 0 {
                            svc.record_success(step, &driver);
                        }
                        let _ = svc.snapshot();
                    }
                }));
            }
            for handle in handles {
                handle
                    .await
                    .expect("task should not panic under concurrent access");
            }
        });

        // Then the service is in a consistent state (no panic, snapshot works)
        let snapshot = service.snapshot();
        // Step attempts should never exceed the per-step budget (max_retries_per_step = 2)
        for count in snapshot.step_attempts.values() {
            assert!(
                *count <= 2,
                "step attempt count {count} should not exceed max_retries_per_step (2)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Step 0 (valid u32)
    // -----------------------------------------------------------------------

    #[test]
    fn should_treat_step_zero_as_valid_when_checking_retry() {
        // Given a service with default config
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());

        // When step 0 fails
        let decision = service.should_retry(0, "driver-a", &error);

        // Then it is treated like any valid step number
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );

        // And a second retry for step 0 also works
        let decision = service.should_retry(0, "driver-a", &error);
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 2,
                max_retries: 2,
            })
        );
    }

    #[test]
    fn should_give_up_when_step_zero_exhausts_its_budget() {
        // Given a service where step 0 has exhausted its budget
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(0, "driver-a", &error); // attempt 1
        service.should_retry(0, "driver-a", &error); // attempt 2

        // When a third failure for step 0 occurs
        let decision = service.should_retry(0, "driver-a", &error);

        // Then the service gives up
        assert_eq!(decision, Some(StepRetryDecision::GiveUp));
    }

    #[test]
    fn should_clear_step_zero_counter_when_resetting_step() {
        // Given a service where step 0 has a retry consumed
        let service = StepRetryService::with_defaults();
        let error = EngineError::ProviderError("connection timeout".to_string());
        service.should_retry(0, "driver-a", &error);

        // When resetting step 0
        service.reset_for_step(0);

        // Then step 0's counter is cleared and a new failure gets full budget
        let snapshot = service.snapshot();
        assert!(!snapshot.step_attempts.contains_key(&0));
        let decision = service.should_retry(0, "driver-a", &error);
        assert_eq!(
            decision,
            Some(StepRetryDecision::Retry {
                attempt: 1,
                max_retries: 2,
            })
        );
    }

    // -----------------------------------------------------------------------
    // StepRetryState::record_success (direct)
    // -----------------------------------------------------------------------

    #[test]
    fn should_reset_counters_when_record_success_is_called_directly() {
        // Given state with a failure recorded for step 1 / driver-a
        let mut state = StepRetryState::new();
        state.record_failure(1, "driver-a", 2, 3);

        // Sanity: counters were incremented
        assert_eq!(state.step_attempt_count(1), 1);
        assert_eq!(state.driver_attempt_count("driver-a"), 1);
        assert_eq!(state.last_failed_driver_id(), Some("driver-a"));

        // When recording a success
        state.record_success(1, "driver-a");

        // Then both counters are reset and last_failed_driver_id is cleared
        assert_eq!(state.step_attempt_count(1), 0);
        assert_eq!(state.driver_attempt_count("driver-a"), 0);
        assert!(state.last_failed_driver_id().is_none());
    }

    #[test]
    fn should_reset_only_that_driver_when_record_success_for_one_driver() {
        // Given state with failures from two drivers
        let mut state = StepRetryState::new();
        state.record_failure(1, "driver-a", 5, 5);
        state.record_failure(2, "driver-b", 5, 5);

        // When recording a success for driver-a only
        state.record_success(1, "driver-a");

        // Then driver-a's counters are reset but driver-b's are preserved
        assert_eq!(state.step_attempt_count(1), 0);
        assert_eq!(state.driver_attempt_count("driver-a"), 0);
        assert_eq!(state.step_attempt_count(2), 1);
        assert_eq!(state.driver_attempt_count("driver-b"), 1);
    }

    // -----------------------------------------------------------------------
    // StepRetryState::reset_for_turn (direct)
    // -----------------------------------------------------------------------

    #[test]
    fn should_clear_all_counters_when_state_resets_for_turn() {
        // Given state with failures across multiple steps and drivers
        let mut state = StepRetryState::new();
        state.record_failure(1, "driver-a", 5, 5);
        state.record_failure(2, "driver-b", 5, 5);
        state.record_failure(3, "driver-a", 5, 5);

        // Sanity: state is populated
        assert_eq!(state.step_attempt_count(1), 1);
        assert_eq!(state.step_attempt_count(2), 1);
        assert_eq!(state.step_attempt_count(3), 1);
        assert_eq!(state.driver_attempt_count("driver-a"), 2);
        assert_eq!(state.driver_attempt_count("driver-b"), 1);
        assert!(state.last_failed_driver_id().is_some());

        // When resetting for a new turn
        state.reset_for_turn();

        // Then all counters are cleared
        assert_eq!(state.step_attempt_count(1), 0);
        assert_eq!(state.step_attempt_count(2), 0);
        assert_eq!(state.step_attempt_count(3), 0);
        assert_eq!(state.driver_attempt_count("driver-a"), 0);
        assert_eq!(state.driver_attempt_count("driver-b"), 0);
        assert!(state.last_failed_driver_id().is_none());
    }
}
