//! Fork pipeline truncation core (F1).
//!
//! A session fork rewinds the wire log to a chosen completed turn and
//! replays the kept prefix into a fresh session. This module holds the
//! two pure, zero-IO building blocks of that pipeline (design 12 §3.2 /
//! §3.3, ADR-F1):
//!
//! - [`truncate_ops_at_turn`]: cut the op stream right after the target
//!   turn's `turn.ended` record, dropping everything that follows —
//!   stray cross-turn records and any trailing open (crashed / still
//!   active) turn bracket alike.
//! - [`filter_forked_ops`]: apply the fork filter table to the truncated
//!   stream so the forked session does not inherit source-only state:
//!   cron registrations (`cron.add` / `cron.delete` / `cron.cursor`) and
//!   historical `forked` markers are dropped, and `goal.update` records
//!   are stripped of their lifecycle fields (`status`, `turns_used`,
//!   `tokens_used`, `wall_clock_ms`) so goals replay back to their
//!   creation-time initial state. Everything else — including the
//!   `metadata` header and unknown op types — passes through untouched.

use crate::session::{SessionError, SessionResult};
use mirri_proto::WireOp;

/// The tail of an op stream: how many turns closed, and whether one is
/// still open (a `turn.prompt` with no matching `turn.ended` after it).
///
/// This is the error-message enrichment core (F4, ADR-F4 rev2): the wire
/// log's `turn.ended` records are the ONLY identity a caller's `turnId`
/// can address (the embedded in-flight registry tracks client-facing
/// `turn-<uuid>` strings — a different namespace, so "turnId == the
/// active turn" is not expressible; see `SessionError::ForkTargetNotFound`
/// for the documented semantics). The open-tail fact therefore rides the
/// error MESSAGE, distinguishing "2 completed turns exist and a third is
/// in progress" from a plain out-of-range typo.
pub(crate) struct TurnTail {
    /// Number of closed turns (`turn.ended` records) in the stream.
    pub completed_turns: usize,
    /// `true` when a `turn.prompt` follows the last `turn.ended` (an open
    /// bracket — an active or crashed turn whose content never carries).
    pub has_open_turn: bool,
}

/// Count the tail shape of an op stream for the fork error message.
pub(crate) fn count_turn_tail(ops: &[WireOp]) -> TurnTail {
    let mut completed_turns = 0usize;
    let mut has_open_turn = false;
    for op in ops {
        match op {
            WireOp::TurnEnded { .. } => {
                completed_turns += 1;
                has_open_turn = false;
            }
            WireOp::TurnPrompt { .. } => has_open_turn = true,
            _ => {}
        }
    }
    TurnTail {
        completed_turns,
        has_open_turn,
    }
}

/// Truncate the op stream at the end of the turn identified by
/// `target_turn` (a `TurnEnded.turn_id`).
///
/// The cut is inclusive of the target's `turn.ended` record and hard:
/// anything after it — stray records injected after the bracket closed,
/// or a trailing still-open turn — is cut away, so the returned stream's
/// last record is always the target's `turn.ended`. Returns
/// [`SessionError::ForkTargetNotFound`] when no `turn.ended` matches,
/// with the log's available `turn_id` range in the message. Pure: no IO,
/// no mutation of the input.
// F1 lands the truncation core ahead of its callers (F2+ wire the fork
// RPC into SessionManager); the pipeline is exercised by unit tests now.
#[allow(dead_code)]
pub(crate) fn truncate_ops_at_turn(ops: &[WireOp], target_turn: u64) -> SessionResult<Vec<WireOp>> {
    let mut cut_index: Option<usize> = None;
    // Available turn ids seen so far (for the error message's range).
    let mut min_turn: Option<u64> = None;
    let mut max_turn: Option<u64> = None;

    for (index, op) in ops.iter().enumerate() {
        if let WireOp::TurnEnded { turn_id, .. } = op {
            min_turn = Some(min_turn.map_or(*turn_id, |min: u64| min.min(*turn_id)));
            max_turn = Some(max_turn.map_or(*turn_id, |max: u64| max.max(*turn_id)));
            if *turn_id == target_turn && cut_index.is_none() {
                cut_index = Some(index);
            }
        }
    }

    let Some(cut_index) = cut_index else {
        // ADR-F4 rev2 guard (F4): the `turn.ended` ids are the only
        // addressable anchor namespace — an ACTIVE turn has no
        // `turn.ended`, hence no id a caller can name, so the only
        // reachable error is "target not found". The message enriches
        // the miss with the tail shape: the completed-turn range plus,
        // when the log ends with an open bracket, the fact that a turn
        // is still in progress (its content never carries into any
        // fork). The embedded layer maps this to invalid_params.
        let tail = count_turn_tail(ops);
        let mut message = match (min_turn, max_turn) {
            (Some(min), Some(max)) => {
                format!(
                    "turn {target_turn} has no turn.ended record; available turn ids: {min}..={max}"
                )
            }
            _ => format!(
                "turn {target_turn} has no turn.ended record; no completed turns are available"
            ),
        };
        if tail.has_open_turn {
            message.push_str(&format!(
                "; a further turn is in progress ({} completed turn{} so far) and \
                 carries into no fork — fork at one of the completed turns above \
                 or omit turnId to anchor at the latest completed turn",
                tail.completed_turns,
                if tail.completed_turns == 1 { "" } else { "s" },
            ));
        }
        return Err(SessionError::ForkTargetNotFound(message));
    };

    Ok(ops[..=cut_index].to_vec())
}

/// Apply the fork filter table to a (typically truncated) op stream.
///
/// Drop list (ADR-F1): the three cron variants and `forked` markers.
/// Field-level rewrite: `goal.update` lifecycle fields (`status`,
/// `turns_used`, `tokens_used`, `wall_clock_ms`) are forced to `None` so
/// the goal returns to its create-time initial state on replay. All
/// other ops — the `metadata` header included — pass through unchanged.
// F1 lands the filter core ahead of its callers (F2+ wire the fork RPC
// into SessionManager); the pipeline is exercised by unit tests now.
#[allow(dead_code)]
pub(crate) fn filter_forked_ops(ops: Vec<WireOp>) -> Vec<WireOp> {
    ops.into_iter().filter_map(rewrite_for_fork).collect()
}

/// One op's fork decision: `None` drops it, `Some(rewritten)` keeps it.
#[allow(dead_code)]
fn rewrite_for_fork(op: WireOp) -> Option<WireOp> {
    match op {
        // ADR-F1: cron state is source-session-local; the forked session
        // starts with no cron registrations or cursors.
        WireOp::CronAdd { .. } | WireOp::CronDelete { .. } | WireOp::CronCursor { .. } => None,
        // Historical fork markers describe the *source* log's checkpoint
        // boundaries — never a property of the freshly forked log.
        WireOp::Forked { .. } => None,
        // Strip the goal's post-creation lifecycle so replay restores the
        // goal to its initial (un-started) state; the restore side owns
        // any default re-activation.
        WireOp::GoalUpdate {
            goal_id,
            status: _,
            reason,
            turns_used: _,
            tokens_used: _,
            wall_clock_ms: _,
            budget_limits,
            actor,
        } => Some(WireOp::GoalUpdate {
            goal_id,
            status: None,
            reason,
            turns_used: None,
            tokens_used: None,
            wall_clock_ms: None,
            budget_limits,
            actor,
        }),
        // Everything else — metadata header, context.*, turn.*, usage,
        // permission, plan, config, compaction, unknown ops — replays
        // verbatim.
        other => Some(other),
    }
}

/// Merge the fork's custom metadata dicts (F3): the source session's
/// `state.json` `custom` is the base, the caller-supplied dict overrides,
/// and the `goal` key is dropped from BOTH sides — a fork's goal lifecycle
/// restarts fresh (the same rule [`filter_forked_ops`] applies to the
/// wire side's `goal.update`). An all-dropped merge yields `None`.
pub(crate) fn merge_fork_custom(
    source: Option<&serde_json::Map<String, serde_json::Value>>,
    fork: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    fn without_goal(
        map: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> serde_json::Map<String, serde_json::Value> {
        match map {
            Some(map) => serde_json::Map::from_iter(
                map.iter()
                    .filter(|(key, _)| key.as_str() != "goal")
                    .map(|(key, value)| (key.clone(), value.clone())),
            ),
            None => serde_json::Map::new(),
        }
    }
    let mut merged = without_goal(source);
    let fork = without_goal(fork);
    let had_keys = !merged.is_empty() || !fork.is_empty();
    merged.extend(fork);
    had_keys.then_some(merged)
}

/// Resolve the fork chain's cache root id (the provider prompt-cache key
/// lineage): the source's persisted root wins when it has one (a nested
/// fork must keep hitting the cache shard the chain root warmed), else
/// the source itself is the root of the new chain — its own id.
///
/// Pure and lossless: no IO. `None` for the source's root (a fresh or
/// legacy source predating the field) falls back to the source id — that
/// fallback is the "source is its own root" semantics. Whether to call
/// this at all when the source's state cannot be READ is the caller's
/// decision (the fork machinery leaves the cache root unset in that case
/// instead of minting a possibly-wrong id).
pub(crate) fn resolve_fork_cache_root(source_cache_root: Option<&str>, source_id: &str) -> String {
    source_cache_root
        .filter(|root| !root.is_empty())
        .unwrap_or(source_id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use crate::session::fork_pipeline::{count_turn_tail, filter_forked_ops, truncate_ops_at_turn};
    use mirri_proto::WireOp;
    use serde_json::json;

    fn turn_prompt(input: &str) -> WireOp {
        WireOp::TurnPrompt {
            input: json!(input),
            origin: json!({ "kind": "user" }),
        }
    }

    fn user_append(text: &str) -> WireOp {
        WireOp::ContextAppendMessage {
            message: json!({ "role": "user", "content": text, "origin": { "kind": "user" } }),
        }
    }

    fn turn_ended(turn_id: u64) -> WireOp {
        WireOp::TurnEnded {
            turn_id,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
        }
    }

    fn metadata() -> WireOp {
        WireOp::Metadata {
            protocol_version: Some("1".to_string()),
            created_at: Some(0),
        }
    }

    fn goal_update(status: Option<&str>) -> WireOp {
        WireOp::GoalUpdate {
            goal_id: Some("g1".to_string()),
            status: status.map(str::to_string),
            reason: None,
            turns_used: Some(3),
            tokens_used: Some(100),
            wall_clock_ms: Some(5000),
            budget_limits: None,
            actor: None,
        }
    }

    #[test]
    fn should_keep_exactly_the_first_two_turns_when_truncating_a_three_turn_log_at_turn_two() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            user_append("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            user_append("t2"),
            WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: None,
            },
            turn_ended(2),
            turn_prompt("t3"),
            user_append("t3"),
            turn_ended(3),
        ];
        let truncated = truncate_ops_at_turn(&ops, 2).expect("turn 2 exists");
        assert_eq!(
            truncated.len(),
            8,
            "metadata + both full turns: {truncated:?}"
        );
        assert!(matches!(
            truncated.last(),
            Some(WireOp::TurnEnded { turn_id: 2, .. })
        ));
        assert!(
            !truncated
                .iter()
                .any(|op| matches!(op, WireOp::TurnPrompt { input, .. } if *input == json!("t3"))),
            "third turn must not survive"
        );
    }

    #[test]
    fn should_drop_cron_variants_when_filtering_a_truncated_stream() {
        let ops = vec![
            turn_prompt("t1"),
            WireOp::CronAdd {
                task: json!({ "id": "c1" }),
            },
            WireOp::CronDelete {
                ids: vec!["c1".to_string()],
            },
            WireOp::CronCursor {
                id: "c1".to_string(),
                last_fired_at: 123,
            },
            user_append("t1"),
            turn_ended(1),
        ];
        let filtered = filter_forked_ops(ops);
        assert_eq!(
            filtered.len(),
            3,
            "only prompt/append/ended survive: {filtered:?}"
        );
        assert!(
            !filtered.iter().any(|op| {
                matches!(
                    op,
                    WireOp::CronAdd { .. } | WireOp::CronDelete { .. } | WireOp::CronCursor { .. }
                )
            }),
            "all three cron variants must be dropped"
        );
    }

    #[test]
    fn should_drop_forked_markers_when_filtering_a_truncated_stream() {
        let ops = vec![
            turn_prompt("t1"),
            WireOp::Forked {},
            user_append("t1"),
            turn_ended(1),
        ];
        let filtered = filter_forked_ops(ops);
        assert_eq!(filtered.len(), 3, "Forked must be dropped: {filtered:?}");
        assert!(
            !filtered
                .iter()
                .any(|op| matches!(op, WireOp::Forked { .. }))
        );
    }

    #[test]
    fn should_strip_goal_update_lifecycle_fields_when_filtering_a_truncated_stream() {
        let ops = vec![
            turn_prompt("t1"),
            goal_update(Some("active")),
            turn_ended(1),
        ];
        let filtered = filter_forked_ops(ops);
        assert_eq!(filtered.len(), 3);
        let WireOp::GoalUpdate {
            ref status,
            ref turns_used,
            ref tokens_used,
            ref wall_clock_ms,
            ..
        } = filtered[1]
        else {
            panic!("goal.update must survive filtering: {:?}", filtered[1]);
        };
        assert_eq!(status, &None, "explicit status must be stripped");
        assert_eq!(turns_used, &None, "turns_used must be stripped");
        assert_eq!(tokens_used, &None, "tokens_used must be stripped");
        assert_eq!(wall_clock_ms, &None, "wall_clock_ms must be stripped");
    }

    #[test]
    fn should_keep_goal_create_and_goal_clear_untouched_when_filtering() {
        let ops = vec![
            turn_prompt("t1"),
            WireOp::GoalCreate {
                goal_id: "g1".to_string(),
                objective: "do it".to_string(),
                completion_criterion: None,
                status: None,
                actor: None,
                budget_limits: None,
            },
            WireOp::GoalClear {},
            turn_ended(1),
        ];
        let filtered = filter_forked_ops(ops);
        assert_eq!(filtered.len(), 4);
        assert!(matches!(filtered[1], WireOp::GoalCreate { .. }));
        assert!(matches!(filtered[2], WireOp::GoalClear { .. }));
    }

    #[test]
    fn should_keep_other_ops_untouched_when_filtering_a_truncated_stream() {
        let ops = vec![
            turn_prompt("t1"),
            WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: Some("session".to_string()),
            },
            user_append("t1"),
            WireOp::Unknown,
            turn_ended(1),
        ];
        let filtered = filter_forked_ops(ops);
        assert_eq!(
            filtered.len(),
            5,
            "every op outside the filter table survives: {filtered:?}"
        );
        assert!(matches!(filtered[1], WireOp::UsageRecord { .. }));
        assert!(matches!(filtered[2], WireOp::ContextAppendMessage { .. }));
        assert!(matches!(filtered[3], WireOp::Unknown));
    }

    #[test]
    fn should_return_an_error_listing_the_available_range_when_the_target_turn_is_missing() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            turn_ended(2),
        ];
        let err = truncate_ops_at_turn(&ops, 9).expect_err("turn 9 does not exist");
        let msg = err.to_string();
        assert!(
            msg.contains('9'),
            "message should mention the target: {msg}"
        );
        assert!(
            msg.contains("1"),
            "message should mention the available min: {msg}"
        );
        assert!(
            msg.contains("2"),
            "message should mention the available max: {msg}"
        );
    }

    #[test]
    fn should_truncate_at_the_last_turn_ended_when_the_log_tail_has_an_open_bracket() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            user_append("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            user_append("t2"),
            // active turn: prompt but no turn.ended
        ];
        let truncated = truncate_ops_at_turn(&ops, 1).expect("turn 1 exists");
        assert_eq!(
            truncated.len(),
            4,
            "open bracket is cut away: {truncated:?}"
        );
        assert!(matches!(
            truncated.last(),
            Some(WireOp::TurnEnded { turn_id: 1, .. })
        ));
    }

    #[test]
    fn should_keep_the_metadata_header_in_first_position_when_present() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            user_append("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            turn_ended(2),
        ];
        let truncated = truncate_ops_at_turn(&ops, 2).expect("turn 2 exists");
        assert!(matches!(truncated.first(), Some(WireOp::Metadata { .. })));
        let filtered = filter_forked_ops(truncated);
        assert!(matches!(filtered.first(), Some(WireOp::Metadata { .. })));
    }

    #[test]
    fn should_cut_stray_records_after_the_target_turn_ended_when_truncating() {
        let ops = vec![
            turn_prompt("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            turn_ended(2),
            // stray records after the target bracket (cross-turn injection)
            WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: None,
            },
            user_append("stray"),
        ];
        let truncated = truncate_ops_at_turn(&ops, 2).expect("turn 2 exists");
        assert!(matches!(
            truncated.last(),
            Some(WireOp::TurnEnded { turn_id: 2, .. })
        ));
        assert_eq!(
            truncated.len(),
            4,
            "stray tail records are cut: {truncated:?}"
        );
    }

    // ---------------------------------------------------------------------
    // F4 — anchor-validation guard: the error-message enrichment. The
    // turn.ended records are the only addressable anchor namespace; the
    // open-tail fact (an active turn) rides the message.
    // ---------------------------------------------------------------------

    /// Given a log with two closed turns and an open third bracket, When
    /// the tail shape is counted, Then it reports 2 completed turns and an
    /// open turn.
    #[test]
    fn should_count_two_completed_turns_and_an_open_turn_when_the_tail_bracket_is_unclosed() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            turn_ended(2),
            turn_prompt("t3"), // active: no turn.ended follows
        ];
        let tail = count_turn_tail(&ops);
        assert_eq!(tail.completed_turns, 2);
        assert!(tail.has_open_turn, "the third open bracket must be seen");
    }

    /// Given a log whose every bracket is closed, When the tail shape is
    /// counted, Then it reports no open turn.
    #[test]
    fn should_report_no_open_turn_when_every_bracket_is_closed() {
        let ops = vec![
            metadata(),
            turn_prompt("t1"),
            turn_ended(1),
            turn_prompt("t2"),
            turn_ended(2),
        ];
        let tail = count_turn_tail(&ops);
        assert_eq!(tail.completed_turns, 2);
        assert!(!tail.has_open_turn);
    }
}
