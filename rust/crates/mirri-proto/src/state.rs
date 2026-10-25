//! Agent state model for the Mirri Rust kernel.
//!
//! [`AgentState`] is the single, authoritative snapshot of everything the
//! kernel knows about a session at a given point in the wire log. It is
//! constructed by replaying [`crate::wire::WireOp`] records through the
//! [`crate::reducer::reduce`] function.
//!
//! ## Raw JSON storage
//!
//! For PoC-1 we deliberately do **not** replicate the TypeScript folding
//! algorithm that transforms wire records into richly typed conversation
//! objects. Instead, messages and loop events are stored as raw
//! [`serde_json::Value`] objects. This is sufficient to prove
//! `wire.jsonl` compatibility (read → store → verify) without committing
//! to a specific message model that may change.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The complete, authoritative state of a Mirri agent session.
///
/// Produced by folding a sequence of [`crate::wire::WireOp`] records
/// through [`crate::reducer::reduce`]. Every field is serialisable so the
/// state can be checkpointed to disk and restored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentState {
    /// Conversation context (messages, loop events, compaction summary).
    pub conversation: ConversationState,
    /// Permission mode governing tool execution.
    pub permission: PermissionState,
    /// The active goal, if one has been created and not yet cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalState>,
    /// Budget limits and current consumption.
    pub budget: BudgetState,
    /// Accumulated token usage across all model calls.
    pub usage: UsageState,
    /// Monotonically increasing turn counter (incremented on `turn.prompt`).
    #[serde(default)]
    pub turn_counter: u64,
    /// Plan mode state, present only while plan mode is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<PlanModeState>,
    /// The plan's durable text — the restore source for the plan content
    /// (F12). Folded from `plan_mode.enter`'s `planText` (the rust
    /// dialect's versioned field) and overwritten by every
    /// `plan.revision` that carries one (the TS restore-semantics carrier,
    /// mapped to inline text here because the rust kernel has no
    /// plan-file/blob store). A `plan_mode.exit` keeps it (the approved
    /// plan stays readable); a `plan_mode.cancel` discards it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_text: Option<String>,
    /// Cron tasks registered via `cron.add`, stored verbatim in the wire
    /// task shape (the F16 wire DTO — camelCase, epoch millis — or the
    /// legacy snake_case serde shape on logs the migration never rewrote).
    /// Raw JSON rather than a typed struct so a single bad record can be
    /// skipped at restore time instead of failing the whole replay;
    /// `cron.delete` removes entries and `cron.cursor` stamps `lastFiredAt`
    /// in place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_tasks: Vec<serde_json::Value>,
    /// Tool-activation state derived from the tools/profile wire ops
    /// (`tools.set_active_tools`, `tools.reset_active_tools`,
    /// `tools.register_user_tool`, `tools.unregister_user_tool`, and the
    /// `profile.bind` seeded active set).
    #[serde(default, skip_serializing_if = "ToolsState::is_default")]
    pub tools: ToolsState,
    /// The bound profile's name (`profile.bind` / `config.update`), when one
    /// was recorded. The profile payload itself is host-owned config — only
    /// the binding is persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    /// The effective model alias the agent runs on (`config.update` /
    /// `profile.bind`), when one was recorded. A resume re-applies it so a
    /// restored session keeps its model instead of falling back to the
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The subagent identity registry folded from `subagent.registered`
    /// ops (B4 T8, D-55) — restart replay rebuilds the children metadata
    /// from this registry, then lazily rehydrates each child's agent from
    /// its own `agents/<id>/wire.jsonl` journal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentRegistration>,
    /// Request-envelope audit summary folded from `request.header.changed`
    /// ops: how many times the request header drifted over the wire log and
    /// the most recent reason — so a resume can reconcile when (and why)
    /// the request prefix changed (P8a-4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_header: Option<RequestHeaderState>,
    /// Steers buffered but not yet flushed into context: one
    /// `{"input": […], "origin": …}` entry per `turn.steer` op, popped by
    /// `turn.steer_flushed` markers. The durable steer buffer a resume
    /// restores into the agent's turn flow (T3, the durable-steer ruling —
    /// parked steers survive a restart instead of being silently dropped).
    /// Raw JSON like `cron_tasks` so one bad record never fails the replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steer_pending: Vec<serde_json::Value>,
    /// Turns whose user-interruption reminder has not reached the
    /// conversation yet (interruption domain; fed by `turn.cancel`,
    /// drained by `interruptionReminder.recorded`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interruption_pending: Vec<u64>,
    /// Active swarm-mode trigger (None = not in swarm mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_trigger: Option<serde_json::Value>,
}

impl AgentState {
    /// Create a fresh state with all defaults (empty conversation,
    /// manual permission, no goal, zero usage, turn counter at zero).
    pub fn new() -> Self {
        Self {
            conversation: ConversationState::default(),
            permission: PermissionState::default(),
            goal: None,
            budget: BudgetState::default(),
            usage: UsageState::default(),
            turn_counter: 0,
            plan_mode: None,
            plan_text: None,
            cron_tasks: Vec::new(),
            tools: ToolsState::default(),
            profile_name: None,
            model: None,
            request_header: None,
            steer_pending: Vec::new(),
            interruption_pending: Vec::new(),
            swarm_trigger: None,
            subagents: Vec::new(),
        }
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

/// Request-envelope audit summary folded from `request.header.changed` ops
/// (P8a-4). A prefix-drift marker: every recorded change means any provider
/// prefix cache was invalid from the first changed token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RequestHeaderState {
    /// How many times the request header changed over the wire log.
    #[serde(default)]
    pub change_count: u64,
    /// The reason carried by the most recent change
    /// (`tools_changed` / `system_prompt_changed` /
    /// `media_projection_degraded` / `media_projection_stripped` /
    /// `compaction_applied`).
    #[serde(default)]
    pub last_reason: String,
}

/// Conversation context accumulated from `context.*` wire ops.
///
/// All fields have natural defaults (empty vectors, `None`, zero), so
/// `Default` is derived.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConversationState {
    /// Raw `ContextMessage` JSON objects from `context.append_message` ops.
    ///
    /// Each entry is the exact JSON value from the wire record, stored
    /// verbatim so that round-trip fidelity can be verified.
    pub messages: Vec<serde_json::Value>,
    /// Raw `LoopRecordedEvent` JSON objects from `context.append_loop_event` ops.
    ///
    /// Loop events include sub-types such as `step.begin`, `step.end`,
    /// `tool.call`, and `tool.result`. They are stored raw for PoC-1.
    pub loop_events: Vec<serde_json::Value>,
    /// The interleaved wire-op order of the two vectors above — without it
    /// the original message/loop-event interleaving is unrecoverable and a
    /// context-memory reconstruction would place every message before every
    /// loop event. Each entry references its record by index; the vectors
    /// stay authoritative for content. Empty on states written before the
    /// field existed (consumers fall back to the lossy two-phase order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<TimelineEntry>,
    /// Compaction summary text, set by `context.apply_compaction`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Token count of the conversation just before the last compaction.
    #[serde(default)]
    pub summary_tokens: u32,
    /// Op sequence number at which the last compaction was applied.
    #[serde(default)]
    pub last_compaction_op: u64,
}

/// One position in the conversation's wire-op order.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum TimelineEntry {
    /// The message at `ConversationState::messages[index]`.
    Message { index: usize },
    /// The loop event at `ConversationState::loop_events[index]`.
    LoopEvent { index: usize },
}

/// Permission mode governing how tool calls are authorised.
///
/// The default mode is `"manual"` (not derivable via `#[derive(Default)]`
/// because `String::default()` is `""`), so `Default` is implemented manually.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionState {
    /// The active permission mode: `"manual"`, `"auto"`, or `"yolo"`.
    pub mode: String,
    /// Deduplicated session-approval patterns derived from
    /// `permission.record_approval_result` records whose decision was
    /// approved with session scope (TS parity: the v2 kernel's
    /// `sessionApprovalRulePatterns` — the full approval records are the
    /// wire log itself; only the derived patterns are model state).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_approval_patterns: Vec<String>,
}

impl Default for PermissionState {
    fn default() -> Self {
        Self {
            mode: "manual".to_string(),
            session_approval_patterns: Vec::new(),
        }
    }
}

/// A goal created via `goal.create` and tracked until `goal.clear`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GoalState {
    /// Unique identifier for the goal.
    pub goal_id: String,
    /// Human-readable description of the goal objective.
    pub objective: String,
    /// Optional completion criterion (natural-language or structured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    /// Current goal status: `"active"`, `"paused"`, `"completed"`, etc.
    pub status: String,
    /// Turns consumed by the goal so far (updated via `goal.update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_used: Option<u64>,
    /// Tokens consumed by the goal so far (updated via `goal.update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    /// Budget limits JSON (structure defined by the goal creator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_limits: Option<serde_json::Value>,
}

/// Budget limits and current consumption for the session.
///
/// All fields default to `None` or `0`, so `Default` is derived.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BudgetState {
    /// Maximum number of turns allowed, if a limit was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_limit: Option<u32>,
    /// Maximum number of tokens allowed, if a limit was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<u64>,
    /// Turns consumed so far.
    #[serde(default)]
    pub turns_used: u32,
    /// Tokens consumed so far.
    #[serde(default)]
    pub tokens_used: u64,
}

/// The session's first LLM call usage snapshot — the baseline the
/// "after cold start" cache-hit metric subtracts (SDR-004).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FirstCallUsage {
    /// Prompt tokens of the first call.
    #[serde(default)]
    pub input_tokens: u64,
    /// Completion tokens of the first call.
    #[serde(default)]
    pub output_tokens: u64,
    /// Cached prompt tokens of the first call.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cache-write prompt tokens of the first call.
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

/// Accumulated token usage across all model calls.
///
/// Both fields default to `0`, so `Default` is derived.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageState {
    /// Total input tokens across all `usage.record` ops.
    #[serde(default)]
    pub total_input_tokens: u64,
    /// Total output tokens across all `usage.record` ops.
    #[serde(default)]
    pub total_output_tokens: u64,
    /// Total prompt-cache read tokens across all `usage.record` ops.
    /// `#[serde(default)]` keeps pre-three-bucket state snapshots
    /// deserializable.
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// Total prompt-cache write (creation) tokens across all `usage.record`
    /// ops.
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    /// The session's first LLM call usage — the baseline the "after cold
    /// start" cache-hit metric subtracts (SDR-004). Captured by the reducer
    /// when the first `usage.record` arrives with no accumulated usage;
    /// `#[serde(default)]` keeps pre-first-call state snapshots
    /// deserializable, and `None` honestly marks sessions whose history
    /// predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_call: Option<FirstCallUsage>,
}

/// State active while the agent is in plan mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlanModeState {
    /// The plan identifier associated with this plan-mode session.
    pub plan_id: String,
}

/// Tool-activation state derived from the tools/profile wire ops.
///
/// Mirrors the TS v2 kernel's `ActiveToolsModel` (`readonly string[] |
/// undefined` — `None` here means the unrestricted default where every
/// registered tool is active) and `UserToolModel` (name → registration).
/// The registrations are stored as raw JSON (the klient
/// `UserToolRegistration` shape, which carries `name`) so one bad record
/// can be skipped at restore time instead of failing the replay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolsState {
    /// The restricted active tool set. `None` = the unrestricted default
    /// (every registered tool active) — set by `tools.reset_active_tools`
    /// or when no restriction was ever applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Vec<String>>,
    /// User-tool registrations (`tools.register_user_tool`), keyed by their
    /// `name` field for replay upsert; `tools.unregister_user_tool` removes
    /// entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_registrations: Vec<serde_json::Value>,
    /// User tools explicitly disabled via `tools.set_user_tool_enabled`
    /// (enabled=false): the restore-time override that removes the names
    /// from the recreated agent's active set. A same-name re-registration
    /// clears the mark (a fresh registration resets visibility), and a
    /// whole-set replace/reset supersedes the per-name overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_user_tools: Vec<String>,
    /// Generic tool-store entries (`tools.update_store`), keyed by store
    /// key. Currently carries the session todo list (`"todo"`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub store: std::collections::HashMap<String, serde_json::Value>,
    /// Todo undo checkpoints (TS parity: the `Checkpointed` model's
    /// `checkpoints` — `conversationTime.ts`). One snapshot of the
    /// `"todo"` store value per real-user-input undo anchor appended to
    /// the conversation; `context.undo` restores the value at the popped
    /// depth, and a full compaction clears the stack.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todo_checkpoints: Vec<serde_json::Value>,
}

impl ToolsState {
    /// Whether the state carries no restriction and no registrations (the
    /// pre-P2d snapshot shape — such states are not serialized at all).
    fn is_default(&self) -> bool {
        self.active.is_none()
            && self.user_registrations.is_empty()
            && self.disabled_user_tools.is_empty()
            && self.store.is_empty()
            && self.todo_checkpoints.is_empty()
    }
}

/// One registered subagent identity (B4 T8, D-55) — the fold of a
/// `subagent.registered` wire op into [`AgentState::subagents`]. The
/// registry is the restart-time source of truth for which children a
/// session owns; the per-child journals carry their conversations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentRegistration {
    /// The child's agent id (`agent-<uuid>`).
    #[serde(rename = "agentId")]
    pub agent_id: String,
    /// The profile the child was spawned under.
    #[serde(rename = "profileName")]
    pub profile_name: String,
    /// The child's resolved model, when one was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The child's thinking-effort spelling, when one was recorded.
    #[serde(
        rename = "thinkingEffort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_effort: Option<String>,
    /// The dispatching parent's agent id (the main agent for a first-level
    /// child, another child for a nested one).
    #[serde(
        rename = "parentAgentId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_agent_id: Option<String>,
    /// The swarm item label the child was spawned under (`AgentSwarm`
    /// batches), when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
    /// The spawning tool call the member belongs to — the parent linkage the
    /// supervision snapshot roster keys on (supervision architecture §4.2).
    #[serde(
        rename = "parentToolCallId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_call_id: Option<String>,
    /// The member's index inside its swarm batch, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_index: Option<u32>,
}
