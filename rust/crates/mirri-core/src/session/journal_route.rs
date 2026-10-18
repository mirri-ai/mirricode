//! Per-subagent journal routing (B4 T7, D-55 — SDR-B4-02).
//!
//! A spawned child's [`TurnFlow`] records through a
//! [`RoutedWireRecorder`]: child-domain ops (the child's own context,
//! turn, usage, llm, and turn-time event ops) land in the child's own
//! `agents/<agent_id>/wire.jsonl` journal, while session-domain ops
//! (goal, plan, cron, permission, config/profile, task, skill, mcp,
//! compaction, swarm, collaboration, interaction, tools, and the
//! `subagent.registered` identity registry) land in the session's main
//! `wire.jsonl` — the single log a restart replays.
//!
//! ## Exhaustive classification (architecture review B1)
//!
//! The routing decision is an **exhaustive `match` over the `WireOp`
//! enum** — every variant has exactly one explicit arm, and there is
//! **no wildcard arm**: adding a `WireOp` variant breaks this file's
//! compilation until it is classified. String-prefix matching is
//! forbidden (`context_size.` is not `context.` — a prefix match would
//! misroute it), and [`WireOp::Unknown`] (the forward-compat catch-all
//! for op types a future TS kernel may write) routes to the **session**
//! journal so unrecognized records are never dropped.
//!
//! The full classification table (every `WireOp` variant) is recorded in
//! SDR-B4-02 and pinned by
//! [`should_classify_every_wire_op_variant_without_a_default_arm`].

use std::future::Future;
use std::pin::Pin;

use mirri_proto::WireOp;

use crate::loop_engine::WireOpRecorder;
use crate::scope::SessionWireLog;

/// The journal a routed op lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalTarget {
    /// The child's own journal (`agents/<agent_id>/wire.jsonl`).
    Agent,
    /// The session's main journal (`wire.jsonl`).
    Session,
}

/// Classify a [`WireOp`] into its journal target (SDR-B4-02's table).
///
/// Exhaustive by construction — one arm per variant, **no wildcard**:
/// a newly added `WireOp` variant fails compilation here until it is
/// classified, which is the enforcement the architecture review requires.
pub fn journal_target(op: &WireOp) -> JournalTarget {
    match op {
        // ── child journal: the child's own context / turn / usage /
        //    llm surface, plus the turn-time event ops a TurnFlow records
        //    between request envelopes ─────────────────────────────────
        WireOp::Metadata { .. } => JournalTarget::Agent,
        WireOp::ContextAppendMessage { .. } => JournalTarget::Agent,
        WireOp::ContextAppendLoopEvent { .. } => JournalTarget::Agent,
        WireOp::ContextClear { .. } => JournalTarget::Agent,
        WireOp::ContextApplyCompaction { .. } => JournalTarget::Agent,
        WireOp::ContextUndo { .. } => JournalTarget::Agent,
        WireOp::ContextSizeMeasured { .. } => JournalTarget::Agent,
        WireOp::TurnPrompt { .. } => JournalTarget::Agent,
        WireOp::TurnSteer { .. } => JournalTarget::Agent,
        WireOp::TurnSteerFlushed { .. } => JournalTarget::Agent,
        WireOp::TurnCancel { .. } => JournalTarget::Agent,
        WireOp::TurnEnded { .. } => JournalTarget::Agent,
        WireOp::UsageRecord { .. } => JournalTarget::Agent,
        WireOp::LlmRequest { .. } => JournalTarget::Agent,
        WireOp::LlmToolsSnapshot { .. } => JournalTarget::Agent,
        WireOp::RequestHeaderChanged { .. } => JournalTarget::Agent,
        WireOp::InterruptionReminderRecorded { .. } => JournalTarget::Agent,

        // ── session journal: session-scoped state the restart replays ──
        WireOp::Forked { .. } => JournalTarget::Session,
        WireOp::GoalCreate { .. } => JournalTarget::Session,
        WireOp::GoalUpdate { .. } => JournalTarget::Session,
        WireOp::GoalClear { .. } => JournalTarget::Session,
        WireOp::PlanModeEnter { .. } => JournalTarget::Session,
        WireOp::PlanModeCancel { .. } => JournalTarget::Session,
        WireOp::PlanModeExit { .. } => JournalTarget::Session,
        WireOp::PlanRevision { .. } => JournalTarget::Session,
        WireOp::PermissionSetMode { .. } => JournalTarget::Session,
        WireOp::PermissionRulesAdd { .. } => JournalTarget::Session,
        WireOp::PermissionRecordApprovalResult { .. } => JournalTarget::Session,
        WireOp::ProfileBind { .. } => JournalTarget::Session,
        WireOp::ConfigUpdate { .. } => JournalTarget::Session,
        // tools.* adjudication (SDR-B4-02 deviation note): ToolManager is
        // per-agent, but the reducer folds these into session-level state
        // (`reducer.rs` ToolsState) and the session-scoped manager is the
        // source of truth — main journal, recorded deviation.
        WireOp::ToolsSetActiveTools { .. } => JournalTarget::Session,
        WireOp::ToolsResetActiveTools { .. } => JournalTarget::Session,
        WireOp::ToolsRegisterUserTool { .. } => JournalTarget::Session,
        WireOp::ToolsUnregisterUserTool { .. } => JournalTarget::Session,
        WireOp::ToolsSetUserToolEnabled { .. } => JournalTarget::Session,
        WireOp::ToolsUpdateStore { .. } => JournalTarget::Session,
        WireOp::McpToolsDiscovered { .. } => JournalTarget::Session,
        WireOp::FullCompactionBegin { .. } => JournalTarget::Session,
        WireOp::FullCompactionCancel { .. } => JournalTarget::Session,
        WireOp::FullCompactionComplete { .. } => JournalTarget::Session,
        WireOp::SkillActivate { .. } => JournalTarget::Session,
        WireOp::TaskStarted { .. } => JournalTarget::Session,
        WireOp::TaskTerminated { .. } => JournalTarget::Session,
        WireOp::SwarmModeEnter { .. } => JournalTarget::Session,
        WireOp::SwarmModeExit { .. } => JournalTarget::Session,
        WireOp::CronAdd { .. } => JournalTarget::Session,
        WireOp::CronDelete { .. } => JournalTarget::Session,
        WireOp::CronCursor { .. } => JournalTarget::Session,
        WireOp::InteractionRequest { .. } => JournalTarget::Session,
        WireOp::InteractionResolved { .. } => JournalTarget::Session,
        WireOp::CollaborationStart { .. } => JournalTarget::Session,
        WireOp::CollaborationEnd { .. } => JournalTarget::Session,
        WireOp::CollaborationMessage { .. } => JournalTarget::Session,
        WireOp::SubagentRegistered { .. } => JournalTarget::Session,
        // The forward-compat catch-all: unknown op types route to the
        // session journal — the log a restart replays — so an
        // unrecognized record is preserved, never dropped.
        WireOp::Unknown => JournalTarget::Session,
    }
}

/// A wire recorder that routes each op to the child's own journal or the
/// session's main journal by [`journal_target`] — the write face of the
/// per-subagent journal split (B4 T7, D-55).
///
/// Both slots are [`SessionWireLog`] handles sharing the same
/// Detached → Deferred → Writer state machine (SDR-004): a child spawned
/// before the session's log materializes buffers its ops until the
/// session's materialization path flushes every registered child slot.
pub struct RoutedWireRecorder {
    agent: SessionWireLog,
    session: SessionWireLog,
}

impl RoutedWireRecorder {
    /// Build a router over the child's journal slot and the session's
    /// journal slot.
    pub fn new(agent: SessionWireLog, session: SessionWireLog) -> Self {
        Self { agent, session }
    }
}

impl WireOpRecorder for RoutedWireRecorder {
    fn record_op<'a>(&'a self, op: WireOp) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            match journal_target(&op) {
                JournalTarget::Agent => self.agent.record_op(op).await,
                JournalTarget::Session => self.session.record_op(op).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every currently-defined `WireOp` variant, as its wire `type` string
    /// with a minimal valid payload, paired with the SDR-B4-02 target.
    ///
    /// The **compile-time** exhaustive match above (no wildcard) is the
    /// enforcement that a new variant must be classified; this test pins
    /// the classification of every *existing* variant in both directions —
    /// an arm that misroutes (either way) fails the pair.
    fn every_op_type() -> Vec<(&'static str, JournalTarget)> {
        vec![
            ("metadata", JournalTarget::Agent),
            ("context.append_message", JournalTarget::Agent),
            ("context.append_loop_event", JournalTarget::Agent),
            ("context.clear", JournalTarget::Agent),
            ("context.apply_compaction", JournalTarget::Agent),
            ("context.undo", JournalTarget::Agent),
            ("context_size.measured", JournalTarget::Agent),
            ("turn.prompt", JournalTarget::Agent),
            ("turn.steer", JournalTarget::Agent),
            ("turn.steer_flushed", JournalTarget::Agent),
            ("turn.cancel", JournalTarget::Agent),
            ("turn.ended", JournalTarget::Agent),
            ("usage.record", JournalTarget::Agent),
            ("llm.request", JournalTarget::Agent),
            ("llm.tools_snapshot", JournalTarget::Agent),
            ("request.header.changed", JournalTarget::Agent),
            ("interruptionReminder.recorded", JournalTarget::Agent),
            ("forked", JournalTarget::Session),
            ("goal.create", JournalTarget::Session),
            ("goal.update", JournalTarget::Session),
            ("goal.clear", JournalTarget::Session),
            ("plan_mode.enter", JournalTarget::Session),
            ("plan_mode.cancel", JournalTarget::Session),
            ("plan_mode.exit", JournalTarget::Session),
            ("plan.revision", JournalTarget::Session),
            ("permission.set_mode", JournalTarget::Session),
            ("permission.rules.add", JournalTarget::Session),
            ("permission.record_approval_result", JournalTarget::Session),
            ("profile.bind", JournalTarget::Session),
            ("config.update", JournalTarget::Session),
            ("tools.set_active_tools", JournalTarget::Session),
            ("tools.reset_active_tools", JournalTarget::Session),
            ("tools.register_user_tool", JournalTarget::Session),
            ("tools.unregister_user_tool", JournalTarget::Session),
            ("tools.set_user_tool_enabled", JournalTarget::Session),
            ("tools.update_store", JournalTarget::Session),
            ("mcp.tools_discovered", JournalTarget::Session),
            ("full_compaction.begin", JournalTarget::Session),
            ("full_compaction.cancel", JournalTarget::Session),
            ("full_compaction.complete", JournalTarget::Session),
            ("skill.activate", JournalTarget::Session),
            ("task.started", JournalTarget::Session),
            ("task.terminated", JournalTarget::Session),
            ("swarm_mode.enter", JournalTarget::Session),
            ("swarm_mode.exit", JournalTarget::Session),
            ("cron.add", JournalTarget::Session),
            ("cron.delete", JournalTarget::Session),
            ("cron.cursor", JournalTarget::Session),
            ("interaction.request", JournalTarget::Session),
            ("interaction.resolved", JournalTarget::Session),
            ("collaboration.start", JournalTarget::Session),
            ("collaboration.end", JournalTarget::Session),
            ("collaboration.message", JournalTarget::Session),
            ("subagent.registered", JournalTarget::Session),
        ]
    }

    /// A minimal valid payload per op type — just enough for serde to
    /// parse the record into the corresponding `WireOp` variant.
    fn minimal_payload(op_type: &str) -> serde_json::Value {
        match op_type {
            "context.append_message" => serde_json::json!({"message": {}}),
            "context.append_loop_event" => serde_json::json!({"event": {}}),
            "context.apply_compaction" => {
                serde_json::json!({"summary": "", "compactedCount": 0})
            }
            "context.undo" => serde_json::json!({"count": 1}),
            "context_size.measured" => serde_json::json!({"length": 1, "tokens": 1}),
            "turn.prompt" | "turn.steer" => serde_json::json!({"input": {}, "origin": {}}),
            "turn.steer_flushed" => serde_json::json!({"count": 1}),
            "turn.ended" => serde_json::json!({"turnId": 1, "reason": ""}),
            "usage.record" => serde_json::json!({"model": "", "usage": {}}),
            "llm.tools_snapshot" => serde_json::json!({"hash": "", "tools": []}),
            "llm.request" => serde_json::json!({
                "kind": "", "provider": "", "model": "", "toolSelect": false,
                "systemPromptHash": "", "toolsHash": "", "messageCount": 0
            }),
            "request.header.changed" => serde_json::json!({"reason": ""}),
            "interruptionReminder.recorded" => serde_json::json!({"turnId": 1}),
            "goal.create" => serde_json::json!({"goalId": "", "objective": ""}),
            "plan_mode.enter" => serde_json::json!({"id": ""}),
            "plan.revision" => {
                serde_json::json!({"id": "", "version": 0, "path": "", "sha256": "", "bytes": 0})
            }
            "profile.bind" => serde_json::json!({
                "thinkingEffort": null, "systemPrompt": "", "disallowedTools": []
            }),
            "tools.set_active_tools" => serde_json::json!({"names": []}),
            "tools.register_user_tool" => serde_json::json!({"registration": {}}),
            "subagent.registered" => {
                serde_json::json!({"agentId": "", "profileName": ""})
            }
            "tools.unregister_user_tool" => serde_json::json!({"name": ""}),
            "tools.set_user_tool_enabled" => serde_json::json!({"name": "", "enabled": true}),
            "tools.update_store" => serde_json::json!({"key": "", "value": {}}),
            "permission.set_mode" => serde_json::json!({"mode": ""}),
            "permission.rules.add" => serde_json::json!({"rules": {}}),
            "permission.record_approval_result" => serde_json::json!({"record": {}}),
            "mcp.tools_discovered" => {
                serde_json::json!({"serverName": "", "hash": "", "tools": [], "enabledNames": []})
            }
            "full_compaction.begin" => serde_json::json!({"data": {}}),
            "skill.activate" => serde_json::json!({"origin": {}}),
            "task.started" | "task.terminated" => serde_json::json!({"info": {}}),
            "swarm_mode.enter" => serde_json::json!({"trigger": {}}),
            "cron.add" => serde_json::json!({"task": {}}),
            "cron.delete" => serde_json::json!({"ids": []}),
            "cron.cursor" => serde_json::json!({"id": "", "lastFiredAt": 0}),
            "interaction.request" => {
                serde_json::json!({"id": "", "kind": "", "request": {}})
            }
            "interaction.resolved" => serde_json::json!({"id": "", "response": {}}),
            "collaboration.start" => {
                serde_json::json!({"sessionId": "", "mode": {}, "task": ""})
            }
            "collaboration.end" => serde_json::json!({"sessionId": ""}),
            "collaboration.message" => serde_json::json!({"sessionId": "", "message": {}}),
            _ => serde_json::json!({}),
        }
    }

    /// should classify every WireOp variant without a default arm
    ///
    /// Walks the full op-type table (both child and session targets),
    /// parses each into its `WireOp` variant, and asserts the router
    /// sends it to the SDR-B4-02 target. A variant whose payload fails
    /// to parse also fails the test — the table must stay in lockstep
    /// with the enum.
    #[test]
    fn should_classify_every_wire_op_variant_without_a_default_arm() {
        let table = every_op_type();
        assert!(
            table.len() >= 50,
            "the classification table must cover every WireOp variant \
             (found {})",
            table.len()
        );
        for (op_type, expected) in table {
            let mut record = minimal_payload(op_type);
            record["type"] = serde_json::Value::String(op_type.to_string());
            let op: WireOp = serde_json::from_value(record)
                .unwrap_or_else(|e| panic!("{op_type} should parse as a WireOp: {e}"));
            assert!(
                !matches!(op, WireOp::Unknown),
                "{op_type} must parse to a known variant"
            );
            let actual = journal_target(&op);
            assert_eq!(
                actual, expected,
                "{op_type} must route to {expected:?} per the SDR-B4-02 table"
            );
        }
    }

    /// should route an unknown op type to the session journal (no data loss)
    #[test]
    fn should_route_unknown_op_types_to_the_session_journal() {
        let op: WireOp =
            serde_json::from_value(serde_json::json!({"type": "some.future.op"})).unwrap();
        assert!(matches!(op, WireOp::Unknown));
        assert_eq!(journal_target(&op), JournalTarget::Session);
    }

    /// should route a context_size op to the child journal (not the
    /// session, whose `context.` prefix a string-prefix match would have
    /// claimed)
    #[test]
    fn should_route_context_size_to_the_agent_journal_not_by_prefix() {
        let op: WireOp = serde_json::from_value(serde_json::json!({
            "type": "context_size.measured", "length": 1, "tokens": 1
        }))
        .unwrap();
        assert_eq!(journal_target(&op), JournalTarget::Agent);
    }
}
