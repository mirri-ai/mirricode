//! Journal version probing and divergence detection.
//!
//! The session wire log (`wire.jsonl`) is append-only, so its byte size works
//! as a cheap version signal: a reader that knows the size of the log as of
//! some anchor can detect, with a single `stat`, whether the on-disk journal
//! is unchanged (`Fresh`), grew (`Divergent`), or was rewritten/truncated
//! (`Truncated`). See spec §2.1 — size is the primary signal; mtime is
//! carried along as auxiliary metadata only.

/// A point-in-time snapshot of the journal file on disk.
///
/// `mtime_ms` is informational only and does not participate in
/// [`check_version`]; a filesystem that cannot determine the modification
/// time reports `0` (via the sync filesystem stat's `mtime` seconds → ms
/// conversion), not `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalVersion {
    pub size: u64,
    pub mtime_ms: Option<u64>,
}

/// Outcome of comparing the on-disk journal version against a known anchor.
///
/// - [`Fresh`](VersionCheck::Fresh) — disk size equals the known size.
/// - [`Divergent`](VersionCheck::Divergent) — disk size is larger (new data
///   appended since the anchor).
/// - [`Truncated`](VersionCheck::Truncated) — disk size is smaller (the
///   journal was rewritten or truncated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    Fresh,
    Divergent,
    Truncated,
}

/// stat 一次 journal 文件；读不到返回 None（调用方按"无法判定"处理）。
///
/// The metadata lookup itself never fails the caller: a missing or
/// unreadable file yields `None`. The stat routes through the injected sync
/// filesystem (zero-IO 契约 6) — mirri-core never stats disk directly.
/// Semantics note: the sync stat does NOT follow symlinks
/// (`symlink_metadata`), unlike a bare `std::fs::metadata` — for the
/// engine-created regular-file journals this is equivalent, and every
/// failure direction stays fail-closed (an unreadable journal probes
/// `None`, which the fences treat as not-fresh / stop).
pub fn probe_journal_version(
    io: &dyn mirri_os::SyncFs,
    path: &std::path::Path,
) -> Option<JournalVersion> {
    let meta = io.stat(path).ok()?;
    let mtime_ms = Some(meta.mtime.saturating_mul(1000));
    Some(JournalVersion {
        size: meta.size,
        mtime_ms,
    })
}

/// 磁盘版本 vs 本地已知字节锚：size 相等 → Fresh；更大 → Divergent；更小 → Truncated。
///
/// Size is the primary signal (the wire log is append-only); `mtime_ms` is
/// informational and deliberately ignored here (see spec §2.1).
pub fn check_version(disk: JournalVersion, known_size: u64) -> VersionCheck {
    match disk.size.cmp(&known_size) {
        std::cmp::Ordering::Equal => VersionCheck::Fresh,
        std::cmp::Ordering::Greater => VersionCheck::Divergent,
        std::cmp::Ordering::Less => VersionCheck::Truncated,
    }
}

/// Outcome of one incremental absorb pass: how many foreign ops were
/// drained and how many bytes the known-size anchor advanced — the same
/// two numbers the follow-up scheduler (Task 5) feeds on.
///
/// `ops_absorbed` counts the main journal's DRAINED LINES (the ops handed
/// to `on_op`, including the routed `subagent.registered` folds and the
/// kinds `apply_absorbed_op` skips with a debug log) — a drain-line count,
/// not an applied-to-memory count. The child-journal counterpart is
/// `child_ops_applied`: every line every child journal's drain handed
/// over (metadata envelopes and unhandled kinds included). The
/// scheduler's freshness probe (§2.1) keys on disk size + anchors, not
/// on these counters.
#[derive(Debug, Default, Clone, Copy)]
pub struct AbsorbReport {
    pub ops_absorbed: usize,
    pub bytes_consumed: u64,
    /// Child-journal drain-line count: every line every child journal's
    /// drain handed over, INCLUDING the `metadata` envelope line and the
    /// kinds `apply_absorbed_op` skips (unhandled kinds) — the same
    /// drain-line semantics `ops_absorbed` has for the main journal, NOT
    /// an applied-to-memory count.
    pub child_ops_applied: usize,
}

/// Apply ONE absorbed foreign op to the target agent's in-memory state and
/// publish the UI-visible event for it.
///
/// The conservative set (spec §5 / US C4.2): messages, loop events, and turn
/// boundaries — exactly the domains the replay reducer folds into
/// `ContextMemory`. Any other op kind is skipped with a debug log (a known
/// v1 clip, tracked in the ROADMAP). `WireOp::SubagentRegistered` is NOT
/// handled here — it is a session-domain op, routed by the caller (the
/// manager's absorb path) to [`absorb_subagent_registration`].
///
/// This runs with the wire-log writer mutex HELD (see
/// [`SessionWireLog::drain_foreign_ops`](crate::scope::SessionWireLog::drain_foreign_ops)):
/// it must only mutate in-memory state and publish events — never write to
/// the wire (that would deadlock).
pub(crate) fn apply_absorbed_op(
    agent: &std::sync::Arc<crate::scope::AgentScope>,
    event_bus: Option<&crate::session::EventBus>,
    agent_id: &str,
    session_id: &str,
    op: mirri_proto::WireOp,
) {
    match op {
        mirri_proto::WireOp::ContextAppendMessage { message } => {
            // Parse through the same converter a resume replays with, so
            // an absorbed message lands in memory exactly like a replayed
            // one (same field paths, same tolerances).
            match crate::session::replay::parse_context_message(&message, None) {
                Some(parsed) => {
                    let role = serde_plain_role(&parsed.message.role);
                    let content_summary = summarize_content(&parsed.message.content);
                    let memory = agent.context_memory();
                    memory
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append_message(parsed);
                    if let Some(event_bus) = event_bus {
                        event_bus.publish(crate::session::SessionEvent::MessageReceived {
                            agent_id: agent_id.to_string(),
                            role,
                            content_summary,
                        });
                    }
                }
                None => tracing::warn!(
                    agent_id,
                    "session consistency: absorbed message could not be converted; skipped"
                ),
            }
        }
        mirri_proto::WireOp::ContextAppendLoopEvent { event } => {
            // Same reducer semantics the replay applies: the raw event
            // folds into ContextMemory's loop-event state machine.
            match crate::session::replay::parse_loop_event(&event) {
                Some(parsed) => {
                    let memory = agent.context_memory();
                    memory
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append_loop_event(parsed);
                }
                None => tracing::debug!(
                    agent_id,
                    "session consistency: absorbed loop event could not be converted; skipped"
                ),
            }
        }
        mirri_proto::WireOp::TurnEnded {
            turn_id,
            reason,
            duration_ms,
            ..
        } => {
            if let Some(event_bus) = event_bus {
                event_bus.publish(crate::session::SessionEvent::TurnCompleted {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    // The durable wire turn key (turn-<numeric>) — a
                    // DIFFERENT spelling from a live turn's UUID-backed key
                    // (turn-<uuid>), the same distinction the fork path's
                    // anchors use.
                    turn_id: format!("turn-{turn_id}"),
                    stop_reason: foreign_stop_reason_wire(&reason),
                    wire_turn_id: turn_id,
                    duration_ms,
                });
            }
            // Close any tool exchange the foreign writer left open when it
            // ended its turn — the same close a replay performs
            // (`reconstruct_context_memory` step 4): without it a crashed
            // foreign process (a tool.call without its result) would leave
            // this memory holding an open exchange forever, poisoning every
            // later append into the deferred buffer.
            let memory = agent.context_memory();
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .close_pending_tool_results();
        }
        other => {
            tracing::debug!(
                op = ?other,
                "session consistency: absorbed op skipped (unhandled kind)"
            );
        }
    }
}

/// Absorb a foreign `subagent.registered` op (batch 1.5, US C1.2): lazily
/// materialize the child agent's scope and its journal slot so the NEXT
/// pass of [`SessionManager::absorb_session_foreign_updates`] drains the
/// child journal into it.
///
/// Idempotent — a child that already exists (restored, locally spawned, or
/// a prior absorb round) keeps its current scope and journal slot
/// untouched: a locally spawned child's anchor already tracks THIS
/// process's own writes and must not be rewound. A freshly adopted FOREIGN
/// child journal (this process never wrote a byte of it) rewinds its
/// anchor to zero, so the next drain folds the child's whole history —
/// the ADR-005 invariant 1 anchor counts only bytes this process wrote,
/// and for a foreign child journal that is none.
///
/// Web-visibility closure (review F1): the live spawn path publishes
/// `SubagentSpawned` then `SubagentStarted` on the session bus, and those
/// two frames are what the web's agent event projector builds the child
/// card from (`subagent.spawned` / `subagent.started` cases). Neither has
/// a WireOp carrier — they exist only as live bus events on the
/// SPAWNING instance — so a following instance absorbs the registration
/// op but would never see the child appear. This function therefore
/// synthesizes the SAME two events (the same shapes, the same order, the
/// fields sourced from the registration op) after the child scope
/// materializes. No new frame vocabulary is invented.
///
/// A child whose scope cannot be created logs a warning and is skipped
/// (the anchor already advanced; the child's content waits for the next
/// session resume) — no lifecycle events are synthesized for it.
///
/// Unlike [`apply_absorbed_op`], this does NOT run under a wire-log lock —
/// the manager drains the main journal's ops into a Vec first and applies
/// them afterwards, so this may await freely (the lazy journal open and
/// the agent creation are both async).
pub(crate) async fn absorb_subagent_registration(
    session: &std::sync::Arc<crate::scope::SessionScope>,
    event_bus: Option<&crate::session::EventBus>,
    registration: &mirri_proto::SubagentRegistration,
) {
    // The identity registry entry (the same fold
    // `SessionScope::record_op` applies to a live registration op — B4
    // T8 / review SF-3): a supervisor's parent-link queries and a
    // supervision roster snapshot read it without a replay.
    session
        .seed_subagent_registry(vec![registration.clone()])
        .await;

    // A child this process already knows (locally spawned or a prior
    // absorb round) keeps its scope, its journal slot, and its anchor —
    // nothing to materialize. Its lifecycle events were published when it
    // was first seen (locally spawned, or a prior absorb round); a
    // replayed registration must not duplicate them.
    if session
        .get_agent(&crate::scope::ids::AgentId::new(&registration.agent_id))
        .await
        .is_some()
    {
        return;
    }

    // The journal slot materializes lazily (open_for_append — never
    // truncated). `child_wire_log`'s install anchors at the file's CURRENT
    // length, which for a foreign child journal is the foreign writer's
    // progress — bytes this process never wrote. Rewind to zero so the
    // next drain folds the child's whole history (the registration op is
    // recorded in the main journal strictly BEFORE the child journal gets
    // any content — B4 T7 spawn ordering — so nothing before this point
    // was ever folded here either).
    let journal = session.child_wire_log(&registration.agent_id).await;
    if journal.is_installed().await {
        journal.rewind_anchor_to_zero().await;
    } else {
        tracing::warn!(
            agent_id = %registration.agent_id,
            "session consistency: child journal slot did not materialize; child content waits for a full resume"
        );
    }

    // The child agent scope — the SAME route `restore_subagents` takes on a
    // restart. The child AgentScopeConfig is deliberately MINIMAL
    // (model/profile from the registration op mapped the way
    // `restore_subagents` maps them, everything else defaulting) — this
    // is the absorb path's lean shape, not `restore_subagents`' full
    // ADR-009 rehydration (approval channels, parent config inheritance).
    // The absorbed child serves read-side following (context memory +
    // events), not turn execution.
    let model_id = registration
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .unwrap_or_default();
    let config = crate::scope::config::AgentScopeConfig {
        model_id: model_id.clone(),
        profile_name: Some(registration.profile_name.clone()),
        thinking_effort: registration.thinking_effort.as_deref().and_then(|effort| {
            serde_json::from_value(serde_json::Value::String(effort.to_string())).ok()
        }),
        ..crate::scope::config::AgentScopeConfig::default()
    };
    match session
        .create_agent(
            crate::scope::ids::AgentId::new(&registration.agent_id),
            config,
        )
        .await
    {
        Ok(_) => {
            // The lifecycle frames the live spawn path publishes, in the
            // same order and shape (`SubagentSpawned` before the child
            // scope exists, `SubagentStarted` after the registration op —
            // see `SessionSubagentHost::spawn`). The fields map from the
            // registration op exactly the way the live path's map:
            // profile_name → subagent_name, the registered model, the
            // parent link, the tool-call link, the swarm slot. The live
            // path's `description` (the dispatch's free-text label) has NO
            // carrier on the wire op, so it stays absent — the web falls
            // back to subagentName, the same degradation it applies to a
            // description-less spawn. `run_in_background` equally has no
            // carrier: the absorb follows the common foreground shape
            // (`false`), matching a wait-mode desktop dispatch.
            if let Some(bus) = event_bus {
                bus.publish(crate::session::SessionEvent::SubagentSpawned {
                    subagent_id: registration.agent_id.clone(),
                    subagent_name: registration.profile_name.clone(),
                    description: None,
                    run_in_background: false,
                    model: if model_id.is_empty() {
                        None
                    } else {
                        Some(model_id)
                    },
                    parent_agent_id: registration.parent_agent_id.clone(),
                    parent_tool_call_id: registration.parent_tool_call_id.clone(),
                    swarm_index: registration.swarm_index,
                });
                bus.publish(crate::session::SessionEvent::SubagentStarted {
                    subagent_id: registration.agent_id.clone(),
                });
            }
        }
        Err(error) => {
            tracing::warn!(
                agent_id = %registration.agent_id,
                %error,
                "session consistency: failed to materialize the absorbed subagent scope; the child journal is not followed this session"
            );
        }
    }
}

/// Map the foreign journal's `turn.ended` reason string onto the wire stop
/// reason vocabulary. Foreign journals may carry engine spellings this
/// kernel never minted (`interrupted`, `max_tokens`, …) — an unrecognized
/// value degrades to `completed` (the same fallback
/// [`turn_stop_reason_wire`](crate::session::event_projection::turn_stop_reason_wire)
/// applies to `Unknown`), never fails the absorb.
fn foreign_stop_reason_wire(reason: &str) -> String {
    match reason {
        "completed" | "filtered" | "blocked" | "cancelled" => reason.to_string(),
        // The synthetic interrupted closure (recovery backfill, ADR-009's
        // fourth interception point) must survive the foreign fold with its
        // reason intact — folding it to `completed` would hide the
        // interruption from every read face.
        "interrupted" => reason.to_string(),
        // Foreign journals may carry engine spellings this kernel never
        // minted (`max_tokens`, …) — an unrecognized value degrades to
        // `completed` (the same fallback
        // [`turn_stop_reason_wire`](crate::session::event_projection::turn_stop_reason_wire)
        // applies to `Unknown`), never fails the absorb.
        _ => "completed".to_string(),
    }
}

/// A short plain-text summary of a message's content for
/// [`SessionEvent::MessageReceived`] — the same shape the send path
/// publishes (text parts concatenated, char-truncated).
fn summarize_content(parts: &[crate::message::ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let crate::message::ContentPart::Text { text: t } = part {
            text.push_str(t);
        }
    }
    let mut truncated: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        truncated.push('…');
    }
    truncated
}

/// The wire spelling of a message role (`user`/`assistant`/…), the same
/// lowercase form the `message_received` event carries.
fn serde_plain_role(role: &crate::message::Role) -> String {
    match role {
        crate::message::Role::System => "system".to_string(),
        crate::message::Role::User => "user".to_string(),
        crate::message::Role::Assistant => "assistant".to_string(),
        crate::message::Role::Tool => "tool".to_string(),
    }
}

#[cfg(test)]
mod drain_tests {
    use crate::scope::SessionWireLog;
    use crate::scope::config::{SessionMetadata, SessionScopeConfig};
    use crate::scope::ids::AgentId;
    use mirri_persistence::AppendLogStore;
    use mirri_persistence::JsonlAppendLog;
    use mirri_proto::WireOp;

    /// A standalone `SessionWireLog` backed by a real tempdir file, with the
    /// initial `ops` flushed through `open_for_append` + `install` (the
    /// already-consumed base the anchor takes — see `SessionWireLog::install`).
    async fn wire_log_with(path: &std::path::Path, ops: &[WireOp]) -> SessionWireLog {
        let mut log = JsonlAppendLog::open_for_append(path).await.unwrap();
        if !ops.is_empty() {
            log.append_batch(ops).await.unwrap();
        }
        let wire_log = SessionWireLog::default();
        assert!(wire_log.install(log).await);
        wire_log
    }

    fn message_op(text: &str) -> WireOp {
        WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": text,
            }),
        }
    }

    fn turn_ended_op(turn_id: u64) -> WireOp {
        WireOp::TurnEnded {
            turn_id,
            reason: "completed".to_string(),
            error: None,
            duration_ms: None,
        }
    }

    #[tokio::test]
    async fn should_absorb_only_complete_lines_and_leave_torn_tail_for_next_round() {
        // Given: a wire log whose on-disk base holds 3 complete op lines
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(
            &path,
            &[message_op("m1"), message_op("m2"), turn_ended_op(1)],
        )
        .await;
        let known_after_install = wire_log.known_size();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), known_after_install);

        // When: a foreign writer appends 1 complete line plus a torn prefix
        let fourth = message_op("m4");
        let fourth_line = format!("{}\n", serde_json::to_string(&fourth).unwrap());
        let torn_prefix = serde_json::to_string(&message_op("m5"))
            .unwrap()
            .chars()
            .take(10)
            .collect::<String>();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{}{}", fourth_line, torn_prefix).unwrap();
        drop(file);

        let mut absorbed_ops: Vec<WireOp> = Vec::new();
        let report = wire_log
            .drain_foreign_ops(&mut |op| absorbed_ops.push(op))
            .await
            .unwrap();

        // Then: exactly the one complete new line is absorbed; the anchor
        // stops at the fourth line's end (the torn tail is left alone, not
        // consumed, not an error).
        assert_eq!(report.ops_absorbed, 1);
        assert_eq!(report.bytes_consumed, fourth_line.len() as u64);
        assert_eq!(absorbed_ops.len(), 1);
        assert!(matches!(
            absorbed_ops[0],
            WireOp::ContextAppendMessage { .. }
        ));
        assert_eq!(
            wire_log.known_size(),
            known_after_install + fourth_line.len() as u64
        );

        // And when: the torn line is completed by a foreign writer and a
        // second drain runs — the now-complete line is absorbed and the
        // anchor reaches the file's end.
        let rest: String = serde_json::to_string(&message_op("m5"))
            .unwrap()
            .chars()
            .skip(10)
            .collect();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", rest).unwrap();
        drop(file);

        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();
        assert_eq!(report.ops_absorbed, 1);
        assert_eq!(
            wire_log.known_size(),
            std::fs::metadata(&path).unwrap().len()
        );
    }

    #[tokio::test]
    async fn should_stop_before_first_unparseable_line_and_not_advance_past_it() {
        // Given: a base log plus a foreign segment: 1 parseable line, 1
        // complete-but-invalid JSON line, 1 parseable line after it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(&path, &[turn_ended_op(1)]).await;
        let known_after_install = wire_log.known_size();

        let good = format!("{}\n", serde_json::to_string(&message_op("ok")).unwrap());
        let bad = "this is not valid json at all\n";
        let after = format!(
            "{}\n",
            serde_json::to_string(&message_op("after-bad")).unwrap()
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{}{}{}", good, bad, after).unwrap();
        drop(file);

        // When: the drain runs
        let mut absorbed_ops = 0usize;
        let report = wire_log
            .drain_foreign_ops(&mut |_op| absorbed_ops += 1)
            .await
            .unwrap();

        // Then: the drain consumes only up to (not including) the bad line,
        // returns Ok (the bad line is "look again later", never a hard
        // error), and the parseable line AFTER it is not swallowed.
        assert_eq!(report.ops_absorbed, 1);
        assert_eq!(absorbed_ops, 1);
        assert_eq!(report.bytes_consumed, good.len() as u64);
        assert_eq!(
            wire_log.known_size(),
            known_after_install + good.len() as u64
        );
    }

    #[tokio::test]
    async fn should_advance_anchor_exactly_to_consumed_bytes_when_absorbing() {
        // Given: a base log, then two foreign lines appended
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(&path, &[message_op("base")]).await;
        let known_after_install = wire_log.known_size();

        let foreign1 = format!("{}\n", serde_json::to_string(&message_op("f1")).unwrap());
        let foreign2 = format!("{}\n", serde_json::to_string(&message_op("f2")).unwrap());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{}{}", foreign1, foreign2).unwrap();
        drop(file);
        let foreign_bytes = foreign1.len() + foreign2.len();

        // When: one drain absorbs both lines
        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();

        // Then: the anchor equals the start plus exactly N absorbed bytes,
        // and a second drain with no new data returns zero ops.
        assert_eq!(report.ops_absorbed, 2);
        assert_eq!(report.bytes_consumed, foreign_bytes as u64);
        assert_eq!(
            wire_log.known_size(),
            known_after_install + foreign_bytes as u64
        );

        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();
        assert_eq!(report.ops_absorbed, 0);
        assert_eq!(report.bytes_consumed, 0);
        assert_eq!(
            wire_log.known_size(),
            known_after_install + foreign_bytes as u64
        );
    }

    #[tokio::test]
    async fn should_leave_torn_multibyte_utf8_tail_for_next_round_without_error() {
        // Given: a base log, then a foreign writer appending a complete
        // line whose text ends with a multi-byte UTF-8 character (Chinese
        // text), caught MID-ENCODING — the read window ends inside the
        // character's byte sequence, with no trailing newline.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(&path, &[message_op("base")]).await;
        let known_after_install = wire_log.known_size();

        let complete = format!(
            "{}\n",
            serde_json::to_string(&message_op("你好世界")).unwrap()
        );
        // Slice inside the multi-byte tail: the final newline AND the last
        // byte of the final 3-byte character are cut — the read window ends
        // mid-character with invalid UTF-8, the exact race a foreign writer
        // appending the line byte-by-byte produces (a read_to_string-based
        // drain would reject the whole read here).
        let torn: &[u8] = &complete.as_bytes()[..complete.len() - 5];
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(torn).unwrap();
        drop(file);

        // When: the drain runs
        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();

        // Then: the torn (non-UTF-8-complete) tail is tolerated exactly
        // like a missing newline — Ok, zero ops, the anchor does not move.
        assert_eq!(report.ops_absorbed, 0);
        assert_eq!(report.bytes_consumed, 0);
        assert_eq!(wire_log.known_size(), known_after_install);

        // And when: the foreign writer completes the line, a second drain
        // absorbs it and the anchor reaches the file's end.
        let rest: &[u8] = &complete.as_bytes()[complete.len() - 5..];
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(rest).unwrap();
        drop(file);

        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();
        assert_eq!(report.ops_absorbed, 1);
        assert_eq!(
            wire_log.known_size(),
            std::fs::metadata(&path).unwrap().len()
        );
    }

    #[tokio::test]
    async fn should_stop_before_a_complete_line_that_is_not_valid_utf8() {
        // Given: a base log, then a foreign segment whose SECOND line is
        // complete (newline-terminated) but not valid UTF-8.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(&path, &[message_op("base")]).await;
        let known_after_install = wire_log.known_size();

        let good = format!("{}\n", serde_json::to_string(&message_op("ok")).unwrap());
        let bad: &[u8] = &[0xff, 0xfe, 0xfd, b'\n'];
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(good.as_bytes()).unwrap();
        file.write_all(bad).unwrap();
        drop(file);

        // When: the drain runs
        let report = wire_log.drain_foreign_ops(&mut |_op| {}).await.unwrap();

        // Then: the drain consumes the good line, stops BEFORE the invalid
        // one (a genuinely bad line, not a torn tail), and returns Ok.
        assert_eq!(report.ops_absorbed, 1);
        assert_eq!(report.bytes_consumed, good.len() as u64);
        assert_eq!(
            wire_log.known_size(),
            known_after_install + good.len() as u64
        );
    }

    #[tokio::test]
    async fn should_not_replay_own_writes_when_absorbing_after_own_record() {
        // Given: one SessionWireLog whose own record_op wrote 2 ops (each
        // record advanced the known-size anchor by its own byte count).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let wire_log = wire_log_with(&path, &[message_op("base")]).await;

        wire_log.record_op(message_op("own-1")).await;
        wire_log.record_op(turn_ended_op(7)).await;
        let known_after_own_writes = wire_log.known_size();
        assert!(known_after_own_writes > 0);

        // When: drain_foreign_ops runs immediately after the own writes
        let mut absorbed_ops = 0usize;
        let report = wire_log
            .drain_foreign_ops(&mut |_op| absorbed_ops += 1)
            .await
            .unwrap();

        // Then: the callback receives zero ops — the process's own writes
        // are already folded into the anchor and must never be re-absorbed
        // (ADR-005 invariant 1).
        assert_eq!(report.ops_absorbed, 0);
        assert_eq!(absorbed_ops, 0);
        assert_eq!(report.bytes_consumed, 0);
        assert_eq!(wire_log.known_size(), known_after_own_writes);
    }

    /// An empty (never-written) wire log slot is a legal drain target: no
    /// writer materialized, nothing to absorb, Ok with zeroed report.
    #[tokio::test]
    async fn should_return_empty_report_when_no_writer_is_installed() {
        let wire_log = SessionWireLog::default();
        let report = wire_log
            .drain_foreign_ops(&mut |_op| panic!("no op may arrive"))
            .await
            .unwrap();
        assert_eq!(report.ops_absorbed, 0);
        assert_eq!(report.bytes_consumed, 0);
    }

    #[tokio::test]
    async fn should_apply_foreign_message_to_memory_and_publish_event_when_manager_absorbs() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session with an installed wire log, and a foreign
        // writer appending one message op to the session's journal.
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(
                SessionMetadata {
                    title: "Absorb test".to_string(),
                    ..Default::default()
                },
                SessionScopeConfig::default(),
            )
            .await
            .unwrap();

        let mut subscriber = {
            let artifacts = manager
                .session_artifacts(session.id())
                .expect("live session has artifacts");
            artifacts.event_bus.subscribe()
        };

        // A live session's main agent is created by the host (the embedded
        // layer's create assembly); the bare manager does not mint one, so
        // the test creates it the way the host would.
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let foreign = message_op("foreign-message");
        let foreign_line = format!("{}\n", serde_json::to_string(&foreign).unwrap());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        write!(file, "{}", foreign_line).unwrap();
        drop(file);

        // When: the manager absorbs the session's foreign updates
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: exactly one op was absorbed, the main agent's context memory
        // holds the foreign message, and a MessageReceived event was
        // published for the UI.
        assert_eq!(report.ops_absorbed, 1);
        let agent = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("main agent exists");
        {
            let memory = agent.context_memory();
            let guard = memory.lock().unwrap();
            let history = guard.history();
            assert_eq!(history.len(), 1);
            assert!(matches!(
                history[0].message.role,
                crate::message::Role::User
            ));
        }
        let event = subscriber
            .try_recv()
            .expect("a MessageReceived event was published");
        assert!(matches!(
            event,
            crate::session::events::SessionEvent::MessageReceived { .. }
        ));
    }

    /// A helper building the absorb fixtures' foreign child-journal
    /// directory: `<session_dir>/agents/<agent_id>/wire.jsonl` with the
    /// protocol envelope plus the given ops, the shape a foreign desktop
    /// instance's subagent host writes.
    async fn write_child_journal(session_dir: &std::path::Path, agent_id: &str, ops: &[WireOp]) {
        let dir = session_dir.join("agents").join(agent_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = JsonlAppendLog::open_for_append(&dir.join("wire.jsonl"))
            .await
            .unwrap();
        let mut all = vec![WireOp::Metadata {
            protocol_version: Some("2".to_string()),
            created_at: Some(1),
        }];
        all.extend_from_slice(ops);
        log.append_batch(&all).await.unwrap();
    }

    fn subagent_registered_op(agent_id: &str) -> WireOp {
        WireOp::SubagentRegistered {
            agent_id: agent_id.to_string(),
            profile_name: "coder".to_string(),
            model: Some("test-model".to_string()),
            thinking_effort: Some("medium".to_string()),
            parent_agent_id: Some("main".to_string()),
            swarm_item: None,
            parent_tool_call_id: None,
            swarm_index: None,
        }
    }

    /// Given a session whose foreign main journal registers a subagent and
    /// whose foreign child journal holds 2 messages, when the manager
    /// absorbs, then the child agent scope materializes lazily, its context
    /// memory holds both messages, and its journal anchor reaches the
    /// file's end — the child journal's own bytes never re-fold on the next
    /// tick.
    #[tokio::test]
    async fn should_lazily_create_child_scope_and_absorb_its_journal_when_subagent_registered_arrives()
     {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session with a main agent, a foreign writer
        // appending a `subagent.registered` op to the main journal, and a
        // foreign child journal holding 2 message ops.
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-child-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let child_id = "agent-child-1";
        write_child_journal(
            wire_log_path.parent().unwrap(),
            child_id,
            &[message_op("child-msg-1"), message_op("child-msg-2")],
        )
        .await;
        let registered = subagent_registered_op(child_id);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);

        // When: the manager absorbs the session's foreign updates
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: the child agent scope exists, its context memory holds both
        // foreign child messages, and its journal anchor sits at the child
        // journal's full length (initialized by the lazy open, not left
        // behind at zero).
        assert_eq!(
            report.ops_absorbed, 1,
            "the registered op folds (the child journal ops fold into the child scope, not the main report)"
        );
        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("the child agent scope materialized lazily");
        {
            let memory = child.context_memory();
            let guard = memory.lock().unwrap();
            let history = guard.history();
            assert_eq!(history.len(), 2, "both foreign child messages folded");
            assert!(
                history
                    .iter()
                    .all(|m| matches!(m.message.role, crate::message::Role::User))
            );
        }
        let child_journal = session.child_wire_log(child_id).await;
        let child_path = manager
            .persistence()
            .agent_wire_log_path(session.id(), child_id);
        assert!(
            child_journal.is_installed().await,
            "the child journal slot is live"
        );
        assert_eq!(
            child_journal.known_size(),
            std::fs::metadata(&child_path).unwrap().len(),
            "the child anchor initialized at the file's full length"
        );

        // And when: a second absorb runs with no new data, nothing re-folds
        // — the anchors hold and the memory is unchanged.
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();
        assert_eq!(report.ops_absorbed, 0);
        let memory = child.context_memory();
        assert_eq!(memory.lock().unwrap().history().len(), 2);
    }

    /// Given a materialized child scope whose journal anchor rests at N
    /// bytes, when a foreign writer appends one message to the child
    /// journal and the manager absorbs again, then the child's memory
    /// grows by exactly that message and a MessageReceived event with the
    /// child's agent dimension reaches the session's event bus.
    #[tokio::test]
    async fn should_absorb_foreign_child_journal_updates_on_each_tick() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a session with a main agent, a foreign child journal with
        // 2 messages, and a foreign `subagent.registered` op — one absorb
        // materializes the child scope and anchors its journal.
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-child-tick-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let mut subscriber = {
            let artifacts = manager
                .session_artifacts(session.id())
                .expect("live session has artifacts");
            artifacts.event_bus.subscribe()
        };

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let child_id = "agent-child-tick";
        write_child_journal(
            wire_log_path.parent().unwrap(),
            child_id,
            &[message_op("child-base-1"), message_op("child-base-2")],
        )
        .await;
        let registered = subagent_registered_op(child_id);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);
        manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child scope materialized");
        assert_eq!(child.context_memory().lock().unwrap().history().len(), 2);
        // Drain the registration-round events (the MessageReceived pair
        // for the two base messages) so the next round asserts on the
        // tick's own event alone.
        while subscriber.try_recv().is_ok() {}

        // When: a foreign writer appends one message to the child journal
        // and the manager absorbs again
        let child_path = manager
            .persistence()
            .agent_wire_log_path(session.id(), child_id);
        let foreign = message_op("child-tick-message");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&child_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&foreign).unwrap()).unwrap();
        drop(file);
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: the child's memory grew by exactly the one foreign message,
        // the child-journal work is accounted in the report's child
        // counters (the main journal held nothing new), and a
        // MessageReceived event carrying the CHILD's agent id reached
        // the bus.
        assert_eq!(report.ops_absorbed, 0);
        assert_eq!(report.child_ops_applied, 1);
        assert_eq!(child.context_memory().lock().unwrap().history().len(), 3);
        let mut saw_child_message = false;
        while let Ok(event) = subscriber.try_recv() {
            if let crate::session::events::SessionEvent::MessageReceived { agent_id, .. } = event
                && agent_id == child_id
            {
                saw_child_message = true;
            }
        }
        assert!(
            saw_child_message,
            "a MessageReceived event with the child's agent dimension must reach the bus"
        );
    }

    #[tokio::test]
    async fn should_close_dangling_tool_exchange_when_absorbing_foreign_turn_end() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session whose foreign journal ends with a step's
        // tool.call that never received its result, followed by the turn's
        // end — the foreign writer crashed mid-exchange and its last durable
        // record closed the turn.
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-dangling-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        for op in [
            mirri_proto::WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "step.begin", "uuid": "foreign-step-1"
                }),
            },
            mirri_proto::WireOp::ContextAppendLoopEvent {
                event: serde_json::json!({
                    "type": "tool.call", "uuid": "foreign-step-1",
                    "callId": "foreign-call-1", "name": "Bash",
                    "input": {"command": "ls"}
                }),
            },
            turn_ended_op(9),
        ] {
            writeln!(file, "{}", serde_json::to_string(&op).unwrap()).unwrap();
        }
        drop(file);

        // When: the manager absorbs the session's foreign updates
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();
        assert_eq!(report.ops_absorbed, 3);

        // Then: the absorbed turn's end closed the dangling exchange — the
        // memory holds no open tool exchange, and the synthesised
        // interrupted result (an error result for the foreign call) is in
        // the history, the same close a replay performs.
        let agent = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("main agent exists");
        let memory = agent.context_memory();
        let guard = memory.lock().unwrap();
        assert!(
            !guard.has_open_tool_exchange(),
            "the absorbed turn end must close the dangling tool exchange"
        );
        let history = guard.history();
        let has_synthesised_result = history.iter().any(|msg| {
            msg.message.tool_call_id.as_deref() == Some("foreign-call-1") && msg.is_error
        });
        assert!(
            has_synthesised_result,
            "a synthesised interrupted tool result must land in the history"
        );
    }

    /// Given a child that already exists (scope + journal slot materialized,
    /// anchor resting at X bytes), when a `subagent.registered` op for the
    /// SAME agent is absorbed again (a replayed registration — the foreign
    /// journal re-registered the identity), then the existing child keeps
    /// everything: the anchor does NOT rewind to zero and the already-folded
    /// memory does not fold a second time.
    #[tokio::test]
    async fn should_not_rewind_or_refold_existing_child_when_registration_replayed() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session with a main agent and a materialized child
        // (one absorb round consumed the registration + both child messages,
        // so the child's anchor rests past them).
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-child-replay-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let child_id = "agent-child-replay";
        write_child_journal(
            wire_log_path.parent().unwrap(),
            child_id,
            &[message_op("child-msg-1"), message_op("child-msg-2")],
        )
        .await;
        let registered = subagent_registered_op(child_id);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);
        manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        let child = session
            .get_agent(&AgentId::new(child_id))
            .await
            .expect("child scope materialized on the first round");
        assert_eq!(child.context_memory().lock().unwrap().history().len(), 2);
        let anchor_after_first = session.child_wire_log(child_id).await.known_size();
        assert!(
            anchor_after_first > 0,
            "the first round drained the child journal"
        );

        // When: the SAME registration op is absorbed again (the foreign
        // journal re-registered the identity — e.g. a replayed segment)
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: the replayed registration is idempotent — the child's
        // anchor did not rewind (no re-fold from zero) and the memory holds
        // exactly the original two messages.
        assert_eq!(report.ops_absorbed, 1, "the replayed registration drained");
        assert_eq!(
            session.child_wire_log(child_id).await.known_size(),
            anchor_after_first,
            "an existing child's anchor must not be rewound by a replayed registration"
        );
        assert_eq!(
            child.context_memory().lock().unwrap().history().len(),
            2,
            "the child's already-folded memory must not fold a second time"
        );
        // And the child's per-truncation signal stays clear (the replay is
        // not a truncation).
        assert!(
            !session.child_wire_log_truncated(child_id).await,
            "a replayed registration must not raise the child's truncation signal"
        );
    }

    /// Given a session whose foreign main journal registers a subagent,
    /// when the manager absorbs, then the session bus carries the SAME two
    /// lifecycle frames the live spawn path publishes — `SubagentSpawned`
    /// then `SubagentStarted`, in that order, with the spawned frame's
    /// fields mapped from the registration op (review F1: the web's
    /// child-card projector consumes these frames, and the absorb path is
    /// their only producer on a following instance because neither has a
    /// WireOp carrier). A REPLAYED registration synthesizes nothing (the
    /// child is already known — no duplicate frames).
    #[tokio::test]
    async fn should_synthesize_subagent_lifecycle_frames_when_registration_absorbed() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session with a main agent, a bus subscriber, a
        // foreign child journal, and the registration op on the main
        // journal (the same fixture the lazy-materialization test uses).
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-lifecycle-frames-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();
        let mut subscriber = {
            let artifacts = manager
                .session_artifacts(session.id())
                .expect("live session has artifacts");
            artifacts.event_bus.subscribe()
        };

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let child_id = "agent-child-lifecycle";
        write_child_journal(
            wire_log_path.parent().unwrap(),
            child_id,
            &[message_op("child-1")],
        )
        .await;
        let mut registered = subagent_registered_op(child_id);
        // A parent tool-call link and a swarm slot — the fields the
        // synthesized spawned frame must carry through.
        if let WireOp::SubagentRegistered {
            parent_tool_call_id,
            swarm_index,
            ..
        } = &mut registered
        {
            *parent_tool_call_id = Some("tool-call-1".to_string());
            *swarm_index = Some(0);
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);

        // When: the manager absorbs the registration
        manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: the bus carries the live-path frame pair — spawned
        // BEFORE started, the spawned frame's identity fields matching
        // the registration op.
        let event = subscriber
            .try_recv()
            .expect("a lifecycle frame was published");
        match event {
            crate::session::events::SessionEvent::SubagentSpawned {
                subagent_id,
                subagent_name,
                run_in_background,
                model,
                parent_agent_id,
                parent_tool_call_id,
                swarm_index,
                ..
            } => {
                assert_eq!(subagent_id, child_id);
                assert_eq!(subagent_name, "coder");
                assert!(
                    !run_in_background,
                    "the absorbed frame follows the foreground shape"
                );
                assert_eq!(model.as_deref(), Some("test-model"));
                assert_eq!(parent_agent_id.as_deref(), Some("main"));
                assert_eq!(parent_tool_call_id.as_deref(), Some("tool-call-1"));
                assert_eq!(swarm_index, Some(0));
            }
            other => panic!("the first synthesized frame must be SubagentSpawned, got {other:?}"),
        }
        let event = subscriber
            .try_recv()
            .expect("the started frame was published");
        assert!(
            matches!(
                event,
                crate::session::events::SessionEvent::SubagentStarted { ref subagent_id }
                    if subagent_id == child_id
            ),
            "the second synthesized frame must be SubagentStarted for the child"
        );

        // And: a REPLAYED registration synthesizes nothing — the child is
        // already known, so no duplicate lifecycle frames reach the bus.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&registered).unwrap()).unwrap();
        drop(file);
        manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();
        while let Ok(event) = subscriber.try_recv() {
            assert!(
                !matches!(
                    event,
                    crate::session::events::SessionEvent::SubagentSpawned { .. }
                        | crate::session::events::SessionEvent::SubagentStarted { .. }
                ),
                "a replayed registration must not duplicate the lifecycle frames"
            );
        }
    }

    /// Given two materialized children, when one child's journal is
    /// externally rebuilt SMALLER than its anchor, then the absorb pass
    /// stops following THAT child only (anchor reset + truncation signal)
    /// while the other child's foreign updates still fold normally.
    #[tokio::test]
    async fn should_stop_following_child_but_continue_others_when_child_journal_truncated() {
        use crate::scope::WorkspaceScope;
        use crate::session::manager::SessionManager;

        // Given: a live session with a main agent and two materialized
        // children, each with one foreign message folded.
        let dir = tempfile::tempdir().unwrap();
        let workspace = std::sync::Arc::new(WorkspaceScope::new(
            crate::scope::WorkspaceId::new("absorb-child-truncate-test-ws"),
            dir.path().to_path_buf(),
            crate::scope::WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            dir.path().join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        let session = manager
            .create_session(SessionMetadata::default(), SessionScopeConfig::default())
            .await
            .unwrap();
        session
            .create_agent(
                AgentId::new("main"),
                crate::scope::AgentScopeConfig::default(),
            )
            .await
            .unwrap();

        let wire_log_path = manager.persistence().wire_log_path(session.id());
        let doomed_id = "agent-child-doomed";
        let healthy_id = "agent-child-healthy";
        write_child_journal(
            wire_log_path.parent().unwrap(),
            doomed_id,
            &[message_op("doomed-1")],
        )
        .await;
        write_child_journal(
            wire_log_path.parent().unwrap(),
            healthy_id,
            &[message_op("healthy-1")],
        )
        .await;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire_log_path)
            .unwrap();
        for op in [
            subagent_registered_op(doomed_id),
            subagent_registered_op(healthy_id),
        ] {
            writeln!(file, "{}", serde_json::to_string(&op).unwrap()).unwrap();
        }
        drop(file);
        manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        let doomed = session
            .get_agent(&AgentId::new(doomed_id))
            .await
            .expect("doomed child materialized");
        let healthy = session
            .get_agent(&AgentId::new(healthy_id))
            .await
            .expect("healthy child materialized");
        assert_eq!(doomed.context_memory().lock().unwrap().history().len(), 1);
        assert_eq!(healthy.context_memory().lock().unwrap().history().len(), 1);
        assert!(
            !session.child_wire_log_truncated(doomed_id).await,
            "no truncation before the external rebuild"
        );

        // When: the doomed child's journal is externally rebuilt smaller
        // AND the healthy child's journal gains one foreign message
        let doomed_path = manager
            .persistence()
            .agent_wire_log_path(session.id(), doomed_id);
        std::fs::write(&doomed_path, "{}\n").unwrap();
        let healthy_path = manager
            .persistence()
            .agent_wire_log_path(session.id(), healthy_id);
        let foreign = message_op("healthy-2");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&healthy_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&foreign).unwrap()).unwrap();
        drop(file);
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();

        // Then: the truncated child stops being followed — its anchor reset
        // to zero and its truncation signal is observable — while the
        // healthy child's foreign message still folded.
        assert_eq!(report.ops_absorbed, 0, "the main journal held nothing new");
        assert!(
            session.child_wire_log_truncated(doomed_id).await,
            "the truncated child's stop signal must be observable"
        );
        assert_eq!(
            session.child_wire_log(doomed_id).await.known_size(),
            0,
            "the truncated child's anchor reset to zero"
        );
        assert_eq!(
            doomed.context_memory().lock().unwrap().history().len(),
            1,
            "the truncated child's memory is left as-is (local stop)"
        );
        assert!(
            !session.child_wire_log_truncated(healthy_id).await,
            "the healthy child carries no truncation signal"
        );
        assert_eq!(
            healthy.context_memory().lock().unwrap().history().len(),
            2,
            "the healthy child's foreign update folded normally"
        );
        // And the session-level signal stays clear — a child's truncation is
        // local, not a full-resume trigger.
        assert!(
            !session.wire_log_truncated(),
            "a child truncation must not raise the session-level signal"
        );

        // And: the follow driver's POSTURE — a subsequent absorb pass (the
        // next tick) must STILL skip the truncated child even though its
        // rebuilt journal now sits ABOVE the zeroed anchor (which would
        // otherwise read as Divergent): the stop signal, not the size
        // comparison, is what governs. The rebuilt journal is a VALID one
        // (the envelope plus a parseable rebuilt history), so a re-fold
        // from zero would fold it onto the stale in-memory one — the very
        // fold the stop semantics forbid.
        {
            let rebuilt_ops = [
                WireOp::Metadata {
                    protocol_version: Some("2".to_string()),
                    created_at: Some(1),
                },
                message_op("doomed-rebuilt-1"),
            ];
            let body = rebuilt_ops
                .iter()
                .map(|op| serde_json::to_string(op).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(
                manager
                    .persistence()
                    .agent_wire_log_path(session.id(), doomed_id),
                format!("{body}\n"),
            )
            .unwrap();
        }
        let report = manager
            .absorb_session_foreign_updates(&session)
            .await
            .unwrap();
        assert_eq!(
            report.child_ops_applied, 0,
            "the next tick must not drain the truncated child's rebuilt journal"
        );
        assert_eq!(
            doomed.context_memory().lock().unwrap().history().len(),
            1,
            "the truncated child's memory must stay untouched across ticks"
        );
        assert!(
            session.child_wire_log_truncated(doomed_id).await,
            "the stop signal must persist across ticks (only a full resume clears it)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn should_report_fresh_when_disk_size_equals_known_size() {
        let disk = JournalVersion {
            size: 100,
            mtime_ms: Some(1),
        };
        assert_eq!(check_version(disk, 100), VersionCheck::Fresh);
    }

    #[test]
    fn should_report_divergent_when_disk_grew_beyond_known_size() {
        let disk = JournalVersion {
            size: 150,
            mtime_ms: Some(2),
        };
        assert_eq!(check_version(disk, 100), VersionCheck::Divergent);
    }

    #[test]
    fn should_report_truncated_when_disk_shrank_below_known_size() {
        let disk = JournalVersion {
            size: 50,
            mtime_ms: Some(3),
        };
        assert_eq!(check_version(disk, 100), VersionCheck::Truncated);
    }

    #[test]
    fn should_return_none_when_probing_missing_file() {
        let io = mirri_os::LocalSyncFs::new();
        assert!(probe_journal_version(&io, Path::new("/nonexistent/wire.jsonl")).is_none());
    }

    #[test]
    fn should_keep_interrupted_reason_in_foreign_fold() {
        // The synthetic interrupted closure (recovery backfill) must survive
        // the foreign-journal fold with its reason intact — ADR-009's fourth
        // interception point; folding it to `completed` would hide the
        // interruption from every read face.
        assert_eq!(foreign_stop_reason_wire("interrupted"), "interrupted");
    }
}
