//! Read-only tail scan for interrupted-task recovery.
//!
//! Spec: `docs/specs/interrupted-task-recovery-architecture.md` §1.3,
//! ADR-008 (read-judge-write, no disk side effect at scan time),
//! ADR-009 (turnId = prompt ordinal; a background hint is a
//! `task.started` without a matching `task.terminated` inside the
//! dangling turn bracket).
//!
//! This module answers one question for a wire log at rest: does its
//! tail end with a turn that was opened (`turn.prompt`) but never
//! closed (`turn.ended` / `turn.cancel`) — i.e. a turn that was running
//! when the process died — and if so, when did it start, which prompt
//! ordinal backs the synthetic closure turnId, and did it leave a
//! background task potentially still running.
//!
//! The scan is strictly read-only: it walks the raw migrated records
//! (pre-`WireOp`, where the per-record `time` stamp still lives — same
//! approach as `read_only_open_turn_started_at`) via
//! [`mirri_persistence::migration::migrate_records_in_memory`], which
//! already serves both on-disk shapes (plain and `.gz`) and drops a
//! torn final line. No file is written, moved, or truncated here.

use std::collections::HashSet;
use std::path::Path;

use mirri_persistence::AppendLogStore;
use mirri_persistence::append_log::JsonlAppendLog;
use mirri_persistence::migration::migrate_records_in_memory;
use mirri_proto::WireOp;
use serde_json::Value;

/// The result of scanning a wire log's tail for a still-open turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTurnScan {
    /// The `time` (epoch-ms) of the dangling turn's `turn.prompt` record.
    pub prompt_time_ms: u64,
    /// The 1-based ordinal of that prompt among all prompts in the log —
    /// the turnId the synthetic interrupted closure reuses (ADR-009).
    pub prompt_ordinal: u64,
    /// A `task.started` occurred inside the dangling turn bracket without
    /// a matching `task.terminated` — the resume prompt should tell the
    /// model to check whether that background task already finished.
    pub pending_background: bool,
}

/// Scan the tail of a wire log for a turn left open by an interruption.
///
/// Returns `Some(OpenTurnScan)` when the last `turn.prompt` has no
/// `turn.ended` and no `turn.cancel` after it; `None` when every turn is
/// closed (a cancelled turn counts as closed — V4: cancelled ≠
/// interrupted) or the log carries no prompt at all. A `.gz`-only
/// session is read decompressed in place, never restored to disk (spec
/// §1.3 — `migrate_records_in_memory` serves both on-disk shapes).
pub async fn scan_open_turn(wire_log_path: &Path) -> anyhow::Result<Option<OpenTurnScan>> {
    // The dual-shape probe: the plain path names the log, but a
    // compressed session holds only `wire.jsonl.gz` — its absence is not
    // "no log" but "the other shape" (the migration reader resolves the
    // shape from the parent dir). Absent BOTH forms: no log at all.
    let parent = wire_log_path.parent();
    let plain_log_exists = wire_log_path.exists(); // zero-io-allow: dual-shape log probe, read-only metadata check, see F-T13-2
    let gz_log_exists = parent.is_some_and(|dir| dir.join("wire.jsonl.gz").exists()); // zero-io-allow: dual-shape log probe, read-only metadata check, see F-T13-2
    if !(plain_log_exists || gz_log_exists) {
        return Ok(None);
    }
    let records = migrate_records_in_memory(wire_log_path).await?;

    // Single forward pass. `open_prompt` holds the currently-open turn's
    // (ordinal, time); it is cleared by turn.ended OR turn.cancel (any
    // turn.cancel closes the bracket — the V4 rule that cancelled turns
    // are not interruptions). `live_tasks` tracks task_ids started but
    // not terminated since the currently-open prompt — the dangling
    // bracket's background content; a new prompt resets it because a task
    // from an earlier closed turn is not this turn's pending hint.
    let mut prompt_ordinal: u64 = 0;
    let mut open_prompt: Option<(u64, u64)> = None;
    let mut live_tasks: HashSet<String> = HashSet::new();
    let mut pending_background = false;

    for record in &records {
        let record_type = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match record_type {
            "turn.prompt" => {
                prompt_ordinal += 1;
                let time = record.get("time").and_then(Value::as_u64);
                open_prompt = time.map(|t| (prompt_ordinal, t));
                live_tasks.clear();
                pending_background = false;
            }
            "turn.ended" | "turn.cancel" => {
                open_prompt = None;
                live_tasks.clear();
                pending_background = false;
            }
            "task.started" => {
                if let Some(task_id) = record.pointer("/info/task_id").and_then(Value::as_str) {
                    live_tasks.insert(task_id.to_string());
                    if open_prompt.is_some() {
                        pending_background = true;
                    }
                }
            }
            "task.terminated" => {
                if let Some(task_id) = record.pointer("/info/task_id").and_then(Value::as_str) {
                    live_tasks.remove(task_id);
                }
                if live_tasks.is_empty() {
                    pending_background = false;
                }
            }
            _ => {}
        }
    }

    match open_prompt {
        Some((ordinal, time)) => Ok(Some(OpenTurnScan {
            prompt_time_ms: time,
            prompt_ordinal: ordinal,
            pending_background: pending_background && !live_tasks.is_empty(),
        })),
        // A prompt without a usable `time` stamp (a legacy TS log) cannot
        // anchor the interruption — mirroring `read_only_open_turn_started_at`,
        // never fabricate one.
        None => Ok(None),
    }
}

/// The result of scanning a wire log's tail for an already-written
/// interrupted closure whose turn was never followed by a newer prompt
/// (F-03 / spec §2.1's persistent arm: a previous boot backfilled the
/// closure, then the server died again before the user acted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedClosureScan {
    /// The `time` (epoch-ms) of the closing `turn.ended { reason:
    /// "interrupted" }` record — the interruption moment.
    pub time_ms: u64,
    /// The closure record's `error.cause` (e.g. `"service_exit"`), when
    /// the record carries one.
    pub cause: Option<String>,
}

/// Scan the tail of a wire log for a trailing interrupted closure with no
/// newer prompt after it (F-03).
///
/// Returns `Some(InterruptedClosureScan)` when the log's LAST
/// `turn.prompt`...`turn.ended` sequence is a `turn.prompt` closed by a
/// `turn.ended { reason: "interrupted" }` with NO later `turn.prompt`
/// (ADR-008's read-time derivation: a newer prompt — a resume or a fresh
/// user message — would make the interruption "already handled"). `None`
/// for any other tail: a completed/cancelled closure, a dangling prompt
/// ([`scan_open_turn`'s territory), a log without prompts, or an
/// interrupted record whose `time` stamp is missing (never fabricate an
/// anchor — the same rule `scan_open_turn` applies).
pub async fn scan_interrupted_closure(
    wire_log_path: &Path,
) -> anyhow::Result<Option<InterruptedClosureScan>> {
    let parent = wire_log_path.parent();
    let plain_log_exists = wire_log_path.exists(); // zero-io-allow: dual-shape log probe, read-only metadata check, see F-T13-2
    let gz_log_exists = parent.is_some_and(|dir| dir.join("wire.jsonl.gz").exists()); // zero-io-allow: dual-shape log probe, read-only metadata check, see F-T13-2
    if !(plain_log_exists || gz_log_exists) {
        return Ok(None);
    }
    let records = migrate_records_in_memory(wire_log_path).await?;

    // Single backward-looking pass: the LAST prompt's closure is the only
    // candidate. Walk forward tracking the latest prompt's
    // (open, closed, closure) state; the answer is whatever state the
    // final prompt ends in.
    let mut last_closure: Option<InterruptedClosureScan> = None;
    let mut open = false;
    for record in &records {
        let record_type = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match record_type {
            "turn.prompt" => {
                open = true;
                last_closure = None;
            }
            "turn.ended" | "turn.cancel" => {
                if open && record.get("reason").and_then(Value::as_str) == Some("interrupted") {
                    let time = record.get("time").and_then(Value::as_u64);
                    let cause = record
                        .pointer("/error/cause")
                        .and_then(Value::as_str)
                        .map(|cause| cause.to_string());
                    // A time-less interrupted record cannot anchor
                    // the retention window — leave it unreported
                    // rather than fabricate (mirrors
                    // `scan_open_turn`).
                    last_closure = time.map(|time_ms| InterruptedClosureScan { time_ms, cause });
                } else {
                    last_closure = None;
                }
                open = false;
            }
            _ => {}
        }
    }

    Ok(last_closure)
}

/// Whether the session directory's `state.json` marks the session
/// archived. Read-only best-effort: any read or parse failure yields
/// `false` (an unreadable state file never suppresses an interruption
/// from the recovery list).
pub async fn session_is_archived(session_dir: &Path) -> bool {
    let state_path = session_dir.join("state.json");
    let Ok(raw) = tokio::fs::read_to_string(&state_path).await else {
        // zero-io-allow: read-only best-effort archived probe on a session dir, see F-T13-2
        return false;
    };
    let Ok(state) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    state
        .get("archived")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Append the synthetic interrupted closure to a non-resident session's
/// wire log: `turn.ended { reason: "interrupted", error: { cause } }` with
/// the dangling prompt's ordinal as the turnId (ADR-009).
///
/// Spec: `docs/specs/interrupted-task-recovery-architecture.md` §2.1 —
/// the backfill runs against the journal of a session that is NOT resident
/// in this process (a resident session's closure goes through its own
/// wire recorder), so the write must not truncate what is already on
/// disk: [`JsonlAppendLog::open_for_append`] appends after the existing
/// records, creating the file only if it is missing.
///
/// Each append is durable on its own (`write_durable`: write + flush +
/// fsync), so the closure has hit disk by the time this returns — the
/// process may exit immediately after (the very crash shape it records).
/// ADR-008's read-judge-write idempotence lives in the caller (Task 5
/// re-checks the tail before writing); this function itself appends
/// unconditionally.
pub async fn append_interrupted_closure(
    wire_log_path: &Path,
    prompt_ordinal: u64,
    cause: &str,
) -> anyhow::Result<()> {
    let mut log = JsonlAppendLog::open_for_append(wire_log_path).await?;
    log.append(&WireOp::TurnEnded {
        turn_id: prompt_ordinal,
        reason: "interrupted".to_string(),
        error: Some(serde_json::json!({ "cause": cause })),
        duration_ms: None,
    })
    .await?;
    log.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh tempdir holding a hand-written `wire.jsonl` (plus whatever
    /// sibling files a test adds to the session dir). The `time` stamps are
    /// hand-written because `JsonlAppendLog::append` stamps them itself —
    /// these cases pin the exact record the persistence layer would have
    /// written (same style as the restore.rs open-turn tests).
    fn write_wire_log(lines: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let wire_log_path = tmp.path().join("wire.jsonl");
        std::fs::write(&wire_log_path, lines.join("\n") + "\n").unwrap();
        tmp
    }

    /// A minimal hand-written prompt record with the given `time` stamp.
    fn prompt(time: u64) -> String {
        format!(
            r#"{{"type":"turn.prompt","input":{{"content":[]}},"origin":{{"kind":"user"}},"time":{time}}}"#
        )
    }

    /// A minimal hand-written turn.ended with the given turnId.
    fn ended(turn_id: u64, time: u64) -> String {
        format!(r#"{{"type":"turn.ended","turnId":{turn_id},"reason":"completed","time":{time}}}"#)
    }

    #[tokio::test]
    async fn should_return_none_when_tail_has_turn_ended() {
        // Given a log whose only turn is opened and then closed
        let tmp = write_wire_log(&[&prompt(1000), &ended(1, 2000)]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then no open turn is reported
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_find_open_turn_with_prompt_ordinal() {
        // Given two fully-closed turns followed by a dangling prompt
        let tmp = write_wire_log(&[
            &prompt(1000),
            &ended(1, 2000),
            &prompt(3000),
            &ended(2, 4000),
            &prompt(5000),
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap()
            .expect("the dangling prompt leaves the turn open");

        // Then the third prompt's ordinal backs the synthetic turnId and
        // its own time anchors the interruption
        assert_eq!(scan.prompt_ordinal, 3);
        assert_eq!(scan.prompt_time_ms, 5000);
        assert!(!scan.pending_background);
    }

    #[tokio::test]
    async fn should_flag_pending_background_when_task_started_without_terminated() {
        // Given a dangling turn whose bracket contains a task.started with
        // no matching task.terminated
        let tmp = write_wire_log(&[
            &prompt(1000),
            r#"{"type":"task.started","info":{"task_id":"t1"},"time":1200}"#,
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap()
            .expect("the dangling prompt leaves the turn open");

        // Then the pending-background hint is flagged
        assert!(scan.pending_background);
        assert_eq!(scan.prompt_time_ms, 1000);
    }

    #[tokio::test]
    async fn should_not_flag_when_prompt_precedes_started_record() {
        // Given a task.started inside an earlier CLOSED turn, then a fresh
        // dangling prompt — the old turn's task is not this turn's hint
        let tmp = write_wire_log(&[
            &prompt(1000),
            r#"{"type":"task.started","info":{"task_id":"t1"},"time":1200}"#,
            &ended(1, 2000),
            &prompt(3000),
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap()
            .expect("the dangling prompt leaves the turn open");

        // Then no background hint is flagged
        assert!(!scan.pending_background);
    }

    #[tokio::test]
    async fn should_not_flag_when_task_terminated_follows_started() {
        // Given a dangling turn whose task.started IS matched by a
        // task.terminated before the interruption
        let tmp = write_wire_log(&[
            &prompt(1000),
            r#"{"type":"task.started","info":{"task_id":"t1"},"time":1200}"#,
            r#"{"type":"task.terminated","info":{"task_id":"t1"},"time":1300}"#,
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap()
            .expect("the dangling prompt leaves the turn open");

        // Then the terminated task leaves no pending hint
        assert!(!scan.pending_background);
    }

    #[tokio::test]
    async fn should_tolerate_torn_final_line() {
        // Given a dangling prompt followed by a torn (truncated, no
        // trailing newline) final line — the crash-mid-append shape.
        // `migrate_records_in_memory` drops the torn tail, so the scan
        // must still surface the prompt's data instead of failing.
        let tmp = tempfile::tempdir().unwrap();
        let wire_log_path = tmp.path().join("wire.jsonl");
        std::fs::write(
            &wire_log_path,
            format!(
                "{}\n{}",
                prompt(1000),
                r#"{"type":"task.started","info":{"task_id":"t1"},"ti"#
            ),
        )
        .unwrap();

        // When the tail is scanned
        let scan = scan_open_turn(&wire_log_path).await.unwrap();

        // Then the dangling prompt still yields its scan — no panic, no
        // error, and the torn record's task is gone with the tail
        assert_eq!(
            scan,
            Some(OpenTurnScan {
                prompt_time_ms: 1000,
                prompt_ordinal: 1,
                pending_background: false,
            })
        );
    }

    #[tokio::test]
    async fn should_treat_turn_cancel_as_closure() {
        // Given a dangling prompt followed by turn.cancel — the user
        // cancelled; V4 says cancelled is not interrupted
        let tmp = write_wire_log(&[
            &prompt(1000),
            r#"{"type":"turn.cancel","turnId":1,"reason":"user","time":2000}"#,
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the cancelled turn is closed, not interrupted
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_treat_turn_cancel_without_turn_id_as_closure() {
        // Given a dangling prompt followed by a turn.cancel that carries
        // no turnId at all (the field is optional on the wire)
        let tmp = write_wire_log(&[&prompt(1000), r#"{"type":"turn.cancel","time":2000}"#]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then ANY turn.cancel after the prompt closes the bracket
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_return_none_when_log_missing_or_promptless() {
        // Given a session dir with no wire log at all
        let tmp = tempfile::tempdir().unwrap();

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the absent log reads as no open turn
        assert_eq!(scan, None);

        // And given a log with records but no prompt
        let tmp = write_wire_log(&[
            r#"{"type":"context.append_message","message":{"role":"assistant"},"time":100}"#,
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then there is no turn to report
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_return_none_when_open_prompt_has_no_time() {
        // Given a dangling prompt whose record carries no `time` stamp (a
        // legacy TS log predating the persistence-layer stamp)
        let tmp = write_wire_log(&[
            &prompt(1000),
            &ended(1, 2000),
            r#"{"type":"turn.prompt","input":{"content":[]},"origin":{"kind":"user"}}"#,
        ]);

        // When the tail is scanned
        let scan = scan_open_turn(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then no anchor is fabricated for the time-less prompt
        assert_eq!(scan, None);
    }

    /// A hand-written interrupted closure (the recovery backfill's wire
    /// shape, pinned here so the closure-scan cases are independent of the
    /// writer).
    fn interrupted_ended(turn_id: u64, time: u64) -> String {
        format!(
            r#"{{"type":"turn.ended","turnId":{turn_id},"reason":"interrupted","error":{{"cause":"service_exit"}},"time":{time}}}"#
        )
    }

    #[tokio::test]
    async fn should_find_trailing_interrupted_closure_with_no_newer_prompt() {
        // Given a log whose last turn is closed by an interrupted closure
        // (a previous boot backfilled it, then the server died again —
        // F-03's persistent arm)
        let tmp = write_wire_log(&[
            &prompt(1000),
            &ended(1, 2000),
            &prompt(3000),
            &interrupted_ended(2, 4000),
        ]);

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap()
            .expect("the trailing interrupted closure is found");

        // Then the closure's own end time anchors the interruption and
        // its recorded cause rides along
        assert_eq!(scan.time_ms, 4000);
        assert_eq!(scan.cause.as_deref(), Some("service_exit"));
    }

    #[tokio::test]
    async fn should_not_report_closure_when_a_newer_prompt_follows() {
        // Given an interrupted closure followed by a NEWER prompt (a
        // resume or a fresh user message — ADR-008's "already handled")
        let tmp = write_wire_log(&[&prompt(1000), &interrupted_ended(1, 2000), &prompt(3000)]);

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the newer prompt suppresses the interruption
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_not_report_closure_when_last_turn_ended_normally() {
        // Given a log whose last turn closes with reason=completed (and an
        // EARLIER interrupted closure — only the last prompt's closure
        // counts)
        let tmp = write_wire_log(&[
            &prompt(1000),
            &interrupted_ended(1, 2000),
            &prompt(3000),
            &ended(2, 4000),
        ]);

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the completed closure is not an interruption
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_not_report_closure_when_tail_is_a_dangling_prompt() {
        // Given a log whose tail is a dangling prompt (the open-turn arm's
        // territory — the closure arm must not double-report)
        let tmp = write_wire_log(&[&prompt(1000), &interrupted_ended(1, 2000), &prompt(3000)]);

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the open turn reads as no trailing closure
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_not_fabricate_time_for_timeless_interrupted_record() {
        // Given a trailing interrupted closure whose record carries no
        // `time` stamp
        let tmp = write_wire_log(&[
            &prompt(1000),
            r#"{"type":"turn.ended","turnId":1,"reason":"interrupted","error":{"cause":"service_exit"}}"#,
        ]);

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then no anchor is fabricated
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_return_none_when_log_missing_for_closure_scan() {
        // Given a session dir with no wire log at all
        let tmp = tempfile::tempdir().unwrap();

        // When the tail is scanned for a closure
        let scan = scan_interrupted_closure(&tmp.path().join("wire.jsonl"))
            .await
            .unwrap();

        // Then the absent log reads as no closure
        assert_eq!(scan, None);
    }

    #[tokio::test]
    async fn should_read_archived_flag_from_state_json() {
        // Given a session dir whose state.json marks the session archived
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("state.json"),
            r#"{"id":"s1","archived":true,"title":"old"}"#,
        )
        .unwrap();

        // When the archived flag is read
        // Then the session reports archived
        assert!(session_is_archived(tmp.path()).await);

        // And given a state.json with archived absent
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("state.json"), r#"{"id":"s1"}"#).unwrap();

        // When the archived flag is read
        // Then the default is not archived
        assert!(!session_is_archived(tmp.path()).await);

        // And given a state.json with archived false
        std::fs::write(
            tmp.path().join("state.json"),
            r#"{"id":"s1","archived":false}"#,
        )
        .unwrap();

        // When the archived flag is read
        // Then the session is not archived
        assert!(!session_is_archived(tmp.path()).await);

        // And given a session dir with no state.json at all
        let tmp = tempfile::tempdir().unwrap();

        // When the archived flag is read
        // Then the read-only best-effort yields false, no panic
        assert!(!session_is_archived(tmp.path()).await);

        // And given a corrupt state.json
        std::fs::write(tmp.path().join("state.json"), "not json").unwrap();

        // When the archived flag is read
        // Then the parse failure still yields false
        assert!(!session_is_archived(tmp.path()).await);
    }
}
