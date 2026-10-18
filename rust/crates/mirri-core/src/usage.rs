//! Per-agent token usage accounting.
//!
//! Ports the TypeScript `usage` domain. [`AgentUsageService`] accumulates
//! token usage reported by the LLM adapter on every chat call, broken down by
//! model name and by the current turn. Listeners registered via
//! [`AgentUsageService::on_did_record`] are notified after each record so the
//! host can react (e.g. persist to storage, update a UI counter).
//!
//! ## Turn lifecycle
//!
//! [`AgentUsageService::reset_current_turn`] is called at the start of each
//! agent turn so that [`AgentUsageService::status`] reports only the current
//! turn's usage in `current_turn`, while `total` and `by_model` keep their
//! cumulative values across the whole session.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TokenUsage
// ---------------------------------------------------------------------------

/// Token usage for a single LLM call or an accumulated total.
///
/// Mirrors the four accounting buckets that providers report: prompt
/// (`input_tokens`), completion (`output_tokens`), cached prompt
/// (`cache_read_tokens`), and cache-write prompt (`cache_creation_tokens`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens consumed.
    pub input_tokens: u64,
    /// Completion tokens generated.
    pub output_tokens: u64,
    /// Prompt tokens served from cache (discounted by most providers).
    pub cache_read_tokens: u64,
    /// Prompt tokens written to cache (will be served as cache reads later).
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    /// The total token count across all four buckets.
    ///
    /// This is a raw sum; it does not apply provider-specific cache discounts.
    /// Use it for upper-bound accounting, not for billing.
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    /// Add another [`TokenUsage`] into this one, field by field.
    ///
    /// Uses saturating addition so an overflow saturates to [`u64::MAX`]
    /// rather than panicking in debug mode.
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
    }
}

// ---------------------------------------------------------------------------
// UsageStatus
// ---------------------------------------------------------------------------

/// A read-only snapshot of the usage service's state.
///
/// Returned by [`AgentUsageService::status`]. All three fields are independent
/// copies so the caller can hold the snapshot without locking the service.
///
/// Serialises with camelCase field names (`byModel`, `currentTurn`, `total`) —
/// the wire spelling the `event.agent.status.updated` payload's `usage`
/// object uses (CR4). The nested [`TokenUsage`] keeps its snake_case serde
/// spelling here; the event projection converts it to the TS `TokenUsage`
/// bucket dialect (`inputOther`/`output`/…) at the boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatus {
    /// Per-model accumulated usage.
    pub by_model: HashMap<String, TokenUsage>,
    /// Cumulative usage across all models and all turns.
    pub total: TokenUsage,
    /// Usage accumulated during the current turn only.
    pub current_turn: TokenUsage,
    /// The session's first LLM call usage — the baseline the "after cold
    /// start" cache-hit metric subtracts (SDR-004). `None` for resumed
    /// sessions whose history predates the field, or before any call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_call: Option<TokenUsage>,
}

// ---------------------------------------------------------------------------
// UsageRecordedContext
// ---------------------------------------------------------------------------

/// The context passed to every listener registered via
/// [`AgentUsageService::on_did_record`].
///
/// Carries the model name, the usage that was just recorded, and an optional
/// source tag (e.g. `"chat"`, `"compaction"`) so listeners can filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecordedContext {
    /// The model name the usage was recorded against.
    pub model: String,
    /// The usage that was just recorded.
    pub usage: TokenUsage,
    /// Optional source tag identifying what triggered the recording.
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// AgentUsageService
// ---------------------------------------------------------------------------

/// Listener callback invoked after each [`AgentUsageService::record`] call.
pub type UsageListener = Box<dyn Fn(&UsageRecordedContext) + Send + Sync>;

/// Per-agent token usage accounting service.
///
/// Accumulates token usage from LLM calls, broken down by model and by the
/// current turn. Listeners can be registered to react to each recording.
///
/// # Example
///
/// ```
/// use mirri_core::usage::{AgentUsageService, TokenUsage};
///
/// let mut service = AgentUsageService::new();
/// service.record("claude-sonnet", TokenUsage {
///     input_tokens: 100,
///     output_tokens: 50,
///     ..Default::default()
/// }, Some("chat".to_string()));
///
/// let status = service.status();
/// assert_eq!(status.total.input_tokens, 100);
/// assert_eq!(status.total.output_tokens, 50);
/// ```
#[derive(Default)]
pub struct AgentUsageService {
    /// Accumulated usage per model name.
    by_model: HashMap<String, TokenUsage>,
    /// Cumulative usage across all models and turns.
    total: TokenUsage,
    /// Usage accumulated during the current turn.
    current_turn: TokenUsage,
    /// The session's first LLM call usage — the cold-start baseline
    /// (SDR-004). Seeded from persisted state on resume, or captured on
    /// the first recorded call of a fresh session.
    first_call: Option<TokenUsage>,
    /// Listeners notified after each [`Self::record`] call.
    listeners: Vec<UsageListener>,
}

impl AgentUsageService {
    /// Create a new service with zeroed usage and no listeners.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a usage entry.
    ///
    /// Accumulates `usage` into `by_model[model]`, `total`, and
    /// `current_turn`, then notifies every registered listener with a
    /// [`UsageRecordedContext`].
    pub fn record(&mut self, model: &str, usage: TokenUsage, source: Option<String>) {
        // SDR-004: the session's first LLM call is the cold-start baseline.
        // Captured only while nothing has accumulated yet — the resume seed
        // (`source == "resume"`) populates totals for a resumed session, so
        // it (and any later call once totals exist) must not masquerade as
        // the session's first call.
        if self.first_call.is_none()
            && source.as_deref() != Some("resume")
            && self.total == TokenUsage::default()
        {
            self.first_call = Some(usage.clone());
        }

        // Accumulate into the per-model bucket.
        self.by_model
            .entry(model.to_string())
            .or_default()
            .add(&usage);

        // Accumulate into the running totals.
        self.total.add(&usage);
        self.current_turn.add(&usage);

        let context = UsageRecordedContext {
            model: model.to_string(),
            usage,
            source,
        };

        for listener in &self.listeners {
            listener(&context);
        }
    }

    /// Return a read-only snapshot of the current usage state.
    pub fn status(&self) -> UsageStatus {
        UsageStatus {
            by_model: self.by_model.clone(),
            total: self.total.clone(),
            current_turn: self.current_turn.clone(),
            first_call: self.first_call.clone(),
        }
    }

    /// Seed the first-call baseline from a resumed session's persisted
    /// state (SDR-004). Called before the resume seed record so the
    /// recorded totals never capture themselves as the first call.
    pub fn set_first_call(&mut self, usage: TokenUsage) {
        self.first_call = Some(usage);
    }

    /// Clear the current-turn accumulator.
    ///
    /// Called at the start of each agent turn so `current_turn` in
    /// [`Self::status`] reflects only the turn in progress.
    pub fn reset_current_turn(&mut self) {
        self.current_turn = TokenUsage::default();
    }

    /// Register a listener invoked after every [`Self::record`] call.
    ///
    /// The listener receives a reference to the [`UsageRecordedContext`]
    /// describing the recorded usage. Listeners are called in registration
    /// order.
    pub fn on_did_record(
        &mut self,
        callback: impl Fn(&UsageRecordedContext) + Send + Sync + 'static,
    ) {
        self.listeners.push(Box::new(callback));
    }
}

impl std::fmt::Debug for AgentUsageService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentUsageService")
            .field("by_model", &self.by_model)
            .field("total", &self.total)
            .field("current_turn", &self.current_turn)
            .field("listeners_count", &self.listeners.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TokenUsage::total
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_zero_total_when_usage_is_default() {
        // Given a default TokenUsage
        let usage = TokenUsage::default();

        // When computing the total
        let total = usage.total();

        // Then the total is zero
        assert_eq!(total, 0);
    }

    #[test]
    fn should_sum_all_buckets_when_computing_total() {
        // Given a TokenUsage with all four buckets populated
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 50,
            cache_creation_tokens: 30,
        };

        // When computing the total
        let total = usage.total();

        // Then the total is the sum of all buckets
        assert_eq!(total, 380);
    }

    // -----------------------------------------------------------------------
    // TokenUsage::add
    // -----------------------------------------------------------------------

    #[test]
    fn should_add_all_fields_when_adding_two_usages() {
        // Given two TokenUsage values
        let mut a = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_creation_tokens: 5,
        };
        let b = TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            cache_read_tokens: 20,
            cache_creation_tokens: 10,
        };

        // When adding b into a
        a.add(&b);

        // Then each field is the sum
        assert_eq!(a.input_tokens, 300);
        assert_eq!(a.output_tokens, 150);
        assert_eq!(a.cache_read_tokens, 30);
        assert_eq!(a.cache_creation_tokens, 15);
    }

    #[test]
    fn should_not_mutate_other_when_adding() {
        // Given two TokenUsage values
        let mut a = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        let b = TokenUsage {
            input_tokens: 200,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };

        // When adding b into a
        a.add(&b);

        // Then b is unchanged
        assert_eq!(b.input_tokens, 200);
        assert_eq!(b.output_tokens, 100);
    }

    // -----------------------------------------------------------------------
    // AgentUsageService::record — accumulation
    // -----------------------------------------------------------------------

    #[test]
    fn should_accumulate_into_total_when_recording_usage() {
        // Given a fresh service
        let mut service = AgentUsageService::new();

        // When recording usage for a model
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 10,
                cache_creation_tokens: 5,
            },
            None,
        );

        // Then the total reflects the recorded usage
        let status = service.status();
        assert_eq!(status.total.input_tokens, 100);
        assert_eq!(status.total.output_tokens, 50);
        assert_eq!(status.total.cache_read_tokens, 10);
        assert_eq!(status.total.cache_creation_tokens, 5);
    }

    #[test]
    fn should_accumulate_per_model_when_recording_multiple_models() {
        // Given a fresh service
        let mut service = AgentUsageService::new();

        // When recording usage for two different models
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );
        service.record(
            "gpt-4",
            TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // Then by_model has separate entries for each model
        let status = service.status();
        assert_eq!(status.by_model.len(), 2);
        assert_eq!(status.by_model["claude-sonnet"].input_tokens, 100);
        assert_eq!(status.by_model["gpt-4"].input_tokens, 200);
        assert_eq!(status.total.input_tokens, 300);
    }

    #[test]
    fn should_accumulate_same_model_when_recording_multiple_times() {
        // Given a service with one recorded entry
        let mut service = AgentUsageService::new();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // When recording another entry for the same model
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 200,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // Then the per-model entry is the sum
        let status = service.status();
        assert_eq!(status.by_model.len(), 1);
        assert_eq!(status.by_model["claude-sonnet"].input_tokens, 300);
        assert_eq!(status.by_model["claude-sonnet"].output_tokens, 150);
        assert_eq!(status.total.input_tokens, 300);
    }

    #[test]
    fn should_split_buckets_when_the_usage_model_key_changes_mid_stream() {
        // Given a service recording one request's usage under a model key
        // (M1 alias — the agent's configured model, the `by_model` key)
        let mut service = AgentUsageService::new();
        service.record(
            "alias-a",
            TokenUsage {
                input_tokens: 11,
                output_tokens: 21,
                ..Default::default()
            },
            None,
        );

        // When the key changes mid-stream (a mid-turn model switch) and
        // the next request records under the new key
        service.record(
            "alias-b",
            TokenUsage {
                input_tokens: 12,
                output_tokens: 22,
                ..Default::default()
            },
            None,
        );

        // Then each bucket carries only its own request's amounts — the
        // pre-switch request's usage stays attributed to the model that
        // served it (spec 11 A13)
        let status = service.status();
        assert_eq!(status.by_model.len(), 2);
        assert_eq!(
            status.by_model["alias-a"],
            TokenUsage {
                input_tokens: 11,
                output_tokens: 21,
                ..Default::default()
            }
        );
        assert_eq!(
            status.by_model["alias-b"],
            TokenUsage {
                input_tokens: 12,
                output_tokens: 22,
                ..Default::default()
            }
        );

        // And the running totals still span both requests
        assert_eq!(status.total.input_tokens, 23);
        assert_eq!(status.total.output_tokens, 43);
        assert_eq!(status.current_turn.output_tokens, 43);
    }

    #[test]
    fn should_accumulate_into_current_turn_when_recording() {
        // Given a fresh service
        let mut service = AgentUsageService::new();

        // When recording usage
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // Then current_turn reflects the recorded usage
        let status = service.status();
        assert_eq!(status.current_turn.input_tokens, 100);
        assert_eq!(status.current_turn.output_tokens, 50);
    }

    // -----------------------------------------------------------------------
    // AgentUsageService::reset_current_turn
    // -----------------------------------------------------------------------

    #[test]
    fn should_clear_current_turn_when_resetting() {
        // Given a service with recorded usage
        let mut service = AgentUsageService::new();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // When resetting the current turn
        service.reset_current_turn();

        // Then current_turn is zeroed but total is preserved
        let status = service.status();
        assert_eq!(status.current_turn.input_tokens, 0);
        assert_eq!(status.current_turn.output_tokens, 0);
        assert_eq!(status.total.input_tokens, 100);
        assert_eq!(status.total.output_tokens, 50);
    }

    #[test]
    fn should_preserve_by_model_when_resetting_current_turn() {
        // Given a service with recorded usage
        let mut service = AgentUsageService::new();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // When resetting the current turn
        service.reset_current_turn();

        // Then by_model is unchanged
        let status = service.status();
        assert_eq!(status.by_model["claude-sonnet"].input_tokens, 100);
    }

    // -----------------------------------------------------------------------
    // AgentUsageService::on_did_record — listeners
    // -----------------------------------------------------------------------

    #[test]
    fn should_notify_listener_when_usage_is_recorded() {
        // Given a service with a registered listener
        let mut service = AgentUsageService::new();
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_clone = received.clone();
        service.on_did_record(move |ctx| {
            received_clone.lock().unwrap().push((
                ctx.model.clone(),
                ctx.usage.clone(),
                ctx.source.clone(),
            ));
        });

        // When recording usage
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            Some("chat".to_string()),
        );

        // Then the listener was called with the recorded context
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, "claude-sonnet");
        assert_eq!(received[0].1.input_tokens, 100);
        assert_eq!(received[0].2, Some("chat".to_string()));
    }

    #[test]
    fn should_notify_multiple_listeners_in_order_when_recorded() {
        // Given a service with two listeners
        let mut service = AgentUsageService::new();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let order1 = order.clone();
        service.on_did_record(move |_| {
            order1.lock().unwrap().push("first");
        });
        let order2 = order.clone();
        service.on_did_record(move |_| {
            order2.lock().unwrap().push("second");
        });

        // When recording usage
        service.record("claude-sonnet", TokenUsage::default(), None);

        // Then listeners were called in registration order
        let order = order.lock().unwrap();
        assert_eq!(*order, vec!["first", "second"]);
    }

    #[test]
    fn should_pass_source_to_listener_when_source_is_provided() {
        // Given a service with a listener
        let mut service = AgentUsageService::new();
        let captured_source = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = captured_source.clone();
        service.on_did_record(move |ctx| {
            *captured.lock().unwrap() = ctx.source.clone();
        });

        // When recording usage with a source
        service.record(
            "claude-sonnet",
            TokenUsage::default(),
            Some("compaction".to_string()),
        );

        // Then the listener received the source
        assert_eq!(
            *captured_source.lock().unwrap(),
            Some("compaction".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // AgentUsageService::status — snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_empty_status_when_newly_created() {
        // Given a fresh service
        let service = AgentUsageService::new();

        // When getting the status
        let status = service.status();

        // Then all fields are empty/zero
        assert!(status.by_model.is_empty());
        assert_eq!(status.total, TokenUsage::default());
        assert_eq!(status.current_turn, TokenUsage::default());
    }

    #[test]
    fn should_return_independent_snapshot_when_status_called_twice() {
        // Given a service with recorded usage
        let mut service = AgentUsageService::new();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // When getting the status and then recording more usage
        let first_status = service.status();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 200,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            None,
        );

        // Then the first snapshot is unchanged (it was a copy)
        assert_eq!(first_status.total.input_tokens, 100);
        assert_eq!(service.status().total.input_tokens, 300);
    }

    // -----------------------------------------------------------------------
    // Serialization round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn should_round_trip_when_serializing_and_deserializing_token_usage() {
        // Given a TokenUsage with all buckets populated
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 50,
            cache_creation_tokens: 30,
        };

        // When serializing and deserializing
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();

        // Then the deserialized value equals the original
        assert_eq!(usage, deserialized);
    }

    // -----------------------------------------------------------------------
    // AgentUsageService::first_call — cold-start baseline (SDR-004)
    // -----------------------------------------------------------------------

    #[test]
    fn should_capture_first_call_when_first_usage_is_recorded() {
        // Given a fresh usage service
        let mut service = AgentUsageService::new();

        // When two real calls are recorded
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            None,
        );
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 200,
                output_tokens: 10,
                ..Default::default()
            },
            None,
        );

        // Then the baseline holds the FIRST call's buckets
        let first = service.status().first_call.expect("first call captured");
        assert_eq!(first.input_tokens, 100);
        assert_eq!(first.output_tokens, 50);
    }

    #[test]
    fn should_not_capture_resume_seed_as_first_call() {
        // Given a resumed session's usage service pre-populated by the
        // resume seed (an old snapshot with no recorded first call)
        let mut service = AgentUsageService::new();
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 800,
                ..Default::default()
            },
            Some("resume".to_string()),
        );

        // When a later real call arrives, the baseline stays honestly
        // absent — it is not the session's first call
        service.record(
            "claude-sonnet",
            TokenUsage {
                input_tokens: 20,
                ..Default::default()
            },
            None,
        );

        assert_eq!(service.status().first_call, None);
    }

    #[test]
    fn should_expose_seeded_first_call_through_status() {
        // Given a resumed session whose persisted state carried the baseline
        let mut service = AgentUsageService::new();
        service.set_first_call(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 30,
            cache_creation_tokens: 5,
        });

        // Then the status exposes it alongside the totals
        let first = service.status().first_call.expect("seeded first call");
        assert_eq!(first.input_tokens, 100);
        assert_eq!(first.cache_read_tokens, 30);
    }
}
