//! Complete wire protocol op types for the Mirri Rust kernel.
//!
//! Each line in `wire.jsonl` is a flat JSON object tagged with a `type`
//! discriminator (e.g. `"context.append_message"`, `"goal.create"`).
//! The `WireOp` enum models every op type defined by the TypeScript
//! agent (context, turn, goal, plan, profile, permission, LLM, MCP,
//! compaction, usage, skill, task, swarm, interruption, cron,
//! interaction, context size, and collaboration ops), plus an `Unknown`
//! variant that provides forward compatibility with future op types the
//! Rust kernel does not yet recognise.
//!
//! ## Field naming
//!
//! The TypeScript wire format uses camelCase for multi-word field names
//! (e.g. `goalId`, `completionCriterion`). Rust conventions use snake_case.
//! Every multi-word field carries a `#[serde(rename = "camelCaseName")]`
//! attribute so that serialised JSON matches the TS wire format exactly.
//!
//! ## Unknown fields
//!
//! Real TS records include a `time` field on most ops. The Rust variants
//! do not capture `time` (it is handled at the persistence layer instead).
//! Because internally-tagged enums ignore unknown fields by default, the
//! `time` field is silently dropped during deserialisation — this is the
//! intended behaviour.
//!
//! ## Forward compatibility
//!
//! The `Unknown` variant (`#[serde(other)]`) catches any `type` value
//! that Rust does not yet know about, allowing the kernel to skip
//! unrecognised records rather than crashing.

use serde::{Deserialize, Serialize};

/// The complete discriminated union of all wire protocol operations.
///
/// Serialised as an internally-tagged JSON object:
/// ```json
/// {"type":"goal.create","goalId":"g1","objective":"...","time":123}
/// ```
///
/// The `type` field selects the variant; all other fields are mapped
/// to that variant's struct fields. Unknown `type` values fall through
/// to [`WireOp::Unknown`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireOp {
    // ── Envelope (not an op) ───────────────────────────────────────────
    /// First line of every `wire.jsonl` file. Carries protocol version
    /// and creation timestamp. Fields use snake_case on the wire (this
    /// is a system-level envelope, not a domain op).
    #[serde(rename = "metadata")]
    Metadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<u64>,
    },

    // ── Context domain ─────────────────────────────────────────────────
    /// Append a conversation message (role + content + optional tool calls).
    #[serde(rename = "context.append_message")]
    ContextAppendMessage { message: serde_json::Value },

    /// Append a loop event (step.begin, step.end, content.part, tool.call, tool.result).
    #[serde(rename = "context.append_loop_event")]
    ContextAppendLoopEvent { event: serde_json::Value },

    /// Clear all conversation context.
    #[serde(rename = "context.clear")]
    ContextClear {},

    /// Apply a compaction summary, replacing some prior context.
    ///
    /// The TS schema is a union of three variants; for the PoC we use a
    /// single struct with optional fields. The `compacted_count` field
    /// also accepts the alias `count` (used by one of the TS variants).
    #[serde(rename = "context.apply_compaction")]
    ContextApplyCompaction {
        summary: String,
        #[serde(rename = "compactedCount", alias = "count")]
        compacted_count: u32,
        #[serde(
            rename = "tokensBefore",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        tokens_before: Option<u64>,
        #[serde(
            rename = "tokensAfter",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        tokens_after: Option<u64>,
        #[serde(
            rename = "contextSummary",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        context_summary: Option<String>,
        #[serde(
            rename = "droppedCount",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        dropped_count: Option<u32>,
        /// ADR-005: retention tail size (memory slots kept verbatim), for
        /// resume-time reconstruction. `Some` together with
        /// `kept_head_user_message_count` marks a rust-native record — the
        /// rust kernel's three write points always record both. TS-written
        /// records never carry it: the reducer then dispatches on
        /// `kept_user_message_count` (TS newer) or the legacy tail rebuild
        /// (TS legacy).
        #[serde(
            rename = "retainedTailCount",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        retained_tail_count: Option<u32>,
        /// TS v2 dialect (ADR-008 optional-append): the number of real user
        /// messages the TS kernel's compaction kept verbatim ahead of the
        /// summary (`keptUserMessageCount`, protocol events.ts:322 — v2 live
        /// always writes it). The rust kernel never writes it. Its presence
        /// (with `retainedTailCount` absent) marks a TS newer record; its
        /// absence is how TS legacy records are detected
        /// (transcript.ts:267) — there is no `legacyTail` field on the wire.
        #[serde(
            rename = "keptUserMessageCount",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        kept_user_message_count: Option<u32>,
        /// ADR-005: real user messages kept verbatim as the head essence.
        /// On a TS newer record this is `keptHeadUserMessageCount`
        /// (events.ts:330) — of `keptUserMessageCount`, how many messages
        /// form the head segment; present iff the TS selection split into
        /// head + tail (the live context then also holds one elision-marker
        /// message between the segments).
        #[serde(
            rename = "keptHeadUserMessageCount",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        kept_head_user_message_count: Option<u32>,
        /// ADR-005: the elision marker exactly as inserted (serialized),
        /// `None` when nothing was omitted between head and summary.
        #[serde(
            rename = "elisionMessage",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        elision_message: Option<serde_json::Value>,
        /// ADR-007: the replaced old segment's entry-number range (closed
        /// interval) — the minimum and maximum `entry_seq` of the live
        /// prefix this apply folded away. A bracket, not a contiguous run:
        /// an undo between the range's ends leaves the removed numbers
        /// inside the bracket. Absent on legacy logs (replay keeps the
        /// retention-count derivation unchanged).
        #[serde(
            rename = "firstReplacedEntrySeq",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        first_replaced_entry_seq: Option<u64>,
        /// ADR-007: the replaced range's upper end (see
        /// `firstReplacedEntrySeq`).
        #[serde(
            rename = "lastReplacedEntrySeq",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        last_replaced_entry_seq: Option<u64>,
        /// ADR-007: the entry numbers of the entries the overflow-recovery
        /// ladder dropped — entries never even covered by the summary input
        /// (the explicit "not recoverable from this note" boundary).
        /// Mapped from the old segment's entry seqs by position, ascending;
        /// hook-context and Injection slots are skipped. Absent (or empty)
        /// when the ladder dropped nothing.
        #[serde(
            rename = "droppedEntrySeqs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        dropped_entry_seqs: Option<Vec<u64>>,
    },

    /// Undo the last N context records.
    #[serde(rename = "context.undo")]
    ContextUndo { count: u32 },

    // ── Turn domain ────────────────────────────────────────────────────
    /// Start a new agent turn with the given input and origin.
    #[serde(rename = "turn.prompt")]
    TurnPrompt {
        input: serde_json::Value,
        origin: serde_json::Value,
    },

    /// Steer an in-progress turn with additional input.
    #[serde(rename = "turn.steer")]
    TurnSteer {
        input: serde_json::Value,
        origin: serde_json::Value,
    },

    /// Consumption marker for buffered steers: the turn's step loop flushed
    /// `count` steers from the steer buffer into context memory (FIFO).
    /// Replay pops `count` entries from the pending steer state, so a
    /// resume restores exactly the steers that were still parked.
    #[serde(rename = "turn.steer_flushed")]
    TurnSteerFlushed { count: u32 },

    /// Cancel an in-progress turn.
    #[serde(rename = "turn.cancel")]
    TurnCancel {
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Signal that a turn has ended.
    #[serde(rename = "turn.ended")]
    TurnEnded {
        #[serde(rename = "turnId")]
        turn_id: u64,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<serde_json::Value>,
        #[serde(
            rename = "durationMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        duration_ms: Option<u64>,
    },

    // ── Goal domain ────────────────────────────────────────────────────
    /// Create a new goal.
    #[serde(rename = "goal.create")]
    GoalCreate {
        #[serde(rename = "goalId")]
        goal_id: String,
        objective: String,
        #[serde(
            rename = "completionCriterion",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        completion_criterion: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
        #[serde(
            rename = "budgetLimits",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        budget_limits: Option<serde_json::Value>,
    },

    /// Update goal state (all fields optional except the op itself).
    #[serde(rename = "goal.update")]
    GoalUpdate {
        #[serde(rename = "goalId", default, skip_serializing_if = "Option::is_none")]
        goal_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(rename = "turnsUsed", default, skip_serializing_if = "Option::is_none")]
        turns_used: Option<u64>,
        #[serde(
            rename = "tokensUsed",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        tokens_used: Option<u64>,
        #[serde(
            rename = "wallClockMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        wall_clock_ms: Option<u64>,
        #[serde(
            rename = "budgetLimits",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        budget_limits: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
    },

    /// Clear the active goal.
    #[serde(rename = "goal.clear")]
    GoalClear {},

    /// Mark the context as forked (checkpoint boundary).
    #[serde(rename = "forked")]
    Forked {},

    // ── Plan domain ────────────────────────────────────────────────────
    /// Enter plan mode.
    ///
    /// The optional `planText` is the rust dialect's versioned field for
    /// the plan content at entry (added by the T6 F12 batch — old logs
    /// lack it and replay as `None`). The TS lifecycle record keeps only
    /// `{ id }`; plan content crosses engines via `plan.revision`.
    #[serde(rename = "plan_mode.enter")]
    PlanModeEnter {
        id: String,
        #[serde(rename = "planText", default, skip_serializing_if = "Option::is_none")]
        plan_text: Option<String>,
    },

    /// Cancel plan mode.
    #[serde(rename = "plan_mode.cancel")]
    PlanModeCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    /// Exit plan mode (optionally with the plan that was approved).
    #[serde(rename = "plan_mode.exit")]
    PlanModeExit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    /// Record a plan revision (file path, hash, size).
    ///
    /// The optional `planText` is the rust dialect's versioned field
    /// carrying the revision content inline (T6 F12): the rust kernel has
    /// no plan-file/blob convention and its zero-IO core cannot write one,
    /// so the wire record itself is the durable content store. The TS
    /// record is reference-only (`{ id, version, path, sha256, bytes }`)
    /// — TS zod schemas strip the unknown field, so both dialects replay.
    #[serde(rename = "plan.revision")]
    PlanRevision {
        id: String,
        version: u64,
        path: String,
        sha256: String,
        bytes: u64,
        #[serde(rename = "planText", default, skip_serializing_if = "Option::is_none")]
        plan_text: Option<String>,
    },

    // ── Profile domain ─────────────────────────────────────────────────
    /// Bind a profile (model, system prompt, tool allow-list, etc.).
    #[serde(rename = "profile.bind")]
    ProfileBind {
        #[serde(
            rename = "modelAlias",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        model_alias: Option<String>,
        #[serde(
            rename = "profileName",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        profile_name: Option<String>,
        #[serde(rename = "thinkingEffort")]
        thinking_effort: serde_json::Value,
        #[serde(rename = "systemPrompt")]
        system_prompt: String,
        #[serde(
            rename = "activeToolNames",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        active_tool_names: Option<Vec<String>>,
        #[serde(rename = "disallowedTools")]
        disallowed_tools: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagents: Option<Vec<String>>,
    },

    /// Update configuration fields (all optional).
    #[serde(rename = "config.update")]
    ConfigUpdate {
        #[serde(
            rename = "modelAlias",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        model_alias: Option<String>,
        #[serde(
            rename = "profileName",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        profile_name: Option<String>,
        #[serde(
            rename = "thinkingEffort",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thinking_effort: Option<serde_json::Value>,
        #[serde(
            rename = "systemPrompt",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        system_prompt: Option<String>,
        #[serde(
            rename = "disallowedTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        disallowed_tools: Option<Vec<String>>,
    },

    /// Set the active tool set by name.
    #[serde(rename = "tools.set_active_tools")]
    ToolsSetActiveTools { names: Vec<String> },

    /// Reset the active tool set to the default.
    #[serde(rename = "tools.reset_active_tools")]
    ToolsResetActiveTools {},

    /// Register a user-defined tool.
    #[serde(rename = "tools.register_user_tool")]
    ToolsRegisterUserTool { registration: serde_json::Value },

    /// Unregister a user-defined tool by name.
    #[serde(rename = "tools.unregister_user_tool")]
    ToolsUnregisterUserTool { name: String },

    /// Enable or disable a registered user tool by name (`tool.setUserToolEnabled`).
    #[serde(rename = "tools.set_user_tool_enabled")]
    ToolsSetUserToolEnabled { name: String, enabled: bool },

    /// Update a key-value entry in the tool store.
    #[serde(rename = "tools.update_store")]
    ToolsUpdateStore {
        key: String,
        value: serde_json::Value,
    },

    // ── Permission domain ──────────────────────────────────────────────
    /// Set the permission mode (e.g. "auto", "manual", "yolo").
    #[serde(rename = "permission.set_mode")]
    PermissionSetMode { mode: String },

    /// Add permission rules.
    #[serde(rename = "permission.rules.add")]
    PermissionRulesAdd { rules: serde_json::Value },

    /// Record the result of a permission approval/denial.
    #[serde(rename = "permission.record_approval_result")]
    PermissionRecordApprovalResult { record: serde_json::Value },

    // ── LLM domain ─────────────────────────────────────────────────────
    /// The request envelope changed and any provider prefix cache is invalid
    /// from the first changed token (P8a-4). Recorded whenever the tools
    /// table, the effective system prompt, the media projection mode, or the
    /// history itself (compaction) changes between requests — the explicit,
    /// auditable drift marker the design (§7.1 请求头变更检测) requires, so a
    /// resume can reconcile when and why the request prefix drifted.
    ///
    /// Known reasons: `tools_changed`, `system_prompt_changed`,
    /// `media_projection_degraded`, `media_projection_stripped`,
    /// `compaction_applied`.
    #[serde(rename = "request.header.changed")]
    RequestHeaderChanged { reason: String },

    /// Snapshot of the tools sent to the LLM (hash + full tool definitions).
    #[serde(rename = "llm.tools_snapshot")]
    LlmToolsSnapshot {
        hash: String,
        tools: serde_json::Value,
    },

    /// Record an LLM API request.
    #[serde(rename = "llm.request")]
    LlmRequest {
        kind: String,
        provider: String,
        model: String,
        #[serde(
            rename = "modelAlias",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        model_alias: Option<String>,
        #[serde(
            rename = "thinkingEffort",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thinking_effort: Option<serde_json::Value>,
        #[serde(rename = "toolSelect")]
        tool_select: bool,
        #[serde(rename = "systemPromptHash")]
        system_prompt_hash: String,
        #[serde(
            rename = "systemPrompt",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        system_prompt: Option<String>,
        #[serde(rename = "toolsHash")]
        tools_hash: String,
        #[serde(rename = "messageCount")]
        message_count: u64,
        #[serde(rename = "turnStep", default, skip_serializing_if = "Option::is_none")]
        turn_step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projection: Option<String>,
        #[serde(
            rename = "droppedCount",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        dropped_count: Option<u64>,
    },

    // ── MCP domain ─────────────────────────────────────────────────────
    /// Record MCP tools discovered from a server.
    #[serde(rename = "mcp.tools_discovered")]
    McpToolsDiscovered {
        #[serde(rename = "serverName")]
        server_name: String,
        hash: String,
        tools: serde_json::Value,
        #[serde(rename = "enabledNames")]
        enabled_names: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collisions: Option<serde_json::Value>,
    },

    // ── Compaction domain ──────────────────────────────────────────────
    /// Begin a full context compaction.
    #[serde(rename = "full_compaction.begin")]
    FullCompactionBegin { data: serde_json::Value },

    /// Cancel an in-progress full compaction.
    #[serde(rename = "full_compaction.cancel")]
    FullCompactionCancel {},

    /// Mark a full compaction as complete.
    #[serde(rename = "full_compaction.complete")]
    FullCompactionComplete {},

    // ── Usage domain ───────────────────────────────────────────────────
    /// Record token usage for a model call.
    #[serde(rename = "usage.record")]
    UsageRecord {
        model: String,
        usage: serde_json::Value,
        #[serde(
            rename = "usageScope",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        usage_scope: Option<String>,
    },

    // ── Skill domain ───────────────────────────────────────────────────
    /// Reverse event (engine→host). The name collides with two other planes
    /// on purpose of history: the `agent.activateSkill` RPC method's
    /// pre-batch-⑥ name was `skill.activate` (renamed in the D4 §2 fold; no
    /// route table accepts it as a method anymore), and the v2 kernel
    /// defines a `skill.activate` Op
    /// (`packages/agent-core-v2/src/agent/skill/skillOps.ts`) that derives
    /// the `skill.activated` client event. Events, RPC methods, and kernel
    /// Ops are separate lookup spaces — this name is never dispatched as a
    /// method.
    #[serde(rename = "skill.activate")]
    SkillActivate { origin: serde_json::Value },

    // ── File-reference domain (E9) ────────────────────────────────────
    /// Observability record for one `message.send` reference-materialization
    /// pass (E9 / multimodal ADR-007 §11.4, US9.10): the per-path summary of
    /// what was actually injected — full text, truncated text, or a directory
    /// listing — with the line/byte budgets the injection consumed. Journaled
    /// right before the `context.append_message` row carrying the
    /// `file_reference` origin, so replay tooling can pair the two.
    #[serde(rename = "file.reference.summary")]
    FileReferenceSummary { summary: serde_json::Value },

    // ── Task domain ────────────────────────────────────────────────────
    /// A background task has started.
    #[serde(rename = "task.started")]
    TaskStarted { info: serde_json::Value },

    /// A background task has terminated.
    #[serde(rename = "task.terminated")]
    TaskTerminated {
        info: serde_json::Value,
        #[serde(
            rename = "outputTail",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        output_tail: Option<String>,
    },

    // ── Swarm domain ───────────────────────────────────────────────────
    /// Enter swarm mode.
    #[serde(rename = "swarm_mode.enter")]
    SwarmModeEnter { trigger: serde_json::Value },

    /// Exit swarm mode.
    #[serde(rename = "swarm_mode.exit")]
    SwarmModeExit {},

    // ── Interruption domain ────────────────────────────────────────────
    /// Record an interruption reminder.
    #[serde(rename = "interruptionReminder.recorded")]
    InterruptionReminderRecorded {
        #[serde(rename = "turnId")]
        turn_id: u64,
    },

    // ── Cron domain ────────────────────────────────────────────────────
    /// Add a cron task.
    #[serde(rename = "cron.add")]
    CronAdd { task: serde_json::Value },

    /// Delete cron tasks by ID.
    #[serde(rename = "cron.delete")]
    CronDelete { ids: Vec<String> },

    /// Update the cursor (last-fired timestamp) for a cron task.
    #[serde(rename = "cron.cursor")]
    CronCursor {
        id: String,
        #[serde(rename = "lastFiredAt")]
        last_fired_at: u64,
    },

    // ── Interaction domain ─────────────────────────────────────────────
    /// Request user interaction (question, confirmation, etc.).
    #[serde(rename = "interaction.request")]
    InteractionRequest {
        id: String,
        kind: String,
        #[serde(
            rename = "toolCallId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        tool_call_id: Option<String>,
        #[serde(rename = "agentId", default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        request: serde_json::Value,
    },

    /// Resolve a previously requested interaction.
    #[serde(rename = "interaction.resolved")]
    InteractionResolved {
        id: String,
        response: serde_json::Value,
    },

    // ── Context size domain ────────────────────────────────────────────
    /// Record measured context size (length + token count).
    #[serde(rename = "context_size.measured")]
    ContextSizeMeasured { length: u64, tokens: u64 },

    // ── Collaboration domain ───────────────────────────────────────────
    /// Start a multi-agent collaboration session.
    ///
    /// `mode` carries the opaque [`CollaborationMode`] JSON (roles + rules)
    /// and `task` the shared objective. The payloads are kept as
    /// `serde_json::Value` so `mirri-proto` stays independent of the
    /// `mirri-collaboration` crate; the reducer records the op for
    /// durability/replay while the runtime state lives in
    /// `CollaborationManager`.
    #[serde(rename = "collaboration.start")]
    CollaborationStart {
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Opaque `CollaborationMode` JSON (see `mirri-collaboration`).
        mode: serde_json::Value,
        task: String,
    },

    /// End a multi-agent collaboration session.
    #[serde(rename = "collaboration.end")]
    CollaborationEnd {
        #[serde(rename = "sessionId")]
        session_id: String,
    },

    /// Record a message sent inside a collaboration session.
    ///
    /// `message` carries the opaque [`AgentMessage`] JSON (see
    /// `mirri-collaboration`), stored verbatim for replay.
    #[serde(rename = "collaboration.message")]
    CollaborationMessage {
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Opaque `AgentMessage` JSON (see `mirri-collaboration`).
        message: serde_json::Value,
    },

    // ── Subagent registry ──────────────────────────────────────────────
    /// A subagent was registered in the session (B4 T8, D-55) — the
    /// identity-registry mirror on the session's MAIN journal: the reducer
    /// folds these into [`crate::state::AgentState::subagents`], so a
    /// restart rebuilds the children metadata and the per-child journals
    /// replay against known identities.
    #[serde(rename = "subagent.registered")]
    SubagentRegistered {
        #[serde(rename = "agentId")]
        agent_id: String,
        #[serde(rename = "profileName")]
        profile_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(
            rename = "thinkingEffort",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thinking_effort: Option<String>,
        #[serde(
            rename = "parentAgentId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        parent_agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_item: Option<String>,
        /// The spawning tool call the member belongs to — the parent linkage
        /// the supervision snapshot roster keys on (supervision architecture
        /// §4.2). Absent on journals written before the field existed.
        #[serde(
            rename = "parentToolCallId",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        parent_tool_call_id: Option<String>,
        /// The member's index inside its swarm batch (swarm members only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u32>,
    },

    // ── Forward compatibility ──────────────────────────────────────────
    /// Catch-all for op types the Rust kernel does not yet recognise.
    /// Must be the last variant. All fields in the JSON record are
    /// silently dropped when this variant is matched.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_roundtrip_retention_fields_when_serializing_apply_compaction() {
        // Given an apply op carrying the ADR-005 reconstruction counts and
        // the ADR-007 provenance interval
        let op = WireOp::ContextApplyCompaction {
            summary: "s".to_string(),
            compacted_count: 10,
            tokens_before: Some(100),
            tokens_after: Some(40),
            context_summary: Some("SUMMARY".to_string()),
            dropped_count: Some(0),
            retained_tail_count: Some(4),
            kept_user_message_count: Some(3),
            kept_head_user_message_count: Some(2),
            elision_message: Some(serde_json::json!({"role": "user"})),
            first_replaced_entry_seq: Some(3),
            last_replaced_entry_seq: Some(17),
            dropped_entry_seqs: Some(vec![5, 7]),
        };

        // When serialized to the wire shape
        let value = serde_json::to_value(&op).unwrap();

        // Then the new fields use their camelCase spellings
        assert_eq!(value["retainedTailCount"], serde_json::json!(4));
        assert_eq!(value["keptUserMessageCount"], serde_json::json!(3));
        assert_eq!(value["keptHeadUserMessageCount"], serde_json::json!(2));
        // And the elision marker survives with its full content, not just
        // its presence
        assert_eq!(value["elisionMessage"], serde_json::json!({"role": "user"}));
        // And the ADR-007 provenance carries the replaced range and the
        // dropped-entry seq list
        assert_eq!(value["firstReplacedEntrySeq"], serde_json::json!(3));
        assert_eq!(value["lastReplacedEntrySeq"], serde_json::json!(17));
        assert_eq!(value["droppedEntrySeqs"], serde_json::json!([5, 7]));

        // And deserializes back losslessly
        let back: WireOp = serde_json::from_value(value).unwrap();
        match back {
            WireOp::ContextApplyCompaction {
                retained_tail_count: Some(4),
                kept_user_message_count: Some(3),
                kept_head_user_message_count: Some(2),
                elision_message,
                first_replaced_entry_seq: Some(3),
                last_replaced_entry_seq: Some(17),
                dropped_entry_seqs,
                ..
            } => {
                assert_eq!(elision_message, Some(serde_json::json!({"role": "user"})));
                assert_eq!(dropped_entry_seqs, Some(vec![5, 7]));
            }
            _ => panic!("expected ContextApplyCompaction with retention counts"),
        }
    }

    #[test]
    fn should_default_retention_fields_to_none_when_deserializing_legacy_op() {
        // Given a legacy apply op written before the retention fields existed
        let value = serde_json::json!({
            "type": "context.apply_compaction",
            "summary": "s",
            "compactedCount": 10
        });

        // When deserialized
        let op: WireOp = serde_json::from_value(value).unwrap();

        // Then the new fields default to None (old logs stay readable) —
        // the ADR-005 counts, the TS kept-user count, and the ADR-007
        // provenance interval
        assert!(matches!(
            op,
            WireOp::ContextApplyCompaction {
                retained_tail_count: None,
                kept_user_message_count: None,
                kept_head_user_message_count: None,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
                dropped_entry_seqs: None,
                ..
            }
        ));
    }
}
