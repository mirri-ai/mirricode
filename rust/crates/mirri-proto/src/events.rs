//! Domain events emitted by the wire-op reducer.
//!
//! [`DomainEvent`] is a flat, internally-tagged enum — exactly like
//! [`crate::wire::WireOp`] — where each variant directly holds its fields
//! and the `type` discriminator selects the variant. There are **no**
//! nested sub-enums; a flat structure is the only design that works with
//! `#[serde(tag = "type")]` because both outer and inner enums would
//! otherwise compete for the single `type` field in the JSON object.
//!
//! ## Forward compatibility
//!
//! The [`DomainEvent::Unknown`] variant (`#[serde(other)]`) catches any
//! `type` value the Rust kernel does not yet recognise, allowing event
//! streams produced by newer agents to be consumed without crashing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A domain event produced by folding a [`crate::wire::WireOp`] through
/// [`crate::reducer::reduce`].
///
/// Serialised as an internally-tagged JSON object:
/// ```json
/// {"type":"turn_started","turn_id":1}
/// ```
///
/// The `type` field selects the variant; all other fields are mapped to
/// that variant's struct fields. Unknown `type` values fall through to
/// [`DomainEvent::Unknown`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum DomainEvent {
    // ── Session events ───────────────────────────────────────────────
    /// A new agent session was created.
    #[serde(rename = "session_created")]
    SessionCreated {
        session_id: String,
        workspace: String,
    },

    /// An agent session was closed.
    #[serde(rename = "session_closed")]
    SessionClosed { session_id: String, reason: String },

    // ── Agent events ─────────────────────────────────────────────────
    /// A new agent was created within a session.
    #[serde(rename = "agent_created")]
    AgentCreated {
        agent_id: String,
        session_id: String,
        profile: String,
    },

    /// An agent was destroyed.
    #[serde(rename = "agent_destroyed")]
    AgentDestroyed { agent_id: String },

    // ── Turn events ──────────────────────────────────────────────────
    /// A new turn has started (incremented turn counter).
    #[serde(rename = "turn_started")]
    TurnStarted { turn_id: u64 },

    /// A turn completed successfully.
    #[serde(rename = "turn_completed")]
    TurnCompleted {
        turn_id: u64,
        input_tokens: u64,
        output_tokens: u64,
    },

    /// A turn was cancelled before completing.
    #[serde(rename = "turn_cancelled")]
    TurnCancelled { turn_id: u64, reason: String },

    /// A turn failed with an error.
    #[serde(rename = "turn_failed")]
    TurnFailed { turn_id: u64, error: String },

    /// A message was appended to the conversation context.
    #[serde(rename = "message_added")]
    MessageAdded {
        role: String,
        content_preview: String,
    },

    /// A loop event was appended to the conversation context.
    #[serde(rename = "loop_event_added")]
    LoopEventAdded { event_type: String },

    // ── Tool events ──────────────────────────────────────────────────
    /// A tool call has started.
    #[serde(rename = "tool_call_started")]
    ToolCallStarted { turn_id: u64, tool_name: String },

    /// A tool call completed successfully.
    #[serde(rename = "tool_call_completed")]
    ToolCallCompleted {
        turn_id: u64,
        tool_name: String,
        duration_ms: u64,
    },

    /// A tool call failed.
    #[serde(rename = "tool_call_failed")]
    ToolCallFailed {
        turn_id: u64,
        tool_name: String,
        error: String,
    },

    // ── Provider events ──────────────────────────────────────────────
    /// A provider (LLM API) request was sent.
    #[serde(rename = "provider_request_sent")]
    ProviderRequestSent { turn_id: u64, model: String },

    /// The first streaming chunk arrived from the provider.
    #[serde(rename = "provider_first_chunk")]
    ProviderFirstChunk { turn_id: u64, latency_ms: u64 },

    /// The provider stream completed.
    #[serde(rename = "provider_stream_completed")]
    ProviderStreamCompleted { turn_id: u64, total_tokens: u64 },

    /// The provider stream errored.
    #[serde(rename = "provider_stream_error")]
    ProviderStreamError { turn_id: u64, error: String },

    // ── Wire events ──────────────────────────────────────────────────
    /// A wire op was appended to the wire log.
    #[serde(rename = "wire_op_appended")]
    WireOpAppended { op_index: u64, op_type: String },

    /// A wire snapshot was written to disk.
    #[serde(rename = "wire_snapshot_written")]
    WireSnapshotWritten { op_index: u64, size_bytes: u64 },

    /// A wire replay completed.
    #[serde(rename = "wire_replay_completed")]
    WireReplayCompleted { ops_replayed: u64, duration_ms: u64 },

    // ── Permission events ────────────────────────────────────────────
    /// The permission mode changed (e.g. manual → auto).
    #[serde(rename = "permission_mode_changed")]
    PermissionModeChanged { old_mode: String, new_mode: String },

    /// A permission decision was made for a tool call.
    #[serde(rename = "permission_decision")]
    PermissionDecision { tool_name: String, decision: String },

    // ── Compaction events ────────────────────────────────────────────
    /// A full context compaction has started.
    #[serde(rename = "compaction_started")]
    CompactionStarted { from_op: u64, message_count: u64 },

    /// A full context compaction completed.
    #[serde(rename = "compaction_completed")]
    CompactionCompleted { summary_tokens: u32, to_op: u64 },

    /// A full context compaction failed.
    #[serde(rename = "compaction_failed")]
    CompactionFailed { reason: String, retry_count: u32 },

    // ── Background task events ───────────────────────────────────────
    /// A background task has started.
    #[serde(rename = "background_task_started")]
    BackgroundTaskStarted {
        task_id: String,
        description: String,
    },

    /// A background task completed.
    #[serde(rename = "background_task_completed")]
    BackgroundTaskCompleted {
        task_id: String,
        exit_code: Option<i32>,
    },

    /// A background task failed.
    #[serde(rename = "background_task_failed")]
    BackgroundTaskFailed { task_id: String, error: String },

    // ── Cron events ──────────────────────────────────────────────────
    /// A cron task was created.
    #[serde(rename = "cron_task_created")]
    CronTaskCreated { task_id: String, schedule: String },

    /// A cron task fired.
    #[serde(rename = "cron_task_fired")]
    CronTaskFired {
        task_id: String,
        coalesced_count: u32,
    },

    /// A cron task was deleted.
    #[serde(rename = "cron_task_deleted")]
    CronTaskDeleted { task_id: String },

    // ── Plan events ──────────────────────────────────────────────────
    /// A plan was activated.
    #[serde(rename = "plan_activated")]
    PlanActivated { plan_id: String },

    /// A plan was approved.
    #[serde(rename = "plan_approved")]
    PlanApproved { plan_id: String },

    /// A plan was deactivated.
    #[serde(rename = "plan_deactivated")]
    PlanDeactivated { plan_id: String },

    // ── Goal events ──────────────────────────────────────────────────
    /// A new goal was started.
    #[serde(rename = "goal_started")]
    GoalStarted { objective: String },

    /// The active goal was paused.
    #[serde(rename = "goal_paused")]
    GoalPaused { reason: String },

    /// The active goal was resumed.
    #[serde(rename = "goal_resumed")]
    GoalResumed {},

    /// The active goal was completed.
    #[serde(rename = "goal_completed")]
    GoalCompleted {},

    /// The active goal was blocked.
    #[serde(rename = "goal_blocked")]
    GoalBlocked { reason: String },

    /// The active goal was cleared.
    #[serde(rename = "goal_cleared")]
    GoalCleared {},

    /// The active goal's status was updated.
    #[serde(rename = "goal_updated")]
    GoalUpdated { status: String },

    // ── Context events ───────────────────────────────────────────────
    /// The conversation context was cleared.
    #[serde(rename = "context_cleared")]
    ContextCleared {},

    /// Context records were undone (removed from the tail).
    #[serde(rename = "context_undone")]
    ContextUndone { count: u32 },

    /// A compaction summary was applied to the conversation context.
    #[serde(rename = "compaction_applied")]
    CompactionApplied { summary_tokens: u32 },

    // ── Usage events ─────────────────────────────────────────────────
    /// Token usage was recorded for a model call.
    #[serde(rename = "usage_recorded")]
    UsageRecorded {
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        /// Prompt-cache read tokens. `#[serde(default)]` keeps events
        /// serialized before the three-bucket carry-through deserializable.
        #[serde(default)]
        cache_read_tokens: u64,
        /// Prompt-cache write (creation) tokens.
        #[serde(default)]
        cache_creation_tokens: u64,
    },

    // ── Plan mode events ─────────────────────────────────────────────
    /// Plan mode was entered.
    #[serde(rename = "plan_mode_entered")]
    PlanModeEntered { plan_id: String },

    /// Plan mode was exited (plan approved or abandoned).
    #[serde(rename = "plan_mode_exited")]
    PlanModeExited {},

    /// Plan mode was cancelled.
    #[serde(rename = "plan_mode_cancelled")]
    PlanModeCancelled {},

    /// A plan revision was recorded.
    #[serde(rename = "plan_revision")]
    PlanRevision { plan_id: String, version: u64 },

    // ── MCP events ───────────────────────────────────────────────────
    /// MCP tools were discovered from a server.
    #[serde(rename = "mcp_tools_discovered")]
    McpToolsDiscovered {
        server_name: String,
        tool_count: u32,
    },

    // ── Profile events ───────────────────────────────────────────────
    /// A profile was bound to the session.
    #[serde(rename = "profile_bound")]
    ProfileBound { profile_name: String },

    /// Configuration was updated.
    #[serde(rename = "config_updated")]
    ConfigUpdated { model_alias: Option<String> },

    // ── Collaboration events ─────────────────────────────────────────
    /// A cohort was created.
    #[serde(rename = "cohort_created")]
    CohortCreated {
        cohort_id: String,
        pattern_name: String,
    },

    /// A cohort was terminated.
    #[serde(rename = "cohort_terminated")]
    CohortTerminated { cohort_id: String, reason: String },

    /// A message was sent between roles.
    #[serde(rename = "role_message")]
    RoleMessage {
        from: String,
        to: String,
        channel: String,
    },

    /// Consensus was reached on a topic.
    #[serde(rename = "consensus_reached")]
    ConsensusReached { topic: String, decision: String },

    /// An escalation was requested.
    #[serde(rename = "escalation_requested")]
    EscalationRequested { from: String, conflict: String },

    /// The collaboration progression advanced to a new phase.
    #[serde(rename = "progression_advanced")]
    ProgressionAdvanced {
        from_phase: String,
        to_phase: String,
    },

    /// A guardrail rule was triggered.
    #[serde(rename = "guardrail_triggered")]
    GuardrailTriggered { rule: String, action: String },

    /// The shared context was updated.
    #[serde(rename = "shared_context_updated")]
    SharedContextUpdated { key: String },

    /// A role was released by an agent.
    #[serde(rename = "role_released")]
    RoleReleased { role_name: String, agent_id: String },

    // ── Collaboration session events ──────────────────────────────────
    /// A collaboration session was started (Gap 3A: wired from the
    /// `collaboration.start` wire op).
    #[serde(rename = "collaboration_session_started")]
    CollaborationSessionStarted { session_id: String, task: String },

    /// A collaboration session was ended (Gap 3A: wired from the
    /// `collaboration.end` wire op).
    #[serde(rename = "collaboration_session_ended")]
    CollaborationSessionEnded { session_id: String },

    /// A message was sent within a collaboration session (Gap 3A: wired
    /// from the `collaboration.message` wire op). `from` is `None` for
    /// messages that carry no sender on the wire.
    #[serde(rename = "collaboration_message_sent")]
    CollaborationMessageSent {
        session_id: String,
        from: Option<String>,
    },

    // ── Request-envelope events ──────────────────────────────────────
    /// The request envelope changed (P8a-4): the tools table, the effective
    /// system prompt, the media projection mode, or the history itself
    /// (compaction) changed between requests, so any provider prefix cache
    /// is invalid from the first changed token.
    #[serde(rename = "request_header_changed")]
    RequestHeaderChanged { reason: String },

    // ── Forward compatibility ────────────────────────────────────────
    /// Catch-all for event types the Rust kernel does not yet recognise.
    /// Must be the last variant. All fields in the JSON object are
    /// silently dropped when this variant is matched.
    #[serde(other)]
    Unknown,
}
