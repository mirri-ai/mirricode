//! Per-turn token-usage fold over a session's wire log.
//!
//! The live read face (`event.turn.step.completed`) streams each step's
//! four usage buckets and the web projector accumulates them per turn —
//! but that state lives only in the browser. The same buckets are
//! already durable on disk: every step end is recorded as a
//! `context.append_loop_event` (`step.end`) op inside its
//! `turn.prompt` .. `turn.ended` bracket. This module re-derives the
//! per-turn aggregate from the raw op stream so a rerender (page reload,
//! session reopen, resync) can restore the per-turn usage pill.
//!
//! Deliberately read-only: nothing here mutates the wire log, the
//! reducer state, or the live agent. Live and restored sessions fold the
//! same log through the same code, so the two read faces cannot drift
//! (guarded by `cross_backend_contract`).

use mirri_proto::WireOp;
use serde_json::Value;

/// One finalized turn's token usage — the fold of every `step.end` loop
/// event inside a `turn.prompt` .. `turn.ended` bracket.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnUsageRecord {
    /// Stable client-facing key. For a COMPLETED bracket (closed by
    /// `turn.ended`) this is `"turn-<id>"` with `<id>` the `turn.ended`
    /// wire record's numeric turn id — the fork-at-turn anchor and the
    /// numbering `session.fork`'s `turnId` validates against. For a
    /// dangling bracket (the turn crashed or was aborted without a
    /// `turn.ended`) this is `"aborted-<n>"` with `<n>` the bracket's
    /// 1-based ordinal of the `turn.prompt` op inside this wire log — a
    /// namespace that can never be mistaken for a fork anchor. The live
    /// event stream's `turn-<uuid>` keys occupy a different namespace and
    /// never collide.
    pub turn_key: String,
    /// The bracket's 1-based `turn.prompt` ordinal inside this wire log —
    /// the record's IDENTITY. `turn_key` alone is ambiguous on legacy logs
    /// whose allocator restarted (the same `turn-<n>` minted by several
    /// process runs); the ordinal is unique per bracket, so downstream
    /// attribution (see [`assign_turn_durations`]) keys on it instead of
    /// the string. Matches [`TurnOpener::bracket`] of the same turn.
    pub bracket: u64,
    /// The `turn.ended` wire record's numeric turn id — the fork-at-turn
    /// anchor. `None` for dangling brackets (no `turn.ended` record).
    pub turn_id: Option<u64>,
    /// The `turn.ended` wire record's server-measured duration in
    /// milliseconds — stamped on the turn's last assistant history row so
    /// a restored transcript renders the same "Worked for X" pill as the
    /// live one. `None` for dangling brackets (no `turn.ended` record) and
    /// for records that omitted the field.
    pub duration_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Deduplicated model aliases recorded by `usage.record` ops inside
    /// the bracket, in first-seen order.
    pub models: Vec<String>,
}

/// The opening user message of one turn bracket, as a matchable
/// signature — the anchor [`super::turn_usage`] consumers match against
/// history rows to attribute assistant rows to their turn.
#[derive(Debug, Clone)]
pub struct TurnOpener {
    pub turn_key: String,
    /// The bracket's 1-based `turn.prompt` ordinal — the opener's IDENTITY,
    /// unique per bracket even when `turn_key` collides across allocator
    /// restarts (see [`TurnUsageRecord::bracket`]).
    pub bracket: u64,
    /// The raw `origin` JSON of the opener's `context.append_message`.
    pub origin: Value,
    /// The normalized content signature (see [`content_signature`]).
    pub content_sig: Value,
}

/// The result of folding a session's wire ops.
#[derive(Debug, Clone, Default)]
pub struct TurnUsageFold {
    /// Finalized turns that consumed tokens, chronological.
    pub turns: Vec<TurnUsageRecord>,
    /// The opener anchor of every turn (usage or not), chronological.
    pub openers: Vec<TurnOpener>,
    /// The turn key of the bracket still open at the end of the op stream —
    /// the in-flight turn. `session.snapshot` surfaces it as
    /// `current_prompt_id` so the client's in-flight seed and the transcript
    /// rows (attributed through the tail opener) share one turn identity;
    /// without it the two copies of the running step split into separate
    /// groups and the running tool card renders twice after a mid-turn
    /// reload.
    pub open_turn_key: Option<String>,
}

/// One in-progress bracket while folding.
struct OpenTurn {
    record: TurnUsageRecord,
    opener: Option<TurnOpener>,
}

/// The per-row turn attribution of one history against one fold — the
/// output of [`assign_turn_rows`].
///
/// Carries two maps over the same assistant rows: the client-facing turn
/// key (what `promptId` stamps — the web's grouping anchor, whose string
/// shape is unchanged) and the fold bracket ordinal (the turn's IDENTITY,
/// unique even on legacy logs whose allocator restarted and minted
/// colliding keys). [`assign_turn_durations`] keys on the bracket.
#[derive(Debug, Clone, Default)]
pub struct TurnRowAssignment {
    /// Assistant row entry_seq → client-facing turn key (`"turn-<n>"` /
    /// `"aborted-<n>"`). Colliding keys across allocator restarts are
    /// preserved verbatim — grouping is the consumer's concern.
    pub prompt_ids: std::collections::HashMap<u64, String>,
    /// Assistant row entry_seq → the fold bracket ordinal the row was
    /// attributed to (see [`TurnUsageRecord::bracket`]).
    pub row_brackets: std::collections::HashMap<u64, u64>,
}

/// Fold a session's wire ops into per-turn usage records plus opener
/// anchors. Pure — call with the full migrated op stream of a wire log.
pub fn fold_turn_usages(ops: &[WireOp]) -> TurnUsageFold {
    let mut fold = TurnUsageFold::default();
    let mut open: Option<OpenTurn> = None;
    let mut ordinal: u64 = 0;

    for op in ops {
        match op {
            WireOp::TurnPrompt { .. } => {
                // A crashed turn never wrote `turn.ended` — finalize the
                // dangling bracket as-is before the next one opens, re-keyed
                // into the `aborted-` namespace so it can never be mistaken
                // for a fork anchor (a legacy log whose allocator restarted
                // can mint a dangling ordinal that equals a real
                // `turn.ended` id of a different turn).
                if let Some(mut bracket) = open.take() {
                    rekey_bracket(&mut bracket, || format!("aborted-{ordinal}"));
                    finalize_bracket(&mut fold, bracket);
                }
                ordinal += 1;
                open = Some(OpenTurn {
                    record: TurnUsageRecord {
                        turn_key: format!("turn-{ordinal}"),
                        bracket: ordinal,
                        ..TurnUsageRecord::default()
                    },
                    opener: None,
                });
            }
            WireOp::ContextAppendMessage { message } => {
                if let Some(bracket) = open.as_mut()
                    && bracket.opener.is_none()
                    && message.get("role").and_then(Value::as_str) == Some("user")
                {
                    bracket.opener = Some(TurnOpener {
                        turn_key: bracket.record.turn_key.clone(),
                        bracket: bracket.record.bracket,
                        origin: message.get("origin").cloned().unwrap_or(Value::Null),
                        content_sig: content_signature(message.get("content")),
                    });
                }
            }
            WireOp::ContextAppendLoopEvent { event } => {
                if event.get("type").and_then(Value::as_str) != Some("step.end") {
                    continue;
                }
                let Some(bracket) = open.as_mut() else {
                    continue;
                };
                let bucket = |key: &str| {
                    event
                        .get("usage")
                        .and_then(|usage| usage.get(key))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                bracket.record.input_tokens += bucket("inputTokens");
                bracket.record.output_tokens += bucket("outputTokens");
                bracket.record.cache_read_tokens += bucket("cacheReadTokens");
                bracket.record.cache_creation_tokens += bucket("cacheCreationTokens");
            }
            WireOp::UsageRecord { model, .. } => {
                if let Some(bracket) = open.as_mut()
                    && !bracket.record.models.iter().any(|known| known == model)
                {
                    bracket.record.models.push(model.clone());
                }
            }
            WireOp::TurnEnded {
                turn_id,
                duration_ms,
                ..
            } => {
                if let Some(mut bracket) = open.take() {
                    // Re-key the completed bracket to the wire turn id: the
                    // client-facing anchor must be the id `session.fork`'s
                    // turnId validates against, not the fold-local ordinal
                    // (legacy logs whose turn allocator restarted — the
                    // close→resume re-seed bug — have ordinals that diverge
                    // from `turn.ended` ids).
                    rekey_bracket(&mut bracket, || format!("turn-{turn_id}"));
                    bracket.record.turn_id = Some(*turn_id);
                    bracket.record.duration_ms = *duration_ms;
                    finalize_bracket(&mut fold, bracket);
                }
            }
            _ => {}
        }
    }
    // The tail bracket is the in-flight turn. No usage record — the turn has
    // not finished, so the pill stays hidden exactly as before — but its
    // opener anchors attribution (otherwise the turn's assistant rows would
    // inherit the previous turn's key, since its user row matches nothing in
    // the queue) and the key itself feeds the snapshot's
    // `current_prompt_id`.
    if let Some(bracket) = open {
        fold.open_turn_key = Some(bracket.record.turn_key.clone());
        if let Some(opener) = bracket.opener {
            fold.openers.push(opener);
        }
    }
    fold
}

/// The highest turn id a wire log has already minted — the floor a live
/// session's turn-id allocator must be seeded past (the next allocation is
/// at least `result + 1`).
///
/// Counts EVERY record that occupies the log's turn-number namespaces, not
/// just `turn.ended`: a dangling bracket's `aborted-<n>` key (and the
/// still-open tail's `turn-<n>` key) is the bracket's 1-based
/// `turn.prompt` ordinal, so a log whose last 21 turns never closed has a
/// higher live floor (29 in that shape) than its ended maximum (8) —
/// seeding from `turn.ended` alone would re-mint an id the aborted
/// namespace already occupies. `turn.cancel`'s optional `turnId` references
/// an allocated id too and is counted when present. `0` for a log with no
/// turn records.
pub fn wire_log_max_turn_id(ops: &[WireOp]) -> u64 {
    let mut max = 0u64;
    let mut prompt_ordinals = 0u64;
    for op in ops {
        match op {
            WireOp::TurnPrompt { .. } => prompt_ordinals += 1,
            WireOp::TurnEnded { turn_id, .. } => max = max.max(*turn_id),
            WireOp::TurnCancel {
                turn_id: Some(turn_id),
                ..
            } => max = max.max(*turn_id),
            _ => {}
        }
    }
    max.max(prompt_ordinals)
}

/// The turn ids a wire log minted more than once — the allocator-restart
/// signature (every restarted counter re-minted its low ids). Returns the
/// duplicated ids in ascending order, deduplicated.
pub fn duplicate_turn_ids(ops: &[WireOp]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::new();
    let mut duplicated = std::collections::BTreeSet::new();
    for op in ops {
        if let WireOp::TurnEnded { turn_id, .. } = op
            && !seen.insert(*turn_id)
        {
            duplicated.insert(*turn_id);
        }
    }
    duplicated.into_iter().collect()
}

/// Re-key a closing bracket (record + opener share the key): the record's
/// final key must be stamped on assistant rows, and the opener anchors the
/// same rows by signature match — the two must never disagree.
fn rekey_bracket(bracket: &mut OpenTurn, key: impl FnOnce() -> String) {
    let key = key();
    bracket.record.turn_key = key.clone();
    if let Some(opener) = bracket.opener.as_mut() {
        opener.turn_key = key;
    }
}

/// Push a closed bracket's deliverables: the opener anchor always, the
/// usage record only when the turn consumed tokens (a turn that never
/// reached an LLM step produces no pill live either).
fn finalize_bracket(fold: &mut TurnUsageFold, bracket: OpenTurn) {
    let OpenTurn { record, opener } = bracket;
    if let Some(opener) = opener {
        fold.openers.push(opener);
    }
    let total = record.input_tokens
        + record.output_tokens
        + record.cache_read_tokens
        + record.cache_creation_tokens;
    if total > 0 {
        fold.turns.push(record);
    }
}

/// Normalize a raw wire `content` value for signature comparison: a
/// string stays itself; an array of parts keeps each part's `type` and
/// (for text parts) the verbatim text, dropping binary/url payloads —
/// media originals are blob-archive-dependent (offloaded at ingest) and
/// must not break opener matching. An array holding exactly one text
/// part collapses to the bare string, so the two wire spellings of the
/// same plain-text message compare equal.
pub fn content_signature(content: Option<&Value>) -> Value {
    let sig = match content {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .map(|part| match part {
                    Value::Object(object) => {
                        let mut sig = serde_json::Map::new();
                        sig.insert(
                            "type".to_string(),
                            object.get("type").cloned().unwrap_or(Value::Null),
                        );
                        if let Some(Value::String(text)) = object.get("text") {
                            sig.insert("text".to_string(), Value::String(text.clone()));
                        }
                        Value::Object(sig)
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        Some(other) => other.clone(),
    };
    // A single text part is the same message as a bare string — collapse
    // so the fold's string content and the history row's parts array
    // produce identical signatures.
    if let Value::Array(parts) = &sig
        && parts.len() == 1
        && let Some(Value::Object(object)) = parts.first()
        && object.get("type").and_then(Value::as_str) == Some("text")
        && let Some(text) = object.get("text")
    {
        return text.clone();
    }
    sig
}

/// Attribute assistant history rows to their turn: the fold's opener
/// anchors keyed by bracket identity.
///
/// Walks the history in order with a queue of the fold's opener anchors
/// (each turn's opening user message, chronological). A user row whose
/// `(origin, content)` signature matches an anchor makes that turn
/// "current"; every assistant row after it carries the turn's key until
/// the next matched anchor. Anchors queued before a match are dropped —
/// their rows were compacted away (compaction removes whole middle turns,
/// so the retained tail still lines up). Non-matching user rows (steers,
/// injections, compaction markers) are skipped.
///
/// The returned [`TurnRowAssignment`] carries, per row, BOTH the
/// client-facing turn key (the web's grouping anchor — unchanged shape,
/// so a legacy log with colliding keys still groups each batch's rows
/// under the same key) and the fold bracket ordinal (the turn's IDENTITY —
/// see [`TurnUsageRecord::bracket`]). Duration attribution
/// ([`assign_turn_durations`]) keys on the ordinal; keying on the string
/// would let a later batch's rows steal an earlier batch's duration slot
/// on collision-ridden legacy logs.
///
/// Accepted limitation: a steer whose content is byte-identical to a
/// later turn's opening prompt consumes that opener early and
/// misattributes the rows in between — a bounded, rare, wrong-pill-only
/// failure. CR13 narrows this class for mid-turn user steers: the busy-path
/// admission tags them `user_steer`, and the origin component of
/// [`row_signature`] then distinguishes them from `user`-origin openers
/// byte-identical text included (regression-locked by
/// `should_not_steal_a_later_opener_when_a_user_steer_row_is_byte_identical_to_it`).
/// The residual class covers same-origin rows (a legacy `User`-origin
/// steer) and media openers: signatures keep only part `type` + text, so
/// two turns with the same opening text but different media compare equal
/// and can misattribute — a wrong pill on that turn, never a crash.
pub fn assign_turn_rows(
    fold: &TurnUsageFold,
    history: &[crate::context::ContextMessage],
) -> TurnRowAssignment {
    let mut prompt_ids = std::collections::HashMap::new();
    let mut row_brackets = std::collections::HashMap::new();
    let mut queue: std::collections::VecDeque<&TurnOpener> = fold.openers.iter().collect();
    let mut current: Option<&TurnOpener> = None;
    for message in history {
        if message.message.role == crate::message::Role::User {
            if let Some((origin, content_sig)) = row_signature(message) {
                let matched = queue.iter().position(|opener| {
                    opener.origin == origin && opener.content_sig == content_sig
                });
                if let Some(index) = matched {
                    // Anchors before the match never materialized (compacted
                    // away) — drop them so later turns still align.
                    let opener = queue.remove(index).expect("position just found");
                    queue.drain(0..index);
                    current = Some(opener);
                }
            }
            continue;
        }
        if message.message.role == crate::message::Role::Assistant
            && let Some(opener) = current
        {
            prompt_ids.insert(message.entry_seq, opener.turn_key.clone());
            row_brackets.insert(message.entry_seq, opener.bracket);
        }
    }
    TurnRowAssignment {
        prompt_ids,
        row_brackets,
    }
}

/// The string-only view of [`assign_turn_rows`]: the assistant-row →
/// turn-key map the transcript read face stamps as `promptId`. Kept for
/// callers that only need the client-facing keys; duration attribution
/// must use the full assignment (bracket identity).
pub fn assign_turn_prompt_ids(
    fold: &TurnUsageFold,
    history: &[crate::context::ContextMessage],
) -> std::collections::HashMap<u64, String> {
    assign_turn_rows(fold, history).prompt_ids
}

/// Attribute each completed turn's server-measured duration to the LAST
/// assistant history row of that turn.
///
/// Takes the row attribution from [`assign_turn_rows`]; for every fold
/// record that carries a duration, the row attributed to that record's
/// BRACKET with the largest entry seq gets the value — the row the live
/// face stamps, so the restored transcript renders the same "Worked for
/// X" pill the live transcript shows. Keying on the bracket ordinal (the
/// record's identity), not the turn-key string: legacy logs whose
/// allocator restarted mint colliding `turn-<n>` keys, and the string
/// key would collapse two distinct turns' rows into one slot (the later
/// batch overwriting the earlier batch's duration). Compaction may have
/// removed earlier rows of the turn, so "last" is the max of whatever
/// survived. Turns whose record carries no duration (dangling brackets,
/// or `turn.ended` records that omitted the field) contribute nothing.
pub fn assign_turn_durations(
    fold: &TurnUsageFold,
    assignment: &TurnRowAssignment,
) -> std::collections::HashMap<u64, u64> {
    let mut last_seq: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for (seq, bracket) in &assignment.row_brackets {
        let entry = last_seq.entry(*bracket).or_insert(*seq);
        if *seq > *entry {
            *entry = *seq;
        }
    }
    let mut out = std::collections::HashMap::new();
    for record in &fold.turns {
        let Some(duration_ms) = record.duration_ms else {
            continue;
        };
        if let Some(seq) = last_seq.get(&record.bracket) {
            out.insert(*seq, duration_ms);
        }
    }
    out
}

/// The matching signature of a user history row: its serialized origin
/// plus a content signature. Media rehydration and blob archives must
/// not break the comparison — see [`content_signature`].
fn row_signature(message: &crate::context::ContextMessage) -> Option<(Value, Value)> {
    let origin = serde_json::to_value(message.origin.as_ref()?).ok()?;
    let content = serde_json::to_value(&message.message.content).ok()?;
    Some((origin, content_signature(Some(&content))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextMessage;
    use crate::message::Message;
    use crate::origin::PromptOrigin;
    use mirri_proto::WireOp;
    use serde_json::json;
    use std::collections::HashMap;

    fn turn_prompt(input: &str) -> WireOp {
        WireOp::TurnPrompt {
            input: json!(input),
            origin: json!({ "kind": "user" }),
        }
    }

    fn user_append(text: &str, origin: Value) -> WireOp {
        WireOp::ContextAppendMessage {
            message: json!({ "role": "user", "content": text, "origin": origin }),
        }
    }

    fn step_end(input: u64, output: u64, read: u64, write: u64) -> WireOp {
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "step.end",
                "uuid": "step-1",
                "usage": {
                    "inputTokens": input,
                    "outputTokens": output,
                    "cacheReadTokens": read,
                    "cacheCreationTokens": write,
                },
            }),
        }
    }

    fn turn_ended(turn_id: u64, duration_ms: Option<u64>) -> WireOp {
        WireOp::TurnEnded {
            turn_id,
            reason: "completed".to_string(),
            error: None,
            duration_ms,
        }
    }

    #[test]
    fn should_aggregate_the_four_usage_buckets_when_step_ends_fall_inside_a_turn_bracket() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(100, 50, 1000, 200),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 1, "one bracket with usage: {fold:?}");
        assert_eq!(fold.turns[0].turn_key, "turn-1");
        assert_eq!(fold.turns[0].input_tokens, 100);
        assert_eq!(fold.turns[0].output_tokens, 50);
        assert_eq!(fold.turns[0].cache_read_tokens, 1000);
        assert_eq!(fold.turns[0].cache_creation_tokens, 200);
    }

    #[test]
    fn should_sum_usage_across_steps_when_multiple_steps_end_inside_one_bracket() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(10, 5, 100, 20),
            step_end(1, 2, 3, 4),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 1);
        assert_eq!(fold.turns[0].input_tokens, 11);
        assert_eq!(fold.turns[0].output_tokens, 7);
        assert_eq!(fold.turns[0].cache_read_tokens, 103);
        assert_eq!(fold.turns[0].cache_creation_tokens, 24);
    }

    #[test]
    fn should_not_emit_a_record_when_a_turn_never_reached_a_step_end() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert!(fold.turns.is_empty(), "no usage → no record: {fold:?}");
        assert_eq!(fold.openers.len(), 1, "the opener anchor is still recorded");
    }

    #[test]
    fn should_capture_the_turn_ended_duration_when_the_wire_record_carries_it() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, Some(1200)),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 1);
        assert_eq!(fold.turns[0].turn_id, Some(1));
        assert_eq!(fold.turns[0].duration_ms, Some(1200));
    }

    #[test]
    fn should_leave_the_duration_none_when_the_bracket_dangles_without_a_turn_ended() {
        // A crashed turn never wrote `turn.ended` — its bracket is re-keyed
        // `aborted-`, and no server-measured duration exists to stamp.
        let ops = vec![
            turn_prompt("first"),
            user_append("first", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_prompt("second"),
            user_append("second", json!({ "kind": "user" })),
            step_end(2, 2, 0, 0),
            turn_ended(2, Some(500)),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 2);
        assert_eq!(fold.turns[0].turn_key, "aborted-1");
        assert_eq!(fold.turns[0].duration_ms, None);
        assert_eq!(fold.turns[1].turn_key, "turn-2");
        assert_eq!(fold.turns[1].duration_ms, Some(500));
    }

    #[test]
    fn should_leave_the_duration_none_when_the_turn_ended_record_omits_it() {
        // The wire field is optional — a record without `durationMs` must
        // fold to None, never to a fabricated 0.
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 1);
        assert_eq!(fold.turns[0].duration_ms, None);
    }

    #[test]
    fn should_collect_deduplicated_models_in_first_seen_order_when_usage_records_fall_inside_a_bracket()
     {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: None,
            },
            step_end(1, 1, 0, 0),
            WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: None,
            },
            WireOp::UsageRecord {
                model: "claude-sonnet-4".to_string(),
                usage: json!({ "inputTokens": 1, "outputTokens": 1 }),
                usage_scope: None,
            },
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 1);
        assert_eq!(fold.turns[0].models, vec!["kimi-k2", "claude-sonnet-4"]);
    }

    #[test]
    fn should_record_the_first_user_message_of_a_bracket_as_the_turn_opener() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.openers.len(), 1);
        assert_eq!(fold.openers[0].turn_key, "turn-1");
        assert_eq!(fold.openers[0].origin, json!({ "kind": "user" }));
        assert_eq!(fold.openers[0].content_sig, json!("hi"));
    }

    #[test]
    fn should_not_replace_the_opener_when_a_second_user_message_lands_inside_the_bracket() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            user_append("a steer", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.openers.len(), 1);
        assert_eq!(fold.openers[0].content_sig, json!("hi"));
    }

    #[test]
    fn should_not_record_an_opener_when_a_user_message_is_appended_outside_any_bracket() {
        let ops = vec![
            user_append("injection", json!({ "kind": "injection" })),
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.openers.len(), 1);
        assert_eq!(fold.openers[0].content_sig, json!("hi"));
    }

    #[test]
    fn should_finalize_a_dangling_open_bracket_when_the_next_turn_prompt_arrives() {
        // A crashed turn never wrote `turn.ended` — its bracket is re-keyed
        // into the `aborted-` namespace so it can never be mistaken for a
        // fork anchor, while the completed bracket keeps the wire id key.
        let ops = vec![
            turn_prompt("first"),
            user_append("first", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_prompt("second"),
            user_append("second", json!({ "kind": "user" })),
            step_end(2, 2, 0, 0),
            turn_ended(2, None),
        ];
        let fold = fold_turn_usages(&ops);
        assert_eq!(fold.turns.len(), 2);
        assert_eq!(fold.turns[0].turn_key, "aborted-1");
        assert_eq!(fold.turns[0].turn_id, None);
        assert_eq!(fold.turns[0].input_tokens, 1);
        assert_eq!(fold.turns[1].turn_key, "turn-2");
        assert_eq!(fold.turns[1].turn_id, Some(2));
        assert_eq!(fold.openers.len(), 2);
        assert_eq!(fold.openers[0].turn_key, "aborted-1");
        assert_eq!(fold.openers[1].turn_key, "turn-2");
    }

    #[test]
    fn should_key_completed_brackets_by_the_wire_turn_id_not_the_fold_ordinal() {
        // Regression (fork-at-turn anchor): a legacy log whose turn
        // allocator restarted (the pre-re-seed close→resume bug) mints
        // `turn.ended` ids that diverge from the bracket ordinals — the
        // fold keyed by ordinal sent the web a `turn-4` anchor that the
        // fork validation rejected ("available turn ids: 1..=2"). The
        // completed bracket's key must be the wire id, so the anchor the
        // web sends is the id `session.fork` validates against.
        let ops = vec![
            turn_prompt("a"),
            user_append("a", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_prompt("b"),
            user_append("b", json!({ "kind": "user" })),
            turn_prompt("c"),
            user_append("c", json!({ "kind": "user" })),
            turn_prompt("d"),
            user_append("d", json!({ "kind": "user" })),
            step_end(4, 4, 0, 0),
            turn_ended(1, None),
            turn_prompt("e"),
            user_append("e", json!({ "kind": "user" })),
            step_end(8, 8, 0, 0),
            turn_ended(2, None),
        ];
        let fold = fold_turn_usages(&ops);
        // Brackets: a, b, c, d(aborted tail closed by turn_ended 1),
        // e(turn_ended 2) — ordinal 5 for the bracket the wire id says is 1.
        assert_eq!(fold.turns.len(), 3);
        assert_eq!(fold.turns[0].turn_key, "aborted-1");
        assert_eq!(fold.turns[1].turn_key, "turn-1");
        assert_eq!(fold.turns[1].turn_id, Some(1));
        assert_eq!(fold.turns[2].turn_key, "turn-2");
        assert_eq!(fold.turns[2].turn_id, Some(2));
        // Attribution stamps the wire-id key on the completed turns' rows.
        let history = vec![
            user_row(1, "a"),
            assistant_row(2, "r1"),
            user_row(3, "d"),
            assistant_row(4, "r2"),
            user_row(5, "e"),
            assistant_row(6, "r3"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert_eq!(ids.get(&2), Some(&"aborted-1".to_string()));
        assert_eq!(ids.get(&4), Some(&"turn-1".to_string()));
        assert_eq!(ids.get(&6), Some(&"turn-2".to_string()));
    }

    #[test]
    fn should_anchor_the_still_open_tail_brackets_opener_without_a_usage_record() {
        // The in-flight turn: no usage record (the turn has not finished, so
        // the pill stays hidden), but its opener anchors attribution and the
        // key itself is exposed — `session.snapshot` surfaces it as
        // `current_prompt_id` so the client's in-flight seed and the
        // transcript rows share one turn identity.
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
        ];
        let fold = fold_turn_usages(&ops);
        assert!(
            fold.turns.is_empty(),
            "open tail emits no usage record: {fold:?}"
        );
        assert_eq!(fold.openers.len(), 1, "the tail opener still anchors rows");
        assert_eq!(fold.openers[0].turn_key, "turn-1");
        assert_eq!(fold.open_turn_key.as_deref(), Some("turn-1"));
    }

    #[test]
    fn should_leave_the_tail_key_unset_and_openers_empty_when_the_open_bracket_has_no_user_row() {
        let ops = vec![turn_prompt("hi"), step_end(1, 1, 0, 0)];
        let fold = fold_turn_usages(&ops);
        assert!(fold.turns.is_empty());
        assert!(fold.openers.is_empty());
        assert_eq!(fold.open_turn_key.as_deref(), Some("turn-1"));
    }

    #[test]
    fn should_stamp_the_in_flight_turns_rows_with_their_own_key_when_the_tail_bracket_is_open() {
        // Regression (mid-turn reload duplicate card): the in-flight turn's
        // opener is the tail bracket, so its assistant rows must carry the
        // tail's own key — not inherit the previous turn's stale attribution,
        // which split the client's transcript group away from the in-flight
        // seed and rendered the running Task card twice.
        let mut ops = Vec::new();
        for (index, text) in ["first", "second"].iter().enumerate() {
            ops.push(turn_prompt(text));
            ops.push(user_append(text, json!({ "kind": "user" })));
            ops.push(step_end(1, 1, 0, 0));
            ops.push(turn_ended(index as u64 + 1, None));
        }
        ops.push(turn_prompt("third"));
        ops.push(user_append("third", json!({ "kind": "user" })));
        ops.push(step_end(1, 1, 0, 0));
        let fold = fold_turn_usages(&ops);
        let history = vec![
            user_row(1, "first"),
            assistant_row(2, "one"),
            user_row(3, "second"),
            assistant_row(4, "two"),
            user_row(5, "third"),
            assistant_row(6, "three (in flight)"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert_eq!(ids.get(&6), Some(&"turn-3".to_string()));
    }

    #[test]
    fn should_reduce_content_parts_to_type_and_text_when_signing_media_content() {
        let content = json!([
            { "type": "text", "text": "look" },
            { "type": "image_url", "image_url": { "url": "blobref:abc", "id": "/archive/x.png" } },
        ]);
        let sig = content_signature(Some(&content));
        assert_eq!(
            sig,
            json!([
                { "type": "text", "text": "look" },
                { "type": "image_url" },
            ])
        );
    }

    fn user_row(entry_seq: u64, text: &str) -> ContextMessage {
        ContextMessage {
            message: Message::user(text),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq,
        }
    }

    fn assistant_row(entry_seq: u64, text: &str) -> ContextMessage {
        let mut row = ContextMessage::from_message(Message::assistant(text));
        row.entry_seq = entry_seq;
        row
    }

    fn fold_with_openers(texts: &[&str]) -> TurnUsageFold {
        let mut ops = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            ops.push(turn_prompt(text));
            ops.push(user_append(text, json!({ "kind": "user" })));
            ops.push(step_end(1, 1, 0, 0));
            ops.push(turn_ended(index as u64 + 1, None));
        }
        fold_turn_usages(&ops)
    }

    #[test]
    fn should_stamp_assistant_rows_with_the_turn_key_when_the_opener_row_matches() {
        let fold = fold_with_openers(&["hi", "again"]);
        let history = vec![
            user_row(1, "hi"),
            assistant_row(2, "hello!"),
            user_row(3, "again"),
            assistant_row(4, "sure"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert_eq!(
            ids,
            HashMap::from([(2u64, "turn-1".to_string()), (4u64, "turn-2".to_string()),])
        );
    }

    #[test]
    fn should_stamp_the_duration_on_the_last_assistant_row_of_each_completed_turn() {
        let ops = vec![
            turn_prompt("first"),
            user_append("first", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, Some(1000)),
            turn_prompt("second"),
            user_append("second", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(2, Some(2000)),
        ];
        let fold = fold_turn_usages(&ops);
        let history = vec![
            user_row(1, "first"),
            assistant_row(2, "r1"),
            assistant_row(3, "r2"),
            user_row(4, "second"),
            assistant_row(5, "r3"),
        ];
        let assignment = assign_turn_rows(&fold, &history);
        let durations = assign_turn_durations(&fold, &assignment);
        assert_eq!(
            durations.get(&3),
            Some(&1000),
            "last row of turn-1: {durations:?}"
        );
        assert_eq!(
            durations.get(&5),
            Some(&2000),
            "last row of turn-2: {durations:?}"
        );
    }

    #[test]
    fn should_count_dangling_turn_ordinals_when_deriving_the_wire_log_turn_id_floor() {
        // The production failure shape: 8 completed turns (ended ids 1..=8)
        // plus dangling brackets whose ordinals run past the ended maximum
        // (29 turn.prompt brackets in total). The floor must be 29 — the
        // aborted namespace's live high mark — not 8, or a seeded allocator
        // would re-mint ids the dangling brackets already occupy.
        let mut ops = Vec::new();
        for n in 1..=8u64 {
            ops.push(turn_prompt(&format!("ended-{n}")));
            ops.push(turn_ended(n, None));
        }
        for n in 9..=29u64 {
            // Dangling brackets: prompts with no turn.ended.
            ops.push(turn_prompt(&format!("dangling-{n}")));
        }
        assert_eq!(
            wire_log_max_turn_id(&ops),
            29,
            "the floor must cover the dangling ordinals, not just ended ids"
        );
    }

    #[test]
    fn should_count_cancelled_turn_ids_when_deriving_the_wire_log_turn_id_floor() {
        let ops = vec![
            turn_prompt("a"),
            turn_ended(4, None),
            turn_prompt("b"),
            WireOp::TurnCancel {
                turn_id: Some(9),
                target: None,
                reason: None,
            },
        ];
        assert_eq!(
            wire_log_max_turn_id(&ops),
            9,
            "a turn.cancel's turnId references an allocated id and must count"
        );
    }

    #[test]
    fn should_return_a_zero_floor_when_the_log_holds_no_turn_records() {
        assert_eq!(wire_log_max_turn_id(&[]), 0);
    }

    #[test]
    fn should_flag_duplicate_turn_ids_when_the_log_holds_repeated_ended_ids() {
        // The allocator-restart signature: every restart re-minted its low
        // ids, so the log holds turn.ended 1 several times.
        let ops = vec![
            turn_prompt("a"),
            turn_ended(1, None),
            turn_prompt("b"),
            turn_ended(2, None),
            turn_prompt("c"),
            turn_ended(1, None),
            turn_prompt("d"),
            turn_ended(1, None),
        ];
        assert_eq!(
            duplicate_turn_ids(&ops),
            vec![1],
            "the re-minted id must be flagged once"
        );
    }

    #[test]
    fn should_flag_nothing_when_every_ended_turn_id_is_unique() {
        let ops = vec![
            turn_prompt("a"),
            turn_ended(1, None),
            turn_prompt("b"),
            turn_ended(2, None),
        ];
        assert!(duplicate_turn_ids(&ops).is_empty());
    }

    #[test]
    fn should_attribute_each_batchs_duration_to_its_own_rows_when_turn_keys_collide_across_restarts()
     {
        // Regression (close→resume turn-id reset): a legacy log whose
        // allocator restarted mints the SAME `turn-<n>` keys in every
        // batch. The duration stamp must follow the fold's record
        // identity (which opener bracket a row belongs to), not the key
        // string — keying by string let the later batch's rows steal the
        // earlier batch's duration slot, so the earlier turn's fold header
        // rendered a bare "Worked" pill.
        let ops = vec![
            // Batch one: turn-1 and turn-2 (a session's first process run).
            turn_prompt("q1"),
            user_append("q1", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, Some(1000)),
            turn_prompt("q2"),
            user_append("q2", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(2, Some(1100)),
            // Batch two: the allocator restarted, so the keys repeat.
            turn_prompt("q3"),
            user_append("q3", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, Some(2000)),
            turn_prompt("q4"),
            user_append("q4", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(2, Some(2100)),
        ];
        let fold = fold_turn_usages(&ops);
        let history = vec![
            user_row(1, "q1"),
            assistant_row(2, "a1"),
            user_row(3, "q2"),
            assistant_row(4, "a2"),
            user_row(5, "q3"),
            assistant_row(6, "a3"),
            user_row(7, "q4"),
            assistant_row(8, "a4"),
        ];
        let assignment = assign_turn_rows(&fold, &history);
        let prompt_ids = &assignment.prompt_ids;
        assert_eq!(prompt_ids.get(&2), Some(&"turn-1".to_string()));
        assert_eq!(prompt_ids.get(&4), Some(&"turn-2".to_string()));
        assert_eq!(
            prompt_ids.get(&6),
            Some(&"turn-1".to_string()),
            "the colliding key stays the wire id (the fork anchor namespace is unchanged)"
        );
        let durations = assign_turn_durations(&fold, &assignment);
        assert_eq!(
            durations.get(&2),
            Some(&1000),
            "batch one's turn-1 keeps its own duration: {durations:?}"
        );
        assert_eq!(
            durations.get(&4),
            Some(&1100),
            "batch one's turn-2 keeps its own duration: {durations:?}"
        );
        assert_eq!(
            durations.get(&6),
            Some(&2000),
            "batch two's turn-1 gets its own duration: {durations:?}"
        );
        assert_eq!(
            durations.get(&8),
            Some(&2100),
            "batch two's turn-2 gets its own duration: {durations:?}"
        );
    }

    #[test]
    fn should_not_stamp_earlier_assistant_rows_of_the_same_turn_when_it_completed() {
        let ops = vec![
            turn_prompt("first"),
            user_append("first", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, Some(1000)),
        ];
        let fold = fold_turn_usages(&ops);
        let history = vec![
            user_row(1, "first"),
            assistant_row(2, "r1"),
            assistant_row(3, "r2"),
        ];
        let assignment = assign_turn_rows(&fold, &history);
        let durations = assign_turn_durations(&fold, &assignment);
        assert_eq!(durations.len(), 1);
        assert!(
            !durations.contains_key(&2),
            "row 2 is not the turn's last assistant row: {durations:?}"
        );
        assert_eq!(durations.get(&3), Some(&1000));
    }

    #[test]
    fn should_stamp_no_row_when_the_turn_ended_without_a_duration() {
        let ops = vec![
            turn_prompt("hi"),
            user_append("hi", json!({ "kind": "user" })),
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        let history = vec![
            user_row(1, "hi"),
            assistant_row(2, "r1"),
            assistant_row(3, "r2"),
        ];
        let assignment = assign_turn_rows(&fold, &history);
        let durations = assign_turn_durations(&fold, &assignment);
        assert!(
            durations.is_empty(),
            "no duration → nothing to stamp: {durations:?}"
        );
    }

    #[test]
    fn should_skip_user_rows_when_their_signatures_match_no_queued_opener() {
        // A steer mid-turn and an injection between turns must not consume
        // openers.
        let fold = fold_with_openers(&["hi", "next"]);
        let history = vec![
            user_row(1, "hi"),
            user_row(2, "a steer"),
            assistant_row(3, "ok"),
            user_row(4, "injected"),
            user_row(5, "next"),
            assistant_row(6, "done"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert_eq!(ids.get(&3), Some(&"turn-1".to_string()));
        assert_eq!(ids.get(&6), Some(&"turn-2".to_string()));
    }

    #[test]
    fn should_drop_leading_openers_when_the_retained_history_starts_mid_turn() {
        // Compaction removed turn 1's opener (and its rows) but kept turn 2's.
        let fold = fold_with_openers(&["first", "second"]);
        let history = vec![
            assistant_row(1, "stale tail of turn 1"),
            user_row(2, "second"),
            assistant_row(3, "fresh"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert!(!ids.contains_key(&1), "no anchor → unattributed");
        assert_eq!(ids.get(&3), Some(&"turn-2".to_string()));
    }

    #[test]
    fn should_return_an_empty_map_when_no_turn_ever_matched() {
        let fold = fold_with_openers(&["hi"]);
        let history = vec![assistant_row(1, "no user row at all")];
        assert!(assign_turn_prompt_ids(&fold, &history).is_empty());
    }

    #[test]
    fn should_match_media_openers_by_part_types_when_payloads_differ_between_sides() {
        // Fold side carries the offloaded blobref; the history row carries
        // the rehydrated form. The signature compares type + text only.
        let ops = vec![
            turn_prompt("look"),
            WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "origin": { "kind": "user" },
                    "content": [
                        { "type": "text", "text": "look" },
                        { "type": "image_url", "image_url": { "url": "blobref:abc" } },
                    ],
                }),
            },
            step_end(1, 1, 0, 0),
            turn_ended(1, None),
        ];
        let fold = fold_turn_usages(&ops);
        let history = vec![
            ContextMessage {
                message: Message {
                    role: crate::message::Role::User,
                    name: None,
                    content: vec![
                        crate::message::ContentPart::Text {
                            text: "look".to_string(),
                        },
                        crate::message::ContentPart::ImageUrl {
                            image_url: crate::message::ImageUrlData {
                                url: "data:image/png;base64,AAAA".to_string(),
                                detail: None,
                                id: None,
                            },
                            detail: None,
                        },
                    ],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: false,
                },
                origin: Some(PromptOrigin::User),
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 1,
            },
            assistant_row(2, "seen"),
        ];
        let ids = assign_turn_prompt_ids(&fold, &history);
        assert_eq!(ids.get(&2), Some(&"turn-1".to_string()));
    }

    /// Given a user steer whose content is byte-identical to a later turn's
    /// opening prompt, when `assign_turn_rows` walks the history, then the
    /// steer row must NOT consume that opener's anchor — a steer is never a
    /// turn opener, so its row is skipped from the anchor-queue lookup (CR13
    /// regression lock: the steer flush's durable `context.append_message`
    /// enlarged the byte-identical misattribution surface).
    #[test]
    fn should_not_steal_a_later_opener_when_a_user_steer_row_is_byte_identical_to_it() {
        // The steer row: user role, user_steer origin, same text as turn 2's
        // opener. It folds inside turn 1's bracket (the CR13 fold).
        let steer_row = ContextMessage {
            message: Message::user("same text"),
            origin: Some(PromptOrigin::UserSteer),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 2,
        };
        let fold = fold_with_openers(&["first", "same text"]);
        let history = vec![
            user_row(1, "first"),
            steer_row,
            assistant_row(3, "turn 1 answer"),
            user_row(4, "same text"),
            assistant_row(5, "turn 2 answer"),
        ];
        let assignment = assign_turn_rows(&fold, &history);
        // Turn 2's opener still anchors its own rows…
        assert_eq!(
            assignment.prompt_ids.get(&5),
            Some(&"turn-2".to_string()),
            "the byte-identical opener must still anchor its own turn: {:?}",
            assignment.prompt_ids
        );
        // …and turn 1's rows stay attributed to turn 1 (the steer did not
        // consume turn 2's anchor early).
        assert_eq!(
            assignment.prompt_ids.get(&3),
            Some(&"turn-1".to_string()),
            "turn 1's rows must stay under turn 1: {:?}",
            assignment.prompt_ids
        );
    }
}
