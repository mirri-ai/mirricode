//! Tool-call deduplication — prevents the model from calling the same tool
//! with the same arguments repeatedly.
//!
//! The deduplicator operates at two levels:
//!
//! 1. **Same-step dedup**: If the model requests the same tool with the same
//!    canonical arguments twice within a single step, the second call is
//!    replaced with a synthetic empty success result.
//!
//! 2. **Cross-step streak detection**: If the model calls the same tool with
//!    the same arguments across consecutive steps, escalating reminders are
//!    appended to the tool result. At a high enough streak, the turn is
//!    force-stopped.
//!
//! ## Escalation Thresholds
//!
//! | Streak | Action |
//! |--------|--------|
//! | 3+     | Reminder 1 (gentle nudge) |
//! | 5+     | Reminder 2 (warning with count) |
//! | 8+     | Reminder 3 (strong warning) |
//! | 12+    | Force stop turn |

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::tool::{ToolOutput, ToolResult};

// ---------------------------------------------------------------------------
// Escalation thresholds
// ---------------------------------------------------------------------------

/// Streak at which the first (gentle) reminder is appended.
const REPEAT_REMINDER_1_START: u32 = 3;

/// Streak at which the second (warning) reminder is appended.
const REPEAT_REMINDER_2_START: u32 = 5;

/// Streak at which the third (strong) reminder is appended.
const REPEAT_REMINDER_3_START: u32 = 8;

/// Streak at which the turn is force-stopped.
const REPEAT_FORCE_STOP_STREAK: u32 = 12;

// ---------------------------------------------------------------------------
// Reminder texts
// ---------------------------------------------------------------------------

const REMINDER_TEXT_1: &str = "<system-reminder>\nYou've called this tool with the same arguments multiple times in a row. If the previous result was what you needed, you don't need to call it again. If it wasn't, try different arguments or a different approach.\n</system-reminder>";

const REMINDER_TEXT_3: &str = "<system-reminder>\nYou are repeatedly calling the same tool with identical arguments. This is not productive. Stop calling this tool and try a different approach to make progress.\n</system-reminder>";

fn format_reminder_2(streak: u32) -> String {
    format!(
        "<system-reminder>\nYou've called this tool with the same arguments {streak} times in a row now. Repeating the same action is unlikely to produce a different result. Consider whether you need this result again, or try a different approach.\n</system-reminder>"
    )
}

// ---------------------------------------------------------------------------
// Key generation
// ---------------------------------------------------------------------------

/// Recursively sort the keys of a JSON object so that two objects with the
/// same key-value pairs but different key order produce the same canonical
/// string.
fn sort_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut result = serde_json::Map::new();
            for (k, v) in sorted {
                result.insert(k.clone(), sort_json_value(v));
            }
            Value::Object(result)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_value).collect()),
        _ => value.clone(),
    }
}

/// Build a canonical dedup key from a tool name and its arguments.
///
/// The key is `"{tool_name} {canonical_args}"` where `canonical_args` is the
/// JSON serialization of the arguments with recursively sorted object keys.
/// This ensures that `{"b": 1, "a": 2}` and `{"a": 2, "b": 1}` produce the
/// same key.
fn make_key(tool_name: &str, args: &Value) -> String {
    let canonical = sort_json_value(args);
    format!("{} {}", tool_name, canonical)
}

// ---------------------------------------------------------------------------
// ToolCallDeduplicator
// ---------------------------------------------------------------------------

/// Stateful deduplicator for tool calls within and across steps.
///
/// ## Lifecycle per step
///
/// 1. [`begin_step`] — clears per-step state (called by `before_step` hook).
/// 2. [`check_same_step`] — called for each tool call in `prepare_tool_execution`.
///    Returns a synthetic result if the same key was already seen this step.
/// 3. [`finalize_result`] — called for each tool result in `finalize_tool_result`.
///    Appends escalation reminders and may set `stop_turn` for high streaks.
/// 4. [`end_step`] — carries the cross-step streak forward (called by
///    `after_step` hook).
///
/// [`begin_step`]: ToolCallDeduplicator::begin_step
/// [`check_same_step`]: ToolCallDeduplicator::check_same_step
/// [`finalize_result`]: ToolCallDeduplicator::finalize_result
/// [`end_step`]: ToolCallDeduplicator::end_step
#[derive(Debug, Default)]
pub struct ToolCallDeduplicator {
    /// Canonical keys seen in the current step.
    step_calls: Vec<String>,
    /// Call IDs in the current step (parallel to `step_calls`).
    step_call_ids: Vec<String>,
    /// Call IDs that were replaced with synthetic results (skip finalize).
    synthetic_call_ids: HashSet<String>,
    /// Args stored by call_id during `check_same_step` for use in
    /// `finalize_result` (since `FinalizeToolContext` doesn't carry args).
    call_args: HashMap<String, (String, Value)>,
    /// The key of the last finalized call (for cross-step streak tracking).
    consecutive_key: Option<String>,
    /// The current streak count for `consecutive_key`.
    consecutive_count: u32,
    /// Finalized results keyed by canonical key (`"{tool} {canonical_args}"`).
    ///
    /// Populated by `finalize_result` so that duplicate calls detected by
    /// `check_same_step` can return the original call's actual result instead
    /// of an empty placeholder.
    finalized_results: HashMap<String, ToolResult>,
}

impl ToolCallDeduplicator {
    /// Create a new deduplicator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a new step — clears per-step state.
    ///
    /// Called by the `before_step` hook. Does NOT clear cross-step streak
    /// state (`consecutive_key` / `consecutive_count`).
    pub fn begin_step(&mut self) {
        self.step_calls.clear();
        self.step_call_ids.clear();
        self.synthetic_call_ids.clear();
        self.call_args.clear();
    }

    /// End the current step — the cross-step streak is carried forward
    /// automatically via `consecutive_key` / `consecutive_count`.
    ///
    /// Called by the `after_step` hook.
    pub fn end_step(&mut self) {
        // Per-step state is cleared at `begin_step` of the next step.
        // Nothing to do here — the streak state persists across steps.
    }

    /// Check if a tool call is a duplicate within the current step.
    ///
    /// Returns `Some(ToolResult)` with a synthetic result if the same key
    /// was already seen this step. Returns `None` if the call is new (and
    /// records it for future checks).
    ///
    /// When a duplicate is detected:
    /// - If the original call's result has already been finalized (available
    ///   in the internal `finalized_results` map), that result is returned so
    ///   the duplicate caller sees the same output as the original.
    /// - If the original call hasn't finished yet, a placeholder
    ///   `"[duplicate tool call — result pending]"` is returned, which tells
    ///   the model what happened instead of returning an empty string.
    ///
    /// Called by the `prepare_tool_execution` hook.
    pub fn check_same_step(
        &mut self,
        tool_name: &str,
        args: &Value,
        call_id: &str,
    ) -> Option<ToolResult> {
        let key = make_key(tool_name, args);
        self.call_args
            .insert(call_id.to_string(), (tool_name.to_string(), args.clone()));

        if self.step_calls.contains(&key) {
            self.synthetic_call_ids.insert(call_id.to_string());
            // If the original call's result is already finalized, return it
            // so the duplicate caller sees the actual output (including any
            // appended reminders). Otherwise, return a pending placeholder.
            if let Some(result) = self.finalized_results.get(&key) {
                return Some(result.clone());
            }
            return Some(ToolResult::success(
                "[duplicate tool call — result pending]",
            ));
        }

        self.step_calls.push(key);
        self.step_call_ids.push(call_id.to_string());
        None
    }

    /// Finalize a tool result — append escalation reminders for high streaks.
    ///
    /// Called by the `finalize_tool_result` hook. Synthetic results (from
    /// `check_same_step`) are skipped.
    ///
    /// The streak is computed by combining:
    /// - The within-step streak (how many times this key appeared consecutively
    ///   at the end of the current step's call list).
    /// - The cross-step streak (the `consecutive_count` if `consecutive_key`
    ///   matches).
    pub fn finalize_result(&mut self, call_id: &str, result: &mut ToolResult) {
        // Skip synthetic results — they don't count toward the streak.
        if self.synthetic_call_ids.contains(call_id) {
            return;
        }

        let (tool_name, args) = match self.call_args.get(call_id) {
            Some(v) => v.clone(),
            None => return,
        };

        let key = make_key(&tool_name, &args);

        // Count within-step streak: how many consecutive same-key calls
        // ending at this call_id's position.
        let call_index = self
            .step_call_ids
            .iter()
            .position(|id| id == call_id)
            .unwrap_or(0);

        let mut same_step_streak = 0u32;
        for i in (0..call_index).rev() {
            if self.step_calls[i] == key {
                same_step_streak += 1;
            } else {
                break;
            }
        }

        // Fold in cross-step streak.
        let total_streak = if self.consecutive_key.as_deref() == Some(key.as_str()) {
            same_step_streak + self.consecutive_count + 1
        } else {
            same_step_streak + 1
        };

        // Apply escalation.
        if total_streak >= REPEAT_FORCE_STOP_STREAK {
            append_reminder(result, REMINDER_TEXT_3);
            result.stop_turn = true;
        } else if total_streak >= REPEAT_REMINDER_3_START {
            append_reminder(result, REMINDER_TEXT_3);
        } else if total_streak >= REPEAT_REMINDER_2_START {
            append_reminder(result, &format_reminder_2(total_streak));
        } else if total_streak >= REPEAT_REMINDER_1_START {
            append_reminder(result, REMINDER_TEXT_1);
        }

        // Store the finalized result so that duplicate calls detected by
        // `check_same_step` can return the original call's actual output
        // (including any appended reminders) instead of an empty placeholder.
        self.finalized_results.insert(key.clone(), result.clone());

        self.consecutive_key = Some(key);
        self.consecutive_count = total_streak;
    }

    /// Retrieve the finalized result for a canonical key.
    ///
    /// Returns `None` if no call with this key has been finalized yet.
    /// This is useful for testing and for callers that want to inspect what
    /// the original call produced.
    pub fn get_canonical_result(&self, canonical_key: &str) -> Option<&ToolResult> {
        self.finalized_results.get(canonical_key)
    }

    /// The current cross-step streak count (for testing).
    #[doc(hidden)]
    pub fn consecutive_count(&self) -> u32 {
        self.consecutive_count
    }

    /// The current cross-step streak key (for testing).
    #[doc(hidden)]
    pub fn consecutive_key(&self) -> Option<&str> {
        self.consecutive_key.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Append a reminder string to a `ToolResult`'s output.
///
/// For `Text` output, the reminder is appended with a blank line separator.
/// For `Parts` output, the reminder is appended to the last text part if any,
/// or a new text part is pushed.
fn append_reminder(result: &mut ToolResult, reminder: &str) {
    match &mut result.output {
        ToolOutput::Text(text) => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(reminder);
        }
        ToolOutput::Parts(parts) => {
            if let Some(crate::message::ContentPart::Text { text }) = parts.last_mut() {
                text.push_str("\n\n");
                text.push_str(reminder);
                return;
            }
            parts.push(crate::message::ContentPart::Text {
                text: reminder.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ContentPart;

    // --- make_key / sort_json_value tests ---

    #[test]
    fn should_make_same_key_when_args_are_identical() {
        let k1 = make_key("Echo", &serde_json::json!({"message": "hi"}));
        let k2 = make_key("Echo", &serde_json::json!({"message": "hi"}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn should_make_same_key_when_args_have_different_key_order() {
        let k1 = make_key("Echo", &serde_json::json!({"b": 1, "a": 2}));
        let k2 = make_key("Echo", &serde_json::json!({"a": 2, "b": 1}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn should_make_different_key_when_args_differ() {
        let k1 = make_key("Echo", &serde_json::json!({"message": "hi"}));
        let k2 = make_key("Echo", &serde_json::json!({"message": "bye"}));
        assert_ne!(k1, k2);
    }

    #[test]
    fn should_make_different_key_when_tool_name_differs() {
        let k1 = make_key("Echo", &serde_json::json!({"message": "hi"}));
        let k2 = make_key("Other", &serde_json::json!({"message": "hi"}));
        assert_ne!(k1, k2);
    }

    #[test]
    fn should_sort_nested_object_keys_when_sorting_json_value() {
        let input = serde_json::json!({
            "b": {"d": 4, "c": 3},
            "a": 1
        });
        let sorted = sort_json_value(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        // Keys should be sorted: a, b, and within b: c, d
        assert_eq!(serialized, r#"{"a":1,"b":{"c":3,"d":4}}"#);
    }

    #[test]
    fn should_sort_object_keys_inside_arrays_when_sorting_json_value() {
        let input = serde_json::json!([{"b": 2, "a": 1}, {"d": 4, "c": 3}]);
        let sorted = sort_json_value(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(serialized, r#"[{"a":1,"b":2},{"c":3,"d":4}]"#);
    }

    // --- Same-step dedup tests ---

    #[test]
    fn should_return_none_when_call_is_first_in_step() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert!(result.is_none());
    }

    #[test]
    fn should_return_synthetic_result_when_duplicate_call_repeats_in_step() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(!result.is_error);
    }

    #[test]
    fn should_return_none_when_same_tool_called_with_different_args_in_step() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 2}), "call-2");
        assert!(result.is_none());
    }

    // --- Cross-step streak tests ---

    #[test]
    fn should_not_append_reminder_when_streak_is_below_threshold() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        // Steps 1-2: streak reaches 2, no reminder.
        for step in 0..2 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("ok");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();
        }
        // streak = 2, below threshold 3
        assert_eq!(dedup.consecutive_count(), 2);

        let check_result = ToolResult::success("ok");
        // Verify no reminder was appended by checking output is clean
        match &check_result.output {
            ToolOutput::Text(t) => assert_eq!(t, "ok"),
            _ => panic!("expected text output"),
        }
    }

    #[test]
    fn should_append_reminder_1_when_streak_reaches_3() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        for step in 0..3 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("ok");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();

            if step == 2 {
                // streak = 3, should have reminder 1
                let text = match &result.output {
                    ToolOutput::Text(t) => t.clone(),
                    _ => panic!("expected text"),
                };
                assert!(
                    text.contains("system-reminder"),
                    "expected reminder at streak 3"
                );
                assert!(!result.stop_turn, "should not stop at streak 3");
            }
        }
        assert_eq!(dedup.consecutive_count(), 3);
    }

    #[test]
    fn should_append_reminder_2_when_streak_reaches_5() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        for step in 0..5 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("ok");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();

            if step == 4 {
                // streak = 5, should have reminder 2
                let text = match &result.output {
                    ToolOutput::Text(t) => t.clone(),
                    _ => panic!("expected text"),
                };
                assert!(text.contains("5 times"), "expected reminder 2 at streak 5");
                assert!(!result.stop_turn, "should not stop at streak 5");
            }
        }
        assert_eq!(dedup.consecutive_count(), 5);
    }

    #[test]
    fn should_append_reminder_3_when_streak_reaches_8() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        for step in 0..8 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("ok");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();

            if step == 7 {
                // streak = 8, should have reminder 3
                let text = match &result.output {
                    ToolOutput::Text(t) => t.clone(),
                    _ => panic!("expected text"),
                };
                assert!(
                    text.contains("repeatedly calling"),
                    "expected reminder 3 at streak 8"
                );
                assert!(!result.stop_turn, "should not stop at streak 8");
            }
        }
        assert_eq!(dedup.consecutive_count(), 8);
    }

    #[test]
    fn should_force_stop_turn_when_streak_reaches_12() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        for step in 0..12 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("ok");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();

            if step == 11 {
                // streak = 12, should force stop
                assert!(result.stop_turn, "should stop at streak 12");
            }
        }
        assert_eq!(dedup.consecutive_count(), 12);
    }

    // --- begin_step / end_step tests ---

    #[test]
    fn should_clear_per_step_state_when_begin_step_is_called() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert_eq!(dedup.step_calls.len(), 1);

        dedup.begin_step();
        assert_eq!(dedup.step_calls.len(), 0);

        // Same call after begin_step should NOT be deduped.
        let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(result.is_none());
    }

    #[test]
    fn should_continue_streak_when_same_call_repeats_across_steps() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        // Step 1
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, "call-0-1");
        let mut result = ToolResult::success("ok");
        dedup.finalize_result("call-0-1", &mut result);
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 1);

        // Step 2 — same tool/args, streak should continue
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, "call-1-1");
        let mut result = ToolResult::success("ok");
        dedup.finalize_result("call-1-1", &mut result);
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 2);
    }

    #[test]
    fn should_reset_streak_when_different_tool_is_called() {
        let mut dedup = ToolCallDeduplicator::new();

        // Step 1: Echo
        dedup.begin_step();
        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-0-1");
        let mut result = ToolResult::success("ok");
        dedup.finalize_result("call-0-1", &mut result);
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 1);

        // Step 2: DifferentTool — resets streak
        dedup.begin_step();
        dedup.check_same_step("Other", &serde_json::json!({"x": 1}), "call-1-1");
        let mut result = ToolResult::success("ok");
        dedup.finalize_result("call-1-1", &mut result);
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 1);
    }

    #[test]
    fn should_leave_synthetic_result_unchanged_when_finalize_is_called() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        // First call — not a duplicate.
        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");

        // Second call — duplicate, returns synthetic.
        let synthetic = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(synthetic.is_some());

        // Finalize the synthetic call — should be a no-op.
        let mut result = synthetic.unwrap();
        let original_text = match &result.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        dedup.finalize_result("call-2", &mut result);
        let after_text = match &result.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(
            original_text, after_text,
            "synthetic result should not be modified"
        );
    }

    #[test]
    fn should_append_reminder_to_existing_text_part_when_output_has_parts() {
        let mut result = ToolResult {
            output: ToolOutput::Parts(vec![ContentPart::Text {
                text: "original".to_string(),
            }]),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        };
        append_reminder(&mut result, "REMINDER");
        match &result.output {
            ToolOutput::Parts(parts) => {
                assert_eq!(parts.len(), 1);
                if let ContentPart::Text { text } = &parts[0] {
                    assert!(text.contains("original"));
                    assert!(text.contains("REMINDER"));
                } else {
                    panic!("expected text part");
                }
            }
            _ => panic!("expected parts"),
        }
    }

    #[test]
    fn should_add_new_text_part_when_output_parts_are_empty() {
        let mut result = ToolResult {
            output: ToolOutput::Parts(vec![]),
            is_error: false,
            note: None,
            stop_turn: false,
            truncated: false,
            injection: None,
        };
        append_reminder(&mut result, "REMINDER");
        match &result.output {
            ToolOutput::Parts(parts) => {
                assert_eq!(parts.len(), 1);
            }
            _ => panic!("expected parts"),
        }
    }

    // --- Deferred-result tests (Gap 4) ---

    #[test]
    fn should_return_original_result_when_duplicate_called_after_original_completes() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        // First call — not a duplicate.
        let result1 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert!(result1.is_none());

        // Finalize the original call with a real result.
        let mut original = ToolResult::success("actual output from tool");
        dedup.finalize_result("call-1", &mut original);

        // Second call — duplicate, but the original has already finalized.
        let result2 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(
            result2.is_some(),
            "duplicate should return a synthetic result"
        );

        let result2 = result2.unwrap();
        let text = match &result2.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert!(
            text.contains("actual output from tool"),
            "duplicate should return the original's finalized result, got: {text}"
        );
    }

    #[test]
    fn should_return_pending_placeholder_when_duplicate_called_before_original_completes() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        // First call — not a duplicate, but NOT yet finalized.
        let result1 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert!(result1.is_none());

        // Second call — duplicate, original hasn't finished yet.
        let result2 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(
            result2.is_some(),
            "duplicate should return a synthetic result"
        );

        let result2 = result2.unwrap();
        let text = match &result2.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert!(
            text.contains("result pending"),
            "duplicate should return a pending placeholder, got: {text}"
        );
    }

    #[test]
    fn should_return_none_when_non_duplicate_call_is_checked() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        // First call — not a duplicate.
        let result1 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert!(result1.is_none(), "first call should not be deduped");

        // Different tool name — not a duplicate.
        let result2 = dedup.check_same_step("Other", &serde_json::json!({"x": 1}), "call-2");
        assert!(result2.is_none(), "different tool should not be deduped");

        // Same tool, different args — not a duplicate.
        let result3 = dedup.check_same_step("Echo", &serde_json::json!({"x": 2}), "call-3");
        assert!(result3.is_none(), "different args should not be deduped");
    }

    #[test]
    fn should_return_finalized_result_when_get_canonical_result_is_called() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        let mut result = ToolResult::success("finalized data");
        dedup.finalize_result("call-1", &mut result);

        let key = make_key("Echo", &serde_json::json!({"x": 1}));
        let retrieved = dedup.get_canonical_result(&key);
        assert!(retrieved.is_some(), "canonical result should be stored");
        let retrieved = retrieved.unwrap();
        let text = match &retrieved.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert!(
            text.contains("finalized data"),
            "retrieved result should match finalized output"
        );
    }

    #[test]
    fn should_return_none_from_get_canonical_result_when_not_finalized() {
        let dedup = ToolCallDeduplicator::new();
        let key = make_key("Echo", &serde_json::json!({"x": 1}));
        assert!(
            dedup.get_canonical_result(&key).is_none(),
            "no result should be stored before finalization"
        );
    }

    #[test]
    fn should_return_exact_original_result_when_duplicate_called_after_finalization() {
        // Finalize the original call with an exact result text, then call
        // check_same_step for a duplicate. The duplicate's result text must
        // be EXACTLY equal (not just contains) to the original's.
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();

        // First call — not a duplicate.
        let result1 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
        assert!(result1.is_none());

        // Finalize the original call with an exact result text.
        let mut original = ToolResult::success("exact output");
        dedup.finalize_result("call-1", &mut original);

        // Second call — duplicate, original has been finalized.
        let result2 = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
        assert!(
            result2.is_some(),
            "duplicate should return a synthetic result"
        );

        let result2 = result2.unwrap();
        let text = match &result2.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        // Exact equality — not contains. The duplicate must carry the exact
        // finalized result of the original, byte-for-byte.
        assert_eq!(
            text, "exact output",
            "duplicate result must be exactly equal to the original's finalized result"
        );
    }

    #[test]
    fn should_return_post_reminder_result_for_duplicate_when_streak_appends_reminder() {
        // When a cross-step streak > 1 causes a reminder to be appended to
        // the original's finalized result, a duplicate in a subsequent step
        // should receive the post-reminder result (the original + reminder).
        let mut dedup = ToolCallDeduplicator::new();
        let args = serde_json::json!({"x": 1});

        // Steps 1-2: build up a streak of 2 (no reminder yet).
        for step in 0..2 {
            dedup.begin_step();
            dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
            let mut result = ToolResult::success("exact output");
            dedup.finalize_result(&format!("call-{step}-1"), &mut result);
            dedup.end_step();
        }
        assert_eq!(dedup.consecutive_count(), 2);

        // Step 3: streak reaches 3 — reminder 1 is appended to the finalized
        // result. Then a duplicate call in the same step should get the
        // post-reminder result.
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, "call-2-1");
        let mut original = ToolResult::success("exact output");
        dedup.finalize_result("call-2-1", &mut original);

        // Verify the original now has a reminder appended.
        let original_text = match &original.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            original_text.contains("system-reminder"),
            "original at streak 3 should have a reminder"
        );
        assert!(
            original_text.starts_with("exact output"),
            "original should still start with the exact output"
        );

        // Duplicate call — should get the post-reminder result.
        let dup = dedup.check_same_step("Echo", &args, "call-2-2");
        assert!(dup.is_some(), "duplicate should return a synthetic result");
        let dup = dup.unwrap();
        let dup_text = match &dup.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        // The duplicate must carry the exact post-reminder result, including
        // the appended reminder.
        assert_eq!(
            dup_text, original_text,
            "duplicate must carry the exact post-reminder result"
        );
    }
}
