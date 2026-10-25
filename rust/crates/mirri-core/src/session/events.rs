//! Broadcast event bus for session lifecycle events.
//!
//! [`EventBus`] fans every [`SessionEvent`] out to all subscribers through a
//! `tokio::sync::broadcast` channel. Every session owns its own bus — created
//! as part of the session's
//! [`SessionArtifacts`](crate::session::artifacts::SessionArtifacts) — so a
//! subscriber only ever observes events from the session it subscribed to:
//!
//! - [`SessionManager`](crate::session::manager::SessionManager) publishes
//!   session-level events (`SessionResumed`, `SessionForked`,
//!   `SessionClosed`) on the affected session's bus.
//! - [`ApprovalGate`](crate::session::gates::ApprovalGate) and
//!   [`QuestionGate`](crate::session::gates::QuestionGate) publish the
//!   requests they register on their session's bus, so external responders
//!   (TUI, API handler) learn the `request_id` and can call back into
//!   `resolve` / `answer`.
//!
//! ## Backpressure model
//!
//! [`EventBus::publish`] never blocks. If a subscriber cannot keep up with
//! the event rate, its `recv()` calls observe [`RecvError::Lagged`] and the
//! events it missed are dropped — subscribers must treat `Lagged` as a resync
//! signal, not a fatal error. The channel closes (subscribers observe
//! [`RecvError::Closed`]) once every `Arc<EventBus>` is dropped.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::compaction::{CompactionResult, CompactionSource};
use crate::subagent::SuspendCause;
use crate::task::BackgroundTaskInfo;

/// Default buffer capacity for event buses created by
/// [`SessionManager`](crate::session::manager::SessionManager).
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 256;

/// A session lifecycle or gate event, broadcast to all subscribers.
///
/// Serialised as an internally-tagged JSON object (`#[serde(tag = "type")]`)
/// with snake_case variant names, mirroring the wire-event convention in
/// `mirri-proto`. Unknown `type` values deserialise to
/// [`SessionEvent::Unknown`] so event streams produced by newer components
/// can be consumed without crashing.
/// Default for [`SessionEvent::QuestionAsked::questions`]: an empty array, not
/// `null` — the projection and every client treat the field as a list.
fn empty_question_items() -> serde_json::Value {
    serde_json::Value::Array(Vec::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
// Event payloads are serialised across the wire as-is; boxing the large
// `CompactionCompleted` variant would churn every producer/consumer for a
// size-only concern, so the enum shape is intentionally kept.
#[allow(clippy::large_enum_variant)]
pub enum SessionEvent {
    /// A new agent was created within a session.
    #[serde(rename = "agent_created")]
    AgentCreated { agent_id: Uuid, session_id: Uuid },

    /// An agent's combined status snapshot changed — the structured
    /// `event.agent.status.updated` payload (CR4).
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other
    /// agent-scoped session event uses. `phase` is the legacy v1 phase
    /// derived from the activity fold
    /// ([`phase_from_activity`](crate::session::agent_status::phase_from_activity));
    /// the optional fields stay entirely absent from the serialised form
    /// when unset (never `null` keys). No `previousStatus`: the TS v2
    /// slice shape has no such key.
    #[serde(rename = "agent_status_updated")]
    AgentStatusUpdated {
        agent_id: String,
        /// The v1 phase (`idle`/`running`/`streaming`/…).
        phase: crate::session::agent_status::AgentPhase,
        /// The effective model alias.
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The effective reasoning-effort level: the rust spellings
        /// (`none`/`low`/`medium`/`high`) plus any model-declared effort
        /// string carried verbatim (e.g. `max`, `xhigh`).
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_effort: Option<crate::profile::ThinkingEffort>,
        /// Tokens currently held in the agent's context.
        #[serde(skip_serializing_if = "Option::is_none")]
        context_tokens: Option<u64>,
        /// The model's context window budget.
        #[serde(skip_serializing_if = "Option::is_none")]
        max_context_tokens: Option<u64>,
        /// Whether plan mode is active.
        #[serde(skip_serializing_if = "Option::is_none")]
        plan_mode: Option<bool>,
        /// Whether swarm mode is active.
        #[serde(skip_serializing_if = "Option::is_none")]
        swarm_mode: Option<bool>,
        /// The usage snapshot
        /// ([`AgentUsageService::status`](crate::usage::AgentUsageService::status)).
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<crate::usage::UsageStatus>,
    },

    /// An agent was removed from its session.
    #[serde(rename = "agent_removed")]
    AgentRemoved { agent_id: Uuid },

    /// A session was resumed from a persisted wire log.
    #[serde(rename = "session_resumed")]
    SessionResumed { session_id: Uuid },

    /// A new session was forked from an existing session's agent.
    #[serde(rename = "session_forked")]
    SessionForked {
        source_session_id: Uuid,
        new_session_id: Uuid,
    },

    /// A session was closed. All pending gate requests have been drained by
    /// the time this event is published.
    #[serde(rename = "session_closed")]
    SessionClosed { session_id: Uuid },

    /// A tool call needs human approval. External responders answer through
    /// [`ApprovalGate::resolve`](crate::session::gates::ApprovalGate::resolve)
    /// using the carried `request_id`.
    #[serde(rename = "tool_approval_requested")]
    ToolApprovalRequested {
        agent_id: Uuid,
        tool_name: String,
        request_id: Uuid,
    },

    /// A pending tool-approval request was resolved.
    #[serde(rename = "tool_approval_resolved")]
    ToolApprovalResolved { request_id: Uuid, approved: bool },

    /// A user question was asked. External responders answer through
    /// [`QuestionGate::answer`](crate::session::gates::QuestionGate::answer)
    /// using the carried `request_id`.
    ///
    /// `questions` is the full item array (the `AskUserQuestion` tool's
    /// `questions` input — `question`/`header`/`options`/`multi_select`).
    /// It rides the event rather than only the reverse-RPC ask so every
    /// consumer of the durable event — a second client, a reconnecting
    /// client replaying from its cursor — can render the real card instead
    /// of a question text with no options. Item and option ids are minted
    /// at projection time (`mirri_core::session::question_items`), never
    /// stored: the gate forwards the tool input verbatim.
    #[serde(rename = "question_asked")]
    QuestionAsked {
        agent_id: Uuid,
        /// The full question item array. Defaults to an empty array rather
        /// than `null` so a producer that omits the field still projects a
        /// well-formed (if empty) item list.
        #[serde(default = "empty_question_items")]
        questions: serde_json::Value,
        request_id: Uuid,
    },

    /// A pending user question was answered.
    #[serde(rename = "question_answered")]
    QuestionAnswered { request_id: Uuid, answer: String },

    /// A pending user question was dismissed (cancelled without an answer).
    /// The awaiting `ask` future resolves to `None`.
    #[serde(rename = "question_dismissed")]
    QuestionDismissed { request_id: Uuid },

    /// A message was appended to an agent's conversation context.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same spelling `session.snapshot` messages use —
    /// because consumers (web, ACP, SDK) route main-agent transcript frames
    /// apart from subagent-scoped ones by this id. The earlier `Uuid` field
    /// collapsed every non-UUID agent name onto `Uuid::nil()`, which is
    /// indistinguishable from "main" on the wire.
    #[serde(rename = "message_received")]
    MessageReceived {
        agent_id: String,
        role: String,
        content_summary: String,
    },

    /// An incremental assistant text chunk streamed live during an engine
    /// turn (a `LiveEvent::TextDelta` surfaced on the session's event bus by
    /// the embedded engine's live-event callback).
    ///
    /// Never recorded in context memory — it is a fire-and-forget UI
    /// notification, the Rust analogue of the ACP `agent_message_chunk`
    /// update. The final block-level assistant message still lands in
    /// context memory as usual.
    ///
    /// `agent_id` carries the agent's string identity (see
    /// [`SessionEvent::MessageReceived`]) so wire consumers can tell the
    /// main agent's streamed transcript from a subagent's.
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk { agent_id: String, text: String },

    /// An incremental reasoning ("thinking") chunk streamed live during an
    /// engine turn (a `LiveEvent::ThinkDelta` surfaced on the session's
    /// event bus by the live-event callback — the reasoning channel rides
    /// the same relay path the assistant text delta does).
    ///
    /// Never recorded in context memory — fire-and-forget UI notification,
    /// the volatile `event.thinking.delta` wire event. The final reasoning
    /// block still lands in context memory as usual. `agent_id` carries
    /// the agent's string identity (see [`SessionEvent::MessageReceived`]).
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { agent_id: String, text: String },

    /// An incremental tool-call argument fragment streamed live during an
    /// engine turn (a `LiveEvent::ToolCallPart` surfaced on the session's
    /// event bus by the live-event callback). `tool_call_id` is exactly the
    /// id the block-level `ToolCallStarted`/tool result events will carry for
    /// the same call.
    ///
    /// Never recorded in context memory — fire-and-forget UI notification,
    /// the volatile `event.tool.call.delta` wire event. The complete tool
    /// call still arrives as usual via the started/result pair.
    /// `agent_id` carries the agent's string identity (see
    /// [`SessionEvent::MessageReceived`]).
    #[serde(rename = "tool_call_delta")]
    ToolCallDelta {
        agent_id: String,
        tool_call_id: String,
        tool_name: Option<String>,
        arguments_part: String,
    },

    /// A background (fire-and-forget) engine turn failed before producing a
    /// [`TurnEndResult`](crate::TurnEndResult): it timed out or its task
    /// panicked. Published on the session's event bus by the `message.send`
    /// fire-mode task so subscribers learn the turn will never complete.
    ///
    /// `turn_id` is the UUID-backed turn id returned to the client by
    /// `message.send` (the `turn-<uuid>` string), not the engine's internal
    /// per-flow numeric counter.
    ///
    /// `error_code` is the engine error's domain code
    /// (`ErrorCode::as_str()` — `provider.api_error`, `context.overflow`, …)
    /// so the wire's structured error object reports the real failure class
    /// instead of a generic `internal`; `internal` covers bridge-level
    /// failures (panic, timeout) that have no engine error to ask.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.turn.ended` frames by it, so
    /// it must NOT be converted to a UUID: the main agent's `"main"` has no
    /// UUID, and a foreign id makes clients treat the frame as a subagent's.
    #[serde(rename = "turn_failed")]
    TurnFailed {
        agent_id: String,
        turn_id: String,
        error: String,
        error_code: String,
        /// The engine's numeric per-flow turn id — the `turn.ended` wire
        /// record's id and the fork-at-turn anchor. Carried so clients can
        /// rebind the turn's rows from the UUID-backed correlation id to
        /// the durable `turn-<id>` key without a snapshot refetch. `0`
        /// when the failure happened before an id was allocated.
        #[serde(default)]
        wire_turn_id: u64,
    },

    /// An engine turn started running: `message.send` accepted the user
    /// message and is about to invoke the LLM loop. Published on the
    /// session's event bus by the `message.send` dispatch before the engine
    /// turn runs, in both wait and fire modes.
    ///
    /// `turn_id` follows the [`SessionEvent::TurnFailed`] convention: the
    /// UUID-backed turn id (`turn-<uuid>`), not the engine's internal
    /// per-flow numeric counter.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.turn.started` frames by it, so
    /// it must NOT be converted to a UUID: the main agent's `"main"` has no
    /// UUID, and a foreign id makes clients treat the frame as a subagent's.
    #[serde(rename = "turn_started")]
    TurnStarted {
        agent_id: String,
        session_id: String,
        turn_id: String,
    },

    /// An engine turn completed successfully with a
    /// [`TurnEndResult`](crate::TurnEndResult). Published on the session's
    /// event bus by the `message.send` dispatch in both wait and fire modes
    /// — the success counterpart of [`SessionEvent::TurnFailed`], so a
    /// subscriber always observes exactly one of the two per started turn.
    ///
    /// `stop_reason` is the wire reason string the `turn.ended` wire-op
    /// record carries (`completed`, `cancelled`, `blocked`, `filtered`), see
    /// `turn_stop_reason_wire`.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.turn.ended` frames by it, so
    /// it must NOT be converted to a UUID: the main agent's `"main"` has no
    /// UUID, and a foreign id makes clients treat the frame as a subagent's.
    #[serde(rename = "turn_completed")]
    TurnCompleted {
        agent_id: String,
        session_id: String,
        turn_id: String,
        stop_reason: String,
        /// The engine's numeric per-flow turn id — the `turn.ended` wire
        /// record's id and the fork-at-turn anchor. Carried so clients can
        /// rebind the turn's rows from the UUID-backed correlation id to
        /// the durable `turn-<id>` key without a snapshot refetch. `0`
        /// when no id was allocated for the turn.
        #[serde(default)]
        wire_turn_id: u64,
        /// The server-measured wall-clock duration of the turn — the same
        /// value the `turn.ended` wire record's `durationMs` carries. Rides
        /// the projected `event.turn.ended` payload so a live completion —
        /// including one whose client resynced mid-turn and lost its local
        /// start anchor — renders the authoritative "Worked for X" instead
        /// of a naked "Worked". `None` when no duration was measured
        /// (engines that predate the field, or turns that never produced a
        /// `TurnEndResult`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },

    /// A tool call started executing within an engine turn. Published live
    /// (not after-the-fact) so the UI can show a "running tool" indicator.
    /// `args` is the full parsed argument object (the wire `tool.call.started`
    /// `args` — the TS protocol carries the same field).
    ///
    /// `agent_id` carries the emitting agent's string identity (`"main"` or
    /// `"agent-<uuid>"`) — the wire `tool.call.started` frame carries the
    /// same value as `agentId`, the field the web routes member tool frames by.
    #[serde(rename = "tool_call_started")]
    ToolCallStarted {
        agent_id: String,
        session_id: String,
        tool_name: String,
        tool_call_id: String,
        args: serde_json::Value,
    },

    /// A tool call completed (success or error). Published live so the UI
    /// can update the tool call card with the result status. `tool_name`
    /// rides along so the wire `tool.result` carries it (the TUI tool card
    /// context; a consumer that missed the started event still attributes
    /// the result to its tool), and `output` carries the result's output
    /// text — the wire `tool.result` output field the TS protocol carries
    /// (a live consumer renders the result without a snapshot reload).
    ///
    /// `agent_id` carries the emitting agent's string identity (`"main"` or
    /// `"agent-<uuid>"`) — the wire `tool.result` frame carries the same
    /// value as `agentId`, the field the web routes member tool frames by.
    #[serde(rename = "tool_call_completed")]
    ToolCallCompleted {
        agent_id: String,
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        output: crate::tool::ToolOutput,
    },

    /// An incremental update from a running tool call (streamed stdout,
    /// progress percent, status text, or a custom payload). Volatile: streamed
    /// live, never journaled or replayed — the protocol's `tool.progress`.
    ///
    /// `agent_id` carries the emitting agent's string identity (`"main"` or
    /// `"agent-<uuid>"`) — the wire `tool.progress` frame carries the same
    /// value as `agentId`, the field the web routes member tool frames by.
    #[serde(rename = "tool_progress")]
    ToolProgress {
        agent_id: String,
        tool_call_id: String,
        update: crate::tool::ToolUpdate,
    },

    /// A full context compaction started. Published by the compaction
    /// dispatch when a manual `session.beginCompaction` is accepted (the
    /// auto path publishes the same variant from inside the turn loop in a
    /// later batch), mirroring the TS `compaction.started` emit at begin.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.compaction.started` frames
    /// by it, so it must NOT be converted to a UUID: the main agent's
    /// `"main"` has no UUID, and a foreign id makes clients treat the frame
    /// as a subagent's.
    #[serde(rename = "compaction_started")]
    CompactionStarted {
        agent_id: String,
        /// What triggered the compaction (`manual` or `auto` on the wire).
        trigger: CompactionSource,
        /// The user's custom summarization instruction, when one was given.
        instruction: Option<String>,
    },

    /// An in-flight manual compaction was cancelled through
    /// `session.cancelCompaction` (TS parity: the cancel of an idle
    /// compaction state is silent — no event). The turn-internal auto
    /// paths publish the same variant when the turn's cancellation
    /// interrupts a compaction they are blocked on.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.compaction.cancelled` frames
    /// by it, so it must NOT be converted to a UUID: the main agent's
    /// `"main"` has no UUID, and a foreign id makes clients treat the frame
    /// as a subagent's.
    #[serde(rename = "compaction_cancelled")]
    CompactionCancelled { agent_id: String },

    /// A turn blocked waiting for an in-flight auto compaction (the
    /// threshold-triggered before-step compaction always blocks the step
    /// for its duration; the overflow-recovery path likewise). Published
    /// by the turn loop right before it awaits the summarizer. `turn_id`
    /// is absent on the wire: the engine-internal turn id is not the wire
    /// `turn-<uuid>` (that lives at the message-send dispatch and is not
    /// plumbed down to the turn loop), and the protocol type marks it
    /// optional.
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.compaction.blocked` frames
    /// by it, so it must NOT be converted to a UUID: the main agent's
    /// `"main"` has no UUID, and a foreign id makes clients treat the frame
    /// as a subagent's.
    #[serde(rename = "compaction_blocked")]
    CompactionBlocked {
        agent_id: String,
        /// Absent on the wire while the wire `turn-<uuid>` is not plumbed
        /// down to the turn loop (see the variant doc); never `null`.
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },

    /// A compaction completed and its summary was applied to memory. The
    /// payload mirrors the TS `CompactionResult` (the rust-only
    /// `context_summary` stays off the wire; the untracked kept* fields
    /// stay absent).
    ///
    /// `agent_id` carries the agent's string identity (`"main"`,
    /// `"agent-<uuid>"`) — the same wire vocabulary every other agent-scoped
    /// session event uses. Clients route `event.compaction.completed` frames
    /// by it, so it must NOT be converted to a UUID: the main agent's
    /// `"main"` has no UUID, and a foreign id makes clients treat the frame
    /// as a subagent's.
    #[serde(rename = "compaction_completed")]
    CompactionCompleted {
        agent_id: String,
        result: CompactionResult,
    },

    /// A UserPromptSubmit hook's result was rendered into the conversation
    /// context (a passing hook's output appended as a user message, or a
    /// blocking hook's message appended as an assistant message before the
    /// turn ends). Published by the turn loop at the UserPromptSubmit site —
    /// the only hook event TS publishes a result for. `blocked` rides the
    /// wire only for the block branch; `turn_id` follows the
    /// [`SessionEvent::CompactionBlocked`] convention (absent until the
    /// wire turn id is plumbed down to the turn loop).
    #[serde(rename = "hook_result")]
    HookResult {
        /// The hook event that produced the result (always
        /// `UserPromptSubmit` today).
        hook_event: String,
        /// The rendered display message (the TS `HookResult.message`
        /// chain: stderr for exit 2, structured message otherwise).
        content: String,
        /// Present (and `true`) only when a hook blocked the prompt.
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked: Option<bool>,
    },

    /// A background task was registered and is now running. Published by
    /// the [`TaskManager`](crate::task::TaskManager) on the session's event
    /// bus — the live bypass that lets subscribers (TUI, web) observe
    /// background work as it starts.
    #[serde(rename = "background_task_started")]
    BackgroundTaskStarted {
        /// The wire snapshot of the task (the TS `BackgroundTaskInfo`).
        info: BackgroundTaskInfo,
    },

    /// A background task reached a terminal status (completed, failed, or
    /// manager-stopped — the `info.status` string discriminates). Published
    /// exactly once per started task by the [`TaskManager`](crate::task::TaskManager).
    #[serde(rename = "background_task_terminated")]
    BackgroundTaskTerminated {
        /// The terminal wire snapshot of the task.
        info: BackgroundTaskInfo,
    },

    /// A user-initiated `!` shell command started running in the foreground
    /// (the rust analogue of the TS `shell.started` event). `task_id` is the
    /// id the command's foreground process is tracked under — the id a
    /// `task.detach` converts into a background task. Transient: streamed
    /// live, never journaled or replayed (the TS protocol's
    /// `VOLATILE_EVENT_TYPES` lists `shell.started`).
    #[serde(rename = "shell_started")]
    ShellStarted {
        /// The client-minted correlation id echoed on every `shell.*` event
        /// of this command.
        command_id: String,
        /// The foreground process's task id (stable across a detach — the
        /// detached background task keeps it).
        task_id: String,
    },

    /// A live stdout/stderr chunk from a user-initiated `!` shell command.
    /// Transient: the final output is recorded once as a
    /// `shell_command`-origin context message on completion (the TS protocol
    /// lists `shell.output` volatile; `command_id` lets consumers route
    /// chunks to the matching live entry and drop stale events).
    #[serde(rename = "shell_output")]
    ShellOutput {
        command_id: String,
        /// The streamed chunk (kind discriminates stdout/stderr).
        update: crate::tool::ToolUpdate,
        /// The foreground task id once the process registered — present on
        /// every chunk the runner emits (it mints the id up front).
        task_id: Option<String>,
    },

    /// A foreground `!` shell command settled (success or failure). A run
    /// detached to the background never emits this — it reports through the
    /// task lifecycle instead (the `background.task.*` events).
    #[serde(rename = "shell_completed")]
    ShellCompleted {
        command_id: String,
        is_error: bool,
        task_id: Option<String>,
    },

    /// A subagent was spawned by a tool call (`Agent` / `AgentSwarm`).
    /// Published by [`SessionSubagentHost`](crate::subagent::SessionSubagentHost)
    /// at spawn-acceptance time (synchronously, before the completion
    /// future runs) — the `subagent.spawned` wire event that drives the
    /// swarm UI's task rows.
    #[serde(rename = "subagent_spawned")]
    SubagentSpawned {
        /// The spawned child's agent id (`"agent-<uuid>"`).
        subagent_id: String,
        /// The profile name the child was spawned from.
        subagent_name: String,
        /// The short UI description the tool call carried.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether the tool call returned immediately (background) or
        /// awaits completion.
        run_in_background: bool,
        /// The resolved model alias the child will use.
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The parent agent's id (`None` when the host was built without
        /// one).
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<String>,
        /// The parent's tool call id (`Agent`/`AgentSwarm` execution
        /// context), when the spawn came from a tool call.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
        /// The swarm item's index (`AgentSwarm` batches), 0-based.
        #[serde(skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u32>,
    },

    /// A spawned subagent's completion future registered its child agent
    /// and is about to run its turn — the `subagent.started` wire event.
    #[serde(rename = "subagent_started")]
    SubagentStarted { subagent_id: String },

    /// A subagent was suspended (parked for a later retry or awaiting a
    /// user decision — ADR-011). Published by
    /// [`SessionSubagentHost`](crate::subagent::SessionSubagentHost) when
    /// the swarm scheduler parks a rate-limited member, and by the host's
    /// approval-gate hooks when a member's ask registers. The `reason` is a
    /// structured [`SuspendCause`] — `{ kind: approval | question |
    /// rate_limit, request_id?, tool_call_id? }` on the wire; the
    /// pre-structuring free-form string stays receivable via
    /// [`SuspendCause::Legacy`] for the compat window.
    #[serde(rename = "subagent_suspended")]
    SubagentSuspended {
        subagent_id: String,
        reason: SuspendCause,
    },

    /// A subagent finished its turn and produced a result — the
    /// `subagent.completed` wire event. `result_summary` is the child's
    /// final assistant text; the token counters mirror the TS
    /// `TokenUsage` wire object.
    #[serde(rename = "subagent_completed")]
    SubagentCompleted {
        subagent_id: String,
        result_summary: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    },

    /// A subagent's completion future failed (LLM construction, turn
    /// error, panic, or cancellation) — the `subagent.failed` wire event.
    #[serde(rename = "subagent_failed")]
    SubagentFailed { subagent_id: String, error: String },

    /// One LLM step of an engine turn is about to run — the
    /// `turn.step.started` wire event (the turn-flow `before_step` hook).
    /// `turn_id` is the wire turn id (`turn-<uuid>`) plumbed down from the
    /// message-send dispatch through [`TurnFlow::set_wire_turn_id`]; a
    /// standalone flow (tests, no dispatch) falls back to the engine's
    /// numeric per-flow counter stringified.
    #[serde(rename = "turn_step_started")]
    TurnStepStarted { turn_id: String, step: u32 },

    /// One LLM step of an engine turn finished — the
    /// `turn.step.completed` wire event (the turn-flow `after_step`
    /// hook), carrying the step's token usage. `turn_id` follows the
    /// [`SessionEvent::TurnStepStarted`] convention.
    #[serde(rename = "turn_step_completed")]
    TurnStepCompleted {
        turn_id: String,
        step: u32,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        /// The model alias the step ran under — the by-model key the
        /// usage was recorded against (the usage sink's `usage_model_key`
        /// handle, falling back to the turn-start `llm.model_name()` for
        /// standalone flows; TS parity N4). `None` = no attribution; the
        /// wire projection then omits the key rather than inventing one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    /// A running step was interrupted (turn aborted mid-step) — the
    /// `turn.step.interrupted` wire event. Emitted from the live-event
    /// relay (`LiveEvent::TurnInterrupted`); `turn_id` is the wire turn
    /// id the relay callback captured at dispatch (`None` only when the
    /// callback was built without one — the projection then omits it).
    #[serde(rename = "turn_step_interrupted")]
    TurnStepInterrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        step: u32,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// A failed step is being re-enqueued for a step-level retry — the
    /// `turn.step.retrying` wire event. Emitted from the live-event relay
    /// (`LiveEvent::StepRetrying`); `turn_id` follows the
    /// [`SessionEvent::TurnStepInterrupted`] convention.
    #[serde(rename = "turn_step_retrying")]
    TurnStepRetrying {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        step: u32,
        failed_attempt: u32,
        next_attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },

    /// The session's goal changed (created, paused, resumed, cancelled) —
    /// the `goal.updated` wire event. `snapshot` is the TS `GoalSnapshot`
    /// wire shape (`null`-valued JSON when the goal was cleared); `change`
    /// the TS `GoalChange` describing what happened.
    #[serde(rename = "goal_updated")]
    GoalUpdated {
        snapshot: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        change: Option<serde_json::Value>,
    },

    /// A skill was activated (user slash command via `agent.activateSkill`) —
    /// the `skill.activated` wire event.
    #[serde(rename = "skill_activated")]
    SkillActivated {
        activation_id: String,
        skill_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        trigger: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        skill_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        skill_source: Option<String>,
    },

    /// An MCP server's connection status changed — the
    /// `mcp.server.status` wire event. Emitted from the session overlay
    /// manager's status listeners, the pool status listeners, and the
    /// reconcile kernel's pool operations (M7, F-9).
    #[serde(rename = "mcp_server_status")]
    McpServerStatus {
        name: String,
        transport: String,
        status: String,
        tool_count: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Which pool owns the server (`user` / `workspace` / `plugin` for
        /// the pool listeners and the reconcile kernel, `session` for the
        /// per-session overlay) — the M7 payload extension, so app-level
        /// consumers can attribute every status to its source pool.
        pool: String,
        /// What kind of change the event reports (M7): `added` / `removed` /
        /// `reconfigured` / `field-change` from the reconcile kernel's pool
        /// operations (every persisted change emits, ADR-008), `status` from
        /// the pool listeners' connection transitions. `None` keeps the
        /// pre-M7 emitters (the session overlay) unchanged in shape.
        #[serde(skip_serializing_if = "Option::is_none")]
        change_kind: Option<String>,
    },

    /// Session metadata changed (title, tags) — the
    /// `session.meta.updated` wire event.
    #[serde(rename = "session_meta_updated")]
    SessionMetaUpdated {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// A raw camelCase patch of the changed metadata fields (the TS
        /// event's `patch` object).
        #[serde(skip_serializing_if = "Option::is_none")]
        patch: Option<serde_json::Value>,
    },

    /// The session's effective model changed (`agent.setModel` /
    /// `session.updateMeta` with a model that differs from the current one).
    /// `from` is the previously effective model id, `to` the new one —
    /// mid-turn switches apply from the next LLM call in the running turn.
    #[serde(rename = "model_switched")]
    ModelSwitched { from: String, to: String },

    /// A plugin command was activated — the `plugin_command.activated` wire
    /// event. The rust engine accepts the activation RPC without a session
    /// context, so no producer publishes this variant today; it exists so
    /// the projection vocabulary stays complete for consumers.
    #[serde(rename = "plugin_command_activated")]
    PluginCommandActivated {
        activation_id: String,
        plugin_id: String,
        command_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        command_args: Option<String>,
        trigger: String,
    },

    /// A due cron task fired — the `cron.fired` wire event. Published by
    /// [`CronManager::check_due`](crate::cron::CronManager::check_due) in
    /// its fire pass, before the driver injects the prompt.
    #[serde(rename = "cron_fired")]
    CronFired {
        task_id: String,
        cron: String,
        prompt: String,
        recurring: bool,
        /// How many overdue periods this single fire merged (`>= 1`; the
        /// TS `CronJobOrigin.coalescedCount`).
        coalesced_count: usize,
        /// The task crossed the 7-day stale threshold — its final delivery.
        stale: bool,
    },

    /// The session's work-facts tuple transitioned (US4.4 / SDR-013): the
    /// activity fold published a real state change. Durable, global fan-out
    /// on the serve wire (protocol §2.3 exception) — list rows update
    /// without a per-session subscription. `sessionId` is not carried here:
    /// the relays inject it from their session context (same as
    /// [`InFlightTurnShadow`](crate::session::in_flight_shadow) events).
    #[serde(rename = "session_work_changed")]
    SessionWorkChanged {
        state: crate::session_activity::SessionActivityState,
    },

    /// Catch-all for event types this kernel does not yet recognise. Must
    /// stay the last variant; all other JSON fields are silently dropped.
    #[serde(other)]
    Unknown,
}

/// Broadcast channel distributing [`SessionEvent`]s to all subscribers.
///
/// Cheap to clone (internally an `Arc`); sharing an [`EventBus`] through
/// `Arc<EventBus>` is the norm so that the channel is only closed when the
/// whole session is torn down.
#[derive(Debug)]
pub struct EventBus {
    sender: broadcast::Sender<SessionEvent>,
}

impl EventBus {
    /// Create a new event bus with the given buffer capacity.
    ///
    /// The capacity bounds how many events a slow subscriber can miss before
    /// it starts receiving [`RecvError::Lagged`].
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Subscribe to all future events.
    ///
    /// Events published *before* this call are not replayed; the receiver
    /// starts at the current position in the stream.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.sender.subscribe()
    }

    /// Publish an event to every subscriber (non-blocking).
    ///
    /// When no subscriber is listening the event is dropped and a debug log
    /// is emitted. When a slow subscriber's buffer overflows the subscriber
    /// observes [`RecvError::Lagged`] and the missed events are lost.
    pub fn publish(&self, event: SessionEvent) {
        if let Err(err) = self.sender.send(event) {
            tracing::debug!("session event dropped (no subscribers): {err}");
        }
    }

    /// Number of currently subscribed receivers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// Best-effort extraction of the UUID embedded in a scope-level identifier.
///
/// Scope IDs are generated as `"<kind>-<uuid>"` (e.g. `"session-<uuid>"` or
/// `"agent-<uuid>"`); a bare UUID string is also accepted. Returns
/// [`Uuid::nil`] when the string carries no parseable UUID — callers that
/// cannot tolerate a nil UUID should map the identifier themselves.
pub fn uuid_from_id(scope_id: &str) -> Uuid {
    if let Ok(uuid) = Uuid::parse_str(scope_id) {
        return uuid;
    }
    scope_id
        .split_once('-')
        .and_then(|(_, rest)| Uuid::parse_str(rest).ok())
        .unwrap_or_else(Uuid::nil)
}
