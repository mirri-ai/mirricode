//! Project [`SessionEvent`]s onto the unified protocol's `event.*`
//! JSON-RPC notification shapes — the single source of truth shared by
//! every Rust backend (mirri-server's WebSocket RPC today; the embedded
//! relay switches onto it in the event-unification b3 batch).
//!
//! Every [`SessionEvent`] variant maps to one protocol event method plus a
//! flat camelCase payload object; [`SessionEvent::Unknown`] has no wire
//! projection and [`project`] returns `None` for it (the transport silently
//! drops such events). Volatility — whether an event is a fire-and-forget
//! stream notification versus a durable, cursor-replayable event — is
//! exposed on [`ProjectedEvent`] and through [`is_volatile`], which the
//! backend event channels use to bypass the durable log.
//!
//! # Envelope and reserved keys
//!
//! [`event_notification`] wraps a projection in the transport envelope:
//! `sessionId`, `volatile` and `timestamp` are always present; `seq` and
//! `epoch` are present if and only if the event is durable and the caller
//! supplied them. The payload keys are merged flat into the envelope's
//! `params` (payload inserted after the envelope keys), so payload objects
//! must never use the reserved envelope keys: `sessionId`, `volatile`,
//! `timestamp`, `seq`, `epoch`.
//!
//! # Transport neutrality
//!
//! The envelope builder returns a plain method + `params` pair
//! ([`ProjectedNotification`]) rather than a `mirri-transport` frame:
//! core never depends on transport, and each backend frames the
//! notification itself.
//!
//! # Embedded seq/epoch semantics
//!
//! Both backends stamp their durable events with a `seq` source (the
//! server's per-session ring buffer today); the embedded backend's
//! seq/epoch stamping lands with the b3 batch of the event unification.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::SessionEvent;
use crate::compaction::CompactionSource;
use crate::loop_engine::TurnStopReason;
use crate::profile::ThinkingEffort;
use crate::session::agent_status::AgentPhase;
use crate::usage::{TokenUsage, UsageStatus};

/// The closed vocabulary of projected `event.*` methods — every wire method
/// [`project`] can produce, sorted. This is the list the TS contract suite
/// reconciles against (exposed through the NAPI `embedded_event_methods`
/// export), so a projection method added or renamed in Rust trips the TS-side
/// test instead of silently drifting from the contract.
pub const PROJECTED_EVENT_METHODS: [&str; 46] = [
    "event.agent.created",
    "event.agent.removed",
    "event.agent.status.updated",
    "event.approval.requested",
    "event.approval.resolved",
    "event.assistant.delta",
    "event.background.task.started",
    "event.background.task.terminated",
    "event.compaction.blocked",
    "event.compaction.cancelled",
    "event.compaction.completed",
    "event.compaction.started",
    "event.cron.fired",
    "event.goal.updated",
    "event.hook.result",
    "event.mcp.server.status",
    "event.message.received",
    "event.plugin_command.activated",
    "event.question.answered",
    "event.question.dismissed",
    "event.question.requested",
    "event.session.deleted",
    "event.session.meta.updated",
    "event.session.model_switched",
    "event.session.updated",
    "event.session.work_changed",
    "event.shell.completed",
    "event.shell.output",
    "event.shell.started",
    "event.skill.activated",
    "event.subagent.completed",
    "event.subagent.failed",
    "event.subagent.spawned",
    "event.subagent.started",
    "event.subagent.suspended",
    "event.thinking.delta",
    "event.tool.call.delta",
    "event.tool.call.started",
    "event.tool.progress",
    "event.tool.result",
    "event.turn.ended",
    "event.turn.started",
    "event.turn.step.completed",
    "event.turn.step.interrupted",
    "event.turn.step.retrying",
    "event.turn.step.started",
];

/// The closed vocabulary of projected **server-scoped** `event.*` methods —
/// the workspace lifecycle events a [`ServerEvent`] produces. Kept separate
/// from [`PROJECTED_EVENT_METHODS`] (the session-scoped vocabulary) because
/// server events are not [`SessionEvent`]s: they carry no session envelope
/// (`sessionId` is the synthesized empty string), no per-session seq, and
/// are never replayed through a session cursor. Both vocabularies together
/// are the `event.*` surface the TS contract suite reconciles against (the
/// NAPI `embedded_event_methods` export unions them).
pub const SERVER_PROJECTED_EVENT_METHODS: [&str; 3] = [
    "event.workspace.created",
    "event.workspace.deleted",
    "event.workspace.updated",
];

/// One workspace's identity snapshot — the payload of the created/updated
/// server events (id + root + the optional display name; a full
/// `WorkspaceView`-shaped object is the RPC result's job, not the event's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    /// The workspace key (`wd_<slug>_<hash12>`).
    pub id: String,
    /// The workspace root directory.
    pub root: String,
    /// The display name, when one was registered.
    pub name: Option<String>,
}

/// A server-scoped lifecycle event — workspace registry changes, not
/// session activity. Emitted by the [`EmbeddedServer`](crate::EmbeddedServer)
/// after a registry mutation lands, broadcast to every event subscriber
/// (and to the `EngineAdapter::subscribe_events` channel) as an
/// `event.workspace.*` notification.
///
/// Deliberately NOT a [`SessionEvent`]: no session is involved, so these
/// events must not pollute the per-session relay/seq machinery (design
/// `08-multi-workspace.md` §T2 structure decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    /// A workspace was registered for the first time.
    WorkspaceCreated { workspace: WorkspaceSnapshot },
    /// A registered workspace's metadata changed (a touch or a rename).
    WorkspaceUpdated { workspace: WorkspaceSnapshot },
    /// A workspace was unregistered.
    WorkspaceDeleted { workspace_id: String, root: String },
}

impl ServerEvent {
    /// The wire method this event projects to.
    pub fn method(&self) -> &'static str {
        match self {
            ServerEvent::WorkspaceCreated { .. } => "event.workspace.created",
            ServerEvent::WorkspaceUpdated { .. } => "event.workspace.updated",
            ServerEvent::WorkspaceDeleted { .. } => "event.workspace.deleted",
        }
    }
}

/// Project a [`ServerEvent`] onto the transport envelope, ready to be framed
/// as a JSON-RPC notification.
///
/// The envelope mirrors the session projection's shape minus the cursor
/// fields: `sessionId` is always the **empty string** (the web
/// `WireEventBase` requires the field, but no session is involved) and
/// `volatile` is `true` (server-scope events are fire-and-forget — no
/// per-session seq/epoch exists for them). The type-specific payload is
/// merged flat into `params` exactly like the session projection.
pub fn project_server_event(event: &ServerEvent) -> ProjectedNotification {
    let (method, payload) = match event {
        ServerEvent::WorkspaceCreated { workspace }
        | ServerEvent::WorkspaceUpdated { workspace } => (
            event.method(),
            json!({ "workspace": snapshot_json(workspace) }),
        ),
        ServerEvent::WorkspaceDeleted { workspace_id, root } => (
            event.method(),
            json!({ "workspaceId": workspace_id, "root": root }),
        ),
    };
    let mut params = serde_json::Map::new();
    params.insert("sessionId".to_string(), json!(""));
    params.insert("volatile".to_string(), json!(true));
    params.insert("timestamp".to_string(), json!(Utc::now().to_rfc3339()));
    if let Value::Object(payload) = payload {
        for (key, value) in payload {
            params.insert(key, value);
        }
    }
    ProjectedNotification {
        method: method.to_string(),
        params: Value::Object(params),
    }
}

/// The workspace snapshot payload shape — `name` is absent (never null)
/// when the workspace has none, the same absent-not-null rule as the
/// session projection's optional fields.
fn snapshot_json(workspace: &WorkspaceSnapshot) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_string(), json!(workspace.id));
    object.insert("root".to_string(), json!(workspace.root));
    if let Some(name) = &workspace.name {
        object.insert("name".to_string(), json!(name));
    }
    Value::Object(object)
}

/// One [`SessionEvent`] projected onto the protocol's `event.*` wire shape:
/// the notification method plus the payload fields to merge into the
/// notification's `params` (the envelope builder adds the `sessionId`/
/// `seq`/`epoch`/`volatile`/`timestamp` envelope fields around them).
pub struct ProjectedEvent {
    /// Protocol method, e.g. `event.message.received`.
    pub method: &'static str,
    /// Whether the event is fire-and-forget (streamed live, never replayed
    /// via cursor): `true` for `event.assistant.delta`,
    /// `event.thinking.delta`, and `event.agent.status.updated`.
    pub volatile: bool,
    /// Type-specific payload fields (camelCase), merged into the frame's
    /// `params` by the envelope builder. Never uses the reserved envelope
    /// keys (`sessionId`, `volatile`, `timestamp`, `seq`, `epoch`).
    pub payload: Value,
}

/// Map a [`SessionEvent`] to its protocol `event.*` notification, or `None`
/// for events with no wire projection ([`SessionEvent::Unknown`]).
pub fn project(event: &SessionEvent) -> Option<ProjectedEvent> {
    let (method, volatile, payload) = match event {
        SessionEvent::MessageReceived {
            agent_id,
            role,
            content_summary,
        } => (
            "event.message.received",
            false,
            json!({
                "agentId": agent_id,
                "role": role,
                "contentSummary": content_summary,
            }),
        ),
        SessionEvent::AgentMessageChunk { agent_id, text } => (
            "event.assistant.delta",
            true,
            json!({
                "agentId": agent_id,
                "delta": text,
            }),
        ),
        SessionEvent::ThinkingDelta { agent_id, text } => (
            "event.thinking.delta",
            true,
            json!({
                "agentId": agent_id,
                "delta": text,
            }),
        ),
        SessionEvent::ToolCallDelta {
            agent_id,
            tool_call_id,
            tool_name,
            arguments_part,
        } => {
            // Volatile (protocol `VOLATILE_EVENT_TYPES` lists
            // `tool.call.delta`): streamed live, no seq stamp, never
            // replayed. The wire turn id is absent — the live callback
            // carries none, matching the `tool.call.started` /
            // `tool.progress` adaptations. `name` is optional on the wire
            // (protocol `z.string().optional()`): present when the provider
            // part carried the identity, absent otherwise — never null.
            let mut payload = json!({
                "agentId": agent_id,
                "toolCallId": tool_call_id,
                "argumentsPart": arguments_part,
            });
            if let Some(name) = tool_name {
                payload["name"] = json!(name);
            }
            ("event.tool.call.delta", true, payload)
        }
        SessionEvent::TurnStarted {
            agent_id,
            session_id: _,
            turn_id,
        } => (
            "event.turn.started",
            false,
            json!({ "agentId": agent_id, "turnId": turn_id }),
        ),
        SessionEvent::TurnCompleted {
            agent_id,
            session_id: _,
            turn_id,
            stop_reason,
            wire_turn_id,
            duration_ms,
        } => {
            // The server-measured duration rides the payload so a live
            // completion renders the same "Worked for X" the wire record
            // carries — including after a resync dropped the client's own
            // start anchor. Absent (never null) when the turn carried none.
            let mut payload = json!({
                "agentId": agent_id,
                "turnId": turn_id,
                "reason": stop_reason,
                "wireTurnId": wire_turn_id,
            });
            if let Some(duration_ms) = duration_ms {
                payload["durationMs"] = json!(duration_ms);
            }
            ("event.turn.ended", false, payload)
        }
        SessionEvent::TurnFailed {
            agent_id,
            turn_id,
            error,
            error_code,
            wire_turn_id,
        } => {
            // ADR-007: a user-initiated hard abort (the exact code the
            // message.abort / goal.pause / goal.cancel dispatches publish)
            // reports `cancelled` — the vocabulary is the attribution, and
            // a user cancellation must not render as an engine failure
            // downstream. EXACT match, not a prefix: `abort.cancelled` is
            // the system-abort code and stays in the failed vocabulary.
            // The structured error payload is retained for diagnostics.
            let reason = if error_code == "abort.user_cancelled" {
                "cancelled"
            } else {
                "failed"
            };
            // Failure folds into the same terminal method as success so
            // subscribers observe exactly one `event.turn.ended` per
            // started turn, discriminated by `reason` plus the structured
            // error object. The object carries the engine error's real
            // domain code and its retryability verdict (the shared
            // `domain_code_retryable` table — the TS KimiErrorPayload
            // contract carries the same `retryable` field).
            (
                "event.turn.ended",
                false,
                json!({
                    "agentId": agent_id,
                    "turnId": turn_id,
                    "reason": reason,
                    "wireTurnId": wire_turn_id,
                    "error": {
                        "code": error_code,
                        "message": error,
                        "retryable": mirri_proto::rpc::domain_code_retryable(error_code),
                    },
                }),
            )
        }
        SessionEvent::ToolCallStarted {
            agent_id,
            tool_name,
            tool_call_id,
            args,
            ..
        } => (
            "event.tool.call.started",
            false,
            json!({
                "agentId": agent_id,
                "toolCallId": tool_call_id,
                "name": tool_name,
                // The full parsed argument object — the same field the TS
                // protocol's `tool.call.started` carries (`args: unknown`);
                // TS emits the untruncated object, so the wire does too.
                "args": args,
            }),
        ),
        SessionEvent::ToolCallCompleted {
            agent_id,
            session_id: _,
            tool_call_id,
            tool_name,
            is_error,
            output,
        } => (
            "event.tool.result",
            false,
            json!({
                "agentId": agent_id,
                "toolCallId": tool_call_id,
                // The tool's name rides on the result so a consumer that
                // missed the started event (or joined mid-stream) still
                // attributes the result to its tool — the TUI tool-card
                // context (the TS protocol gained the optional `name` on
                // `tool.result` for this dialect).
                "name": tool_name,
                "isError": is_error,
                // The result's output — the same field the TS protocol's
                // `tool.result` carries (text string or content-part array),
                // so a live consumer renders the result content without
                // waiting for a snapshot reload.
                "output": output,
            }),
        ),
        SessionEvent::ToolProgress {
            agent_id,
            tool_call_id,
            update,
        } => (
            // Volatile (protocol `VOLATILE_EVENT_TYPES` lists `tool.progress`):
            // streamed live, no seq stamp, never replayed. The wire turn id is
            // absent — the live callback carries none, matching the
            // `tool.call.started` / `tool.result` adaptations.
            "event.tool.progress",
            true,
            json!({
                "agentId": agent_id,
                "toolCallId": tool_call_id,
                "update": update,
            }),
        ),
        SessionEvent::AgentStatusUpdated {
            agent_id,
            phase,
            model,
            thinking_effort,
            context_tokens,
            max_context_tokens,
            plan_mode,
            swarm_mode,
            usage,
        } => {
            // The structured agent status payload (CR4) — key-by-key the
            // protocol `AgentStatusUpdatedEvent`. Built through the
            // camelCase intermediate struct below so an absent optional
            // field is entirely absent from the JSON (never a null key).
            // `contextUsage` = contextTokens / maxContextTokens as f64,
            // absent when either side is. No `previousStatus` — the TS
            // v2 slice shape has no such key.
            let payload = AgentStatusPayload {
                agent_id,
                phase: *phase,
                model: model.as_deref(),
                thinking_effort: thinking_effort.clone(),
                context_tokens: *context_tokens,
                max_context_tokens: *max_context_tokens,
                context_usage: match (context_tokens, max_context_tokens) {
                    (Some(context), Some(max)) => Some(*context as f64 / *max as f64),
                    _ => None,
                },
                plan_mode: *plan_mode,
                swarm_mode: *swarm_mode,
                usage: usage.as_ref().map(WireUsageStatus::from),
            };
            (
                "event.agent.status.updated",
                true,
                serde_json::to_value(payload).expect("plain data payload serializes"),
            )
        }
        SessionEvent::ToolApprovalRequested {
            agent_id,
            tool_name,
            request_id,
        } => (
            "event.approval.requested",
            false,
            json!({
                "agentId": agent_id,
                "requestId": request_id,
                "toolName": tool_name,
            }),
        ),
        SessionEvent::ToolApprovalResolved {
            request_id,
            approved,
        } => (
            "event.approval.resolved",
            false,
            json!({
                "requestId": request_id,
                "approved": approved,
            }),
        ),
        SessionEvent::QuestionAsked {
            agent_id,
            questions,
            request_id,
        } => (
            "event.question.requested",
            false,
            json!({
                "agentId": agent_id,
                "requestId": request_id,
                // The first item's text stays as a summary for consumers
                // that only need to announce the ask (the rust CLI's print
                // mode). The full items ride `questions` so every client
                // can render the real card from the durable event alone.
                "question": crate::session::gates::question_texts(questions)
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
                "questions": crate::session::question_items::mint_question_item_ids(questions),
            }),
        ),
        SessionEvent::QuestionAnswered { request_id, answer } => (
            "event.question.answered",
            false,
            json!({
                "requestId": request_id,
                "answer": answer,
            }),
        ),
        SessionEvent::QuestionDismissed { request_id } => (
            "event.question.dismissed",
            false,
            json!({ "requestId": request_id }),
        ),
        SessionEvent::SessionResumed { .. } => (
            "event.session.updated",
            false,
            json!({ "change": "resumed" }),
        ),
        SessionEvent::CompactionStarted {
            agent_id,
            trigger,
            instruction,
        } => {
            // The absent instruction stays absent — the protocol type
            // marks it optional, no null placeholder.
            let mut payload = json!({
                "agentId": agent_id,
                "trigger": match trigger {
                    CompactionSource::Manual => "manual",
                    CompactionSource::Auto => "auto",
                },
            });
            if let Some(instruction) = instruction {
                payload["instruction"] = json!(instruction);
            }
            ("event.compaction.started", false, payload)
        }
        SessionEvent::CompactionCancelled { agent_id } => (
            "event.compaction.cancelled",
            false,
            json!({ "agentId": agent_id }),
        ),
        SessionEvent::CompactionBlocked { agent_id, turn_id } => {
            // The engine does not plumb the wire turn id down to the turn
            // loop, so the optional turnId stays absent (never a null
            // placeholder, never the engine's internal counter).
            let mut payload = json!({ "agentId": agent_id });
            if let Some(turn_id) = turn_id {
                payload["turnId"] = json!(turn_id);
            }
            ("event.compaction.blocked", false, payload)
        }
        SessionEvent::HookResult {
            hook_event,
            content,
            blocked,
        } => {
            // The optional blocked flag stays absent on the passing branch —
            // the protocol type marks it optional, no null placeholder.
            let mut payload = json!({
                "hookEvent": hook_event,
                "content": content,
            });
            if let Some(blocked) = blocked {
                payload["blocked"] = json!(blocked);
            }
            ("event.hook.result", false, payload)
        }
        SessionEvent::BackgroundTaskStarted { info } => (
            "event.background.task.started",
            false,
            json!({ "info": info }),
        ),
        SessionEvent::BackgroundTaskTerminated { info } => (
            "event.background.task.terminated",
            false,
            json!({ "info": info }),
        ),
        SessionEvent::ShellStarted {
            command_id,
            task_id,
        } => (
            // Volatile (protocol `VOLATILE_EVENT_TYPES` lists `shell.started`):
            // streamed live, no seq stamp, never replayed.
            "event.shell.started",
            true,
            json!({
                "commandId": command_id,
                "taskId": task_id,
            }),
        ),
        SessionEvent::ShellOutput {
            command_id,
            update,
            task_id,
        } => {
            // Volatile (protocol `VOLATILE_EVENT_TYPES` lists `shell.output`):
            // the final output is recorded once as a `shell_command`-origin
            // context message on completion, so the live chunk never occupies
            // cursor space. The optional `taskId` stays absent (never a null
            // placeholder) when the run carries no task id.
            let mut payload = json!({
                "commandId": command_id,
                "update": update,
            });
            if let Some(task_id) = task_id {
                payload["taskId"] = json!(task_id);
            }
            ("event.shell.output", true, payload)
        }
        SessionEvent::ShellCompleted {
            command_id,
            is_error,
            task_id,
        } => {
            let mut payload = json!({
                "commandId": command_id,
                "isError": is_error,
            });
            if let Some(task_id) = task_id {
                payload["taskId"] = json!(task_id);
            }
            ("event.shell.completed", false, payload)
        }
        SessionEvent::CompactionCompleted { agent_id, result } => {
            // The optional ADR-007 provenance fields stay absent when the
            // apply never materialized a replaced prefix (refusal paths) —
            // never null placeholders. `droppedEntrySeqCount` mirrors the
            // `droppedCount` pattern (a count, never the list itself) and is
            // likewise absent when no entries were dropped.
            let mut compaction_result = json!({
                "summary": result.summary,
                "compactedCount": result.compacted_count,
                "tokensBefore": result.tokens_before,
                "tokensAfter": result.tokens_after,
                "droppedCount": result.dropped_count,
                // The rust-only `context_summary` (the compaction
                // handoff prefix) stays off the wire — the protocol's
                // CompactionResult has no such field. The kept*
                // optional fields stay absent too: the engine does
                // not track them.
            });
            if let Some(first) = result.first_replaced_entry_seq {
                compaction_result["firstReplacedEntrySeq"] = json!(first);
            }
            if let Some(last) = result.last_replaced_entry_seq {
                compaction_result["lastReplacedEntrySeq"] = json!(last);
            }
            if !result.dropped_entry_seqs.is_empty() {
                compaction_result["droppedEntrySeqCount"] = json!(result.dropped_entry_seqs.len());
            }
            (
                "event.compaction.completed",
                false,
                json!({ "agentId": agent_id, "result": compaction_result }),
            )
        }
        SessionEvent::SessionForked {
            source_session_id,
            new_session_id,
        } => (
            "event.session.updated",
            false,
            json!({
                "change": "forked",
                "sourceSessionId": source_session_id,
                // `newSessionId` (not `sessionId`) — the envelope already
                // carries the session the notification is published on, so
                // a payload key of the same name would collide with it.
                "newSessionId": new_session_id,
            }),
        ),
        SessionEvent::SessionClosed { .. } => (
            "event.session.deleted",
            false,
            // Hardcoded `user_initiated`: `SessionEvent::SessionClosed` is
            // only published on user-initiated closes today; widening the
            // reason vocabulary is tracked as design follow-up F3.
            json!({ "reason": "user_initiated" }),
        ),
        SessionEvent::AgentCreated { agent_id, .. } => {
            ("event.agent.created", false, json!({ "agentId": agent_id }))
        }
        SessionEvent::AgentRemoved { agent_id } => {
            ("event.agent.removed", false, json!({ "agentId": agent_id }))
        }
        SessionEvent::SubagentSpawned {
            subagent_id,
            subagent_name,
            description,
            run_in_background,
            model,
            parent_agent_id,
            parent_tool_call_id,
            swarm_index,
        } => {
            // Mirrors the TS `SubagentSpawnedEvent` (the protocol schema's
            // `parentToolCallId` is required there; the rust host knows it
            // only when the spawn came from a tool call, so the optional
            // fields stay absent — never null placeholders).
            let mut payload = json!({
                "subagentId": subagent_id,
                "subagentName": subagent_name,
                "runInBackground": run_in_background,
            });
            if let Some(description) = description {
                payload["description"] = json!(description);
            }
            if let Some(model) = model {
                payload["model"] = json!(model);
            }
            if let Some(parent_agent_id) = parent_agent_id {
                payload["parentAgentId"] = json!(parent_agent_id);
            }
            if let Some(parent_tool_call_id) = parent_tool_call_id {
                payload["parentToolCallId"] = json!(parent_tool_call_id);
            }
            if let Some(swarm_index) = swarm_index {
                payload["swarmIndex"] = json!(swarm_index);
            }
            ("event.subagent.spawned", false, payload)
        }
        SessionEvent::SubagentStarted { subagent_id } => (
            "event.subagent.started",
            false,
            json!({ "subagentId": subagent_id }),
        ),
        SessionEvent::SubagentSuspended {
            subagent_id,
            reason,
        } => {
            // ADR-011: the reason is the structured snake_case object
            // `{ kind: "approval" | "question" | "rate_limit", request_id?,
            // tool_call_id? }`. The legacy pre-structuring string stays
            // receivable — `SuspendCause::Legacy` serializes as a plain
            // string, so the wire keeps both shapes and consumers
            // discriminate with `typeof`.
            let reason = serde_json::to_value(reason).unwrap_or_else(|_| json!(reason.to_string()));
            (
                "event.subagent.suspended",
                false,
                json!({
                    "subagentId": subagent_id,
                    "reason": reason,
                }),
            )
        }
        SessionEvent::SubagentCompleted {
            subagent_id,
            result_summary,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        } => (
            "event.subagent.completed",
            false,
            json!({
                "subagentId": subagent_id,
                "resultSummary": result_summary,
                "usage": wire_token_usage(*input_tokens, *output_tokens, *cache_read_tokens, *cache_creation_tokens),
            }),
        ),
        SessionEvent::SubagentFailed { subagent_id, error } => (
            "event.subagent.failed",
            false,
            json!({
                "subagentId": subagent_id,
                "error": error,
            }),
        ),
        SessionEvent::TurnStepStarted { turn_id, step } => (
            "event.turn.step.started",
            false,
            json!({
                // The wire turn id (`turn-<uuid>`) — the same spelling
                // `event.turn.started` carries, plumbed down from the
                // message-send dispatch through `set_wire_turn_id`.
                "turnId": turn_id,
                "step": step,
            }),
        ),
        SessionEvent::TurnStepCompleted {
            turn_id,
            step,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            model,
        } => {
            // The step's usage-attribution model (the alias the usage sink
            // recorded under). Optional on the wire: present when the flow
            // attributed the step's usage to a model, absent otherwise —
            // never null (the same optional-payload pattern `ToolCallDelta`
            // uses for `name`).
            let mut payload = json!({
                "turnId": turn_id,
                "step": step,
                "usage": wire_token_usage(*input_tokens, *output_tokens, *cache_read_tokens, *cache_creation_tokens),
            });
            if let Some(model) = model {
                payload["model"] = json!(model);
            }
            ("event.turn.step.completed", false, payload)
        }
        SessionEvent::TurnStepInterrupted {
            turn_id,
            step,
            reason,
            message,
        } => {
            // The live-event relay callback captured the wire turn id at
            // dispatch, so the protocol-required `turnId` is present on
            // every embedded path (it stays absent only when the callback
            // was built without one).
            let mut payload = json!({
                "step": step,
                "reason": reason,
            });
            if let Some(turn_id) = turn_id {
                payload["turnId"] = json!(turn_id);
            }
            if let Some(message) = message {
                payload["message"] = json!(message);
            }
            ("event.turn.step.interrupted", false, payload)
        }
        SessionEvent::TurnStepRetrying {
            turn_id,
            step,
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
        } => {
            let mut payload = json!({
                "step": step,
                "failedAttempt": failed_attempt,
                "nextAttempt": next_attempt,
                "maxAttempts": max_attempts,
                "delayMs": delay_ms,
            });
            if let Some(turn_id) = turn_id {
                payload["turnId"] = json!(turn_id);
            }
            ("event.turn.step.retrying", false, payload)
        }
        SessionEvent::GoalUpdated { snapshot, change } => {
            let mut payload = json!({ "snapshot": snapshot });
            if let Some(change) = change {
                payload["change"] = json!(change);
            }
            ("event.goal.updated", false, payload)
        }
        SessionEvent::SkillActivated {
            activation_id,
            skill_name,
            args,
            trigger,
            skill_path,
            skill_source,
        } => {
            let mut payload = json!({
                "activationId": activation_id,
                "skillName": skill_name,
                "trigger": trigger,
            });
            if let Some(args) = args {
                payload["args"] = json!(args);
            }
            if let Some(skill_path) = skill_path {
                payload["skillPath"] = json!(skill_path);
            }
            if let Some(skill_source) = skill_source {
                payload["skillSource"] = json!(skill_source);
            }
            ("event.skill.activated", false, payload)
        }
        SessionEvent::McpServerStatus {
            name,
            transport,
            status,
            tool_count,
            error,
            pool,
            change_kind,
        } => {
            let mut server = json!({
                "name": name,
                "transport": transport,
                "status": status,
                "toolCount": tool_count,
                "pool": pool,
            });
            if let Some(error) = error {
                server["error"] = json!(error);
            }
            if let Some(change_kind) = change_kind {
                server["changeKind"] = json!(change_kind);
            }
            let payload = json!({ "server": server });
            ("event.mcp.server.status", false, payload)
        }
        SessionEvent::SessionMetaUpdated { title, patch } => {
            let mut payload = json!({});
            if let Some(title) = title {
                payload["title"] = json!(title);
            }
            if let Some(patch) = patch {
                payload["patch"] = json!(patch);
            }
            ("event.session.meta.updated", false, payload)
        }
        SessionEvent::ModelSwitched { from, to } => (
            // Durable: the switch is a cursor-replayable lifecycle event, so
            // the envelope stamps seq/epoch and future replays observe it.
            "event.session.model_switched",
            false,
            json!({
                "from": from,
                "to": to,
            }),
        ),
        SessionEvent::PluginCommandActivated {
            activation_id,
            plugin_id,
            command_name,
            command_args,
            trigger,
        } => {
            let mut payload = json!({
                "activationId": activation_id,
                "pluginId": plugin_id,
                "commandName": command_name,
                "trigger": trigger,
            });
            if let Some(command_args) = command_args {
                payload["commandArgs"] = json!(command_args);
            }
            ("event.plugin_command.activated", false, payload)
        }
        SessionEvent::CronFired {
            task_id,
            cron,
            prompt,
            recurring,
            coalesced_count,
            stale,
        } => (
            // Mirrors the TS `CronFiredEvent`: the cron-job origin object
            // plus the prompt. The rust manager coalesces overdue periods
            // and flags stale final deliveries (B5), so those origin
            // fields carry the real values from the fire pass.
            "event.cron.fired",
            false,
            json!({
                "origin": {
                    "kind": "cron_job",
                    "jobId": task_id,
                    "cron": cron,
                    "recurring": recurring,
                    "coalescedCount": coalesced_count,
                    "stale": stale,
                },
                "prompt": prompt,
            }),
        ),
        // The work-facts tuple is folded from session activity (US4.4 /
        // SDR-013): the durable global fan-out lets list rows update
        // without a per-session subscription. Durable — the payload rides
        // the replay log. Absent `last_turn_reason` (no completed turn yet)
        // stays absent: the protocol says the key is missing before any
        // turn, so the payload never carries a null placeholder.
        SessionEvent::SessionWorkChanged { state } => {
            let mut payload = json!({
                "busy": state.busy,
                "main_turn_active": state.main_turn_active,
                "pending_interaction": state.pending_interaction,
            });
            if let Some(reason) = state.last_turn_reason {
                payload["last_turn_reason"] = json!(reason);
            }
            ("event.session.work_changed", false, payload)
        }
        SessionEvent::Unknown => return None,
    };
    Some(ProjectedEvent {
        method,
        volatile,
        payload,
    })
}

/// Whether an event is fire-and-forget: streamed live to subscribers but
/// never stamped with a seq nor retained in the durable replay log.
///
/// Returns `true` for [`SessionEvent::AgentMessageChunk`],
/// [`SessionEvent::ThinkingDelta`], [`SessionEvent::AgentStatusUpdated`],
/// [`SessionEvent::ToolCallDelta`], [`SessionEvent::ToolProgress`],
/// [`SessionEvent::ShellStarted`], and [`SessionEvent::ShellOutput`]
/// (all align with the protocol's `VOLATILE_EVENT_TYPES`), and
/// [`SessionEvent::Unknown`] (the latter has no wire projection, so it is
/// dropped by the forwarder either way and must not occupy cursor space).
pub fn is_volatile(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::AgentMessageChunk { .. }
            | SessionEvent::ThinkingDelta { .. }
            | SessionEvent::AgentStatusUpdated { .. }
            | SessionEvent::ToolCallDelta { .. }
            | SessionEvent::ToolProgress { .. }
            | SessionEvent::ShellStarted { .. }
            | SessionEvent::ShellOutput { .. }
            | SessionEvent::Unknown
    )
}

/// Map a [`TurnStopReason`] to the wire reason string carried by
/// `event.turn.ended` payloads and `turn.ended` wire-log records (mirrors
/// the TS kernel's `turnStopReasonToWire`).
pub fn turn_stop_reason_wire(reason: &TurnStopReason) -> &'static str {
    match reason {
        TurnStopReason::EndTurn => "completed",
        TurnStopReason::MaxTokens => "completed",
        TurnStopReason::Filtered => "filtered",
        TurnStopReason::Paused => "completed",
        TurnStopReason::Unknown => "completed",
        TurnStopReason::Aborted => "cancelled",
        TurnStopReason::Stalled => "stalled",
        TurnStopReason::BlockedByHook => "blocked",
    }
}

/// Build the TS `TokenUsage` wire object (`inputOther`/`output`/
/// `inputCacheRead`/`inputCacheCreation`) from the engine's raw token
/// counters — the shape the `turn.step.completed` and
/// `subagent.completed` payloads carry (and the bucket dialect
/// [`WireUsageStatus`] reuses).
fn wire_token_usage(
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> Value {
    serde_json::to_value(WireTokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    })
    .expect("plain u64 struct serializes")
}

/// A projected event wrapped in the transport envelope, ready to be framed
/// as a JSON-RPC notification by the backend.
///
/// Deliberately transport-neutral: it carries the method string and the
/// full `params` object (envelope keys merged with the flat payload), and
/// does not depend on `mirri-transport` — core never depends on transport.
pub struct ProjectedNotification {
    /// Protocol method, e.g. `event.message.received`.
    pub method: String,
    /// The full notification `params`: envelope keys (`sessionId`,
    /// `volatile`, `timestamp`, plus `seq`/`epoch` when durable) merged
    /// with the event's flat payload fields.
    pub params: Value,
}

/// Build the transport envelope around one [`SessionEvent`]'s projection,
/// or `None` when the event has no wire projection
/// ([`SessionEvent::Unknown`]).
///
/// `sessionId`, `volatile` and `timestamp` are always present. `seq` and
/// `epoch` are present if and only if the event is durable and the caller
/// supplied them — both backends have a seq source for their durable
/// events, so a durable event without a supplied seq/epoch is a caller
/// bug guarded by `debug_assert` (never a production panic). The payload
/// fields are merged flat into `params` after the envelope keys; payload
/// objects never use the reserved envelope keys, so the merge is
/// collision-free.
pub fn event_notification(
    event: &SessionEvent,
    session_id: &str,
    seq: Option<u64>,
    epoch: Option<&str>,
    timestamp: DateTime<Utc>,
) -> Option<ProjectedNotification> {
    let projected = project(event)?;
    let mut params = serde_json::Map::new();
    params.insert("sessionId".to_string(), json!(session_id));
    params.insert("volatile".to_string(), json!(projected.volatile));
    params.insert("timestamp".to_string(), json!(timestamp.to_rfc3339()));
    if !projected.volatile {
        if let Some(seq) = seq {
            params.insert("seq".to_string(), json!(seq));
        }
        if let Some(epoch) = epoch {
            params.insert("epoch".to_string(), json!(epoch));
        }
        debug_assert!(
            seq.is_some(),
            "durable event {} requires a seq",
            projected.method
        );
        debug_assert!(
            epoch.is_some(),
            "durable event {} requires an epoch",
            projected.method
        );
    }
    // Merge the type-specific payload fields into the envelope (design
    // §7.0: `additionalProperties: true`). Payload keys never collide with
    // the reserved envelope keys, so payload insertion order cannot clobber
    // the envelope.
    if let Value::Object(payload) = projected.payload {
        for (key, value) in payload {
            params.insert(key, value);
        }
    }
    Some(ProjectedNotification {
        method: projected.method.to_string(),
        params: Value::Object(params),
    })
}

/// The camelCase `event.agent.status.updated` payload (CR4).
///
/// Built through serde (not the `json!` macro) so an absent optional field
/// is entirely absent from the JSON — the protocol marks every field but
/// `agentId`/`phase` optional with a null-rejecting schema, so a null
/// placeholder would fail the client contract.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentStatusPayload<'a> {
    agent_id: &'a str,
    phase: AgentPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_effort: Option<ThinkingEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_usage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swarm_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<WireUsageStatus<'a>>,
}

/// The protocol `UsageStatus` wire object (`byModel`/`currentTurn`/`total`),
/// each bucket in the TS `TokenUsage` spelling.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireUsageStatus<'a> {
    by_model: HashMap<&'a str, WireTokenUsage>,
    current_turn: WireTokenUsage,
    total: WireTokenUsage,
    /// The agent's first LLM call — the cold-start baseline (SDR-004);
    /// absent before any call or for pre-field history.
    #[serde(skip_serializing_if = "Option::is_none")]
    first_call: Option<WireTokenUsage>,
}

impl<'a> From<&'a UsageStatus> for WireUsageStatus<'a> {
    fn from(status: &'a UsageStatus) -> Self {
        Self {
            by_model: status
                .by_model
                .iter()
                .map(|(model, usage)| (model.as_str(), WireTokenUsage::from(usage)))
                .collect(),
            current_turn: WireTokenUsage::from(&status.current_turn),
            total: WireTokenUsage::from(&status.total),
            first_call: status.first_call.as_ref().map(WireTokenUsage::from),
        }
    }
}

/// The TS `TokenUsage` wire object (`inputOther`/`output`/`inputCacheRead`/
/// `inputCacheCreation`) — the bucket dialect the protocol's usage objects
/// speak, distinct from the rust [`TokenUsage`]'s snake_case serde shape.
#[derive(Serialize)]
struct WireTokenUsage {
    #[serde(rename = "inputOther")]
    input_tokens: u64,
    #[serde(rename = "output")]
    output_tokens: u64,
    #[serde(rename = "inputCacheRead")]
    cache_read_tokens: u64,
    #[serde(rename = "inputCacheCreation")]
    cache_creation_tokens: u64,
}

impl From<&TokenUsage> for WireTokenUsage {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, root: &str, name: Option<&str>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.to_string(),
            root: root.to_string(),
            name: name.map(str::to_string),
        }
    }

    // Given a workspace created server event, when it is projected, then the
    // notification carries the event.workspace.created method, the empty
    // session id (the WireEventBase-required field, no session involved),
    // the volatile flag, and the workspace snapshot payload.
    #[test]
    fn should_project_created_event_with_the_empty_session_envelope() {
        let event = ServerEvent::WorkspaceCreated {
            workspace: snapshot("wd_a_000000000001", "/proj/a", Some("a")),
        };

        let notification = project_server_event(&event);

        assert_eq!(notification.method, "event.workspace.created");
        assert_eq!(notification.params["sessionId"], json!(""));
        assert_eq!(notification.params["volatile"], json!(true));
        assert!(
            notification.params["timestamp"].as_str().is_some(),
            "a timestamp is present"
        );
        assert_eq!(
            notification.params["workspace"]["id"],
            json!("wd_a_000000000001")
        );
        assert_eq!(notification.params["workspace"]["root"], json!("/proj/a"));
        assert_eq!(notification.params["workspace"]["name"], json!("a"));
    }

    // Given a workspace without a display name, when its updated event is
    // projected, then the name key is absent (never null) — the same
    // absent-not-null rule as the session projection's optional fields.
    #[test]
    fn should_omit_the_name_when_the_workspace_snapshot_has_none() {
        let event = ServerEvent::WorkspaceUpdated {
            workspace: snapshot("wd_b_000000000002", "/proj/b", None),
        };

        let notification = project_server_event(&event);

        assert_eq!(notification.method, "event.workspace.updated");
        let workspace = &notification.params["workspace"];
        assert_eq!(workspace["root"], json!("/proj/b"));
        assert!(
            workspace.get("name").is_none(),
            "an unset name must be absent, never null"
        );
    }

    // Given a workspace deleted server event, when it is projected, then the
    // notification carries the camelCase workspaceId/root payload plus the
    // server-scoped envelope.
    #[test]
    fn should_project_deleted_event_with_the_workspace_id_payload() {
        let event = ServerEvent::WorkspaceDeleted {
            workspace_id: "wd_c_000000000003".to_string(),
            root: "/proj/c".to_string(),
        };

        let notification = project_server_event(&event);

        assert_eq!(notification.method, "event.workspace.deleted");
        assert_eq!(notification.params["sessionId"], json!(""));
        assert_eq!(
            notification.params["workspaceId"],
            json!("wd_c_000000000003")
        );
        assert_eq!(notification.params["root"], json!("/proj/c"));
    }

    // Given the server event vocabulary, when compared to the closed list,
    // then every variant maps to exactly one method (the contract 3 single
    // source for the workspace lifecycle verbs).
    #[test]
    fn should_cover_every_server_event_variant_in_the_vocabulary() {
        let projected: Vec<&str> = [
            ServerEvent::WorkspaceCreated {
                workspace: snapshot("w", "/p", None),
            },
            ServerEvent::WorkspaceUpdated {
                workspace: snapshot("w", "/p", None),
            },
            ServerEvent::WorkspaceDeleted {
                workspace_id: "w".to_string(),
                root: "/p".to_string(),
            },
        ]
        .iter()
        .map(|event| event.method())
        .collect();
        let mut vocabulary = SERVER_PROJECTED_EVENT_METHODS.to_vec();
        vocabulary.sort_unstable();
        let mut projected = projected;
        projected.sort_unstable();
        assert_eq!(vocabulary, projected);
    }
}
