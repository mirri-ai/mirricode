//! Live-event relay: the shared [`LiveEvent`] → [`SessionEvent`] mapping
//! every session-scoped live callback uses.
//!
//! The mapping used to live privately in `mirri-embedded`'s
//! `event_relay.rs`; it moved into `mirri-core` so the subagent host (also
//! core) can wire its child agents' live events onto the session bus
//! through the exact same translation — one authority, no dialect drift
//! between the main agent's and a subagent's tool events (contract 3).
//!
//! All of this is pure logic: no IO, no clock, no spawn.

use std::sync::Arc;

use crate::LiveEvent;
use crate::session::events::{EventBus, SessionEvent};

/// Map one live event onto its session-bus [`SessionEvent`], or `None` for
/// events with no session projection (a `ToolCallPart` without a call id —
/// the fragment preceding its identity header, only possible in malformed
/// provider streams; the wire `tool.call.delta` requires a `toolCallId`, and
/// the rust block-level `ToolCall` only flushes at stream end, so buffering
/// would forfeit the live-streaming purpose entirely).
pub fn live_event_to_session_event(
    live: LiveEvent,
    agent_id: &str,
    session_id: &str,
    turn_id: Option<&str>,
) -> Option<SessionEvent> {
    let event = match live {
        LiveEvent::TextDelta { text } => SessionEvent::AgentMessageChunk {
            agent_id: agent_id.to_string(),
            text,
        },
        LiveEvent::ThinkDelta { text } => SessionEvent::ThinkingDelta {
            agent_id: agent_id.to_string(),
            text,
        },
        LiveEvent::ToolCallStarted {
            tool_name,
            tool_call_id,
            args,
        } => SessionEvent::ToolCallStarted {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            tool_name,
            tool_call_id,
            args,
        },
        LiveEvent::ToolCallCompleted {
            tool_call_id,
            tool_name,
            is_error,
            output,
        } => SessionEvent::ToolCallCompleted {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            tool_call_id,
            tool_name,
            is_error,
            output,
        },
        LiveEvent::ToolProgress {
            tool_call_id,
            update,
        } => SessionEvent::ToolProgress {
            agent_id: agent_id.to_string(),
            tool_call_id,
            update,
        },
        LiveEvent::ToolCallPart {
            call_id: Some(tool_call_id),
            tool_name,
            arguments_part,
            ..
        } => SessionEvent::ToolCallDelta {
            agent_id: agent_id.to_string(),
            tool_call_id,
            tool_name,
            arguments_part,
        },
        LiveEvent::TurnInterrupted {
            reason,
            attempted_steps: _,
            active_step,
            message,
        } => SessionEvent::TurnStepInterrupted {
            // The wire turn id is supplied by the callback's caller (the
            // dispatch minted it); `None` only for callbacks built without
            // one — the projection then omits the field.
            turn_id: turn_id.map(str::to_string),
            step: active_step.unwrap_or(0),
            reason,
            message,
        },
        LiveEvent::StepRetrying {
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
        } => SessionEvent::TurnStepRetrying {
            turn_id: turn_id.map(str::to_string),
            // Step-level retries re-enqueue the current step — the live
            // channel carries no step number either, so the retried step is
            // identified by its attempt counters.
            step: 0,
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
        },
        // ToolCallPart without a call id: dropped (see the doc comment).
        LiveEvent::ToolCallPart { call_id: None, .. } => return None,
        // The in-flight heartbeat is volatile: it feeds the
        // stall-watchdog clock and nothing else — no bus projection.
        LiveEvent::LlmPending { .. } => return None,
    };
    Some(event)
}

/// Build the live-event callback that surfaces a running turn's live events
/// on the given session's event bus.
///
/// `agent_id` travels verbatim ("main", "agent-<uuid>") — consumers route
/// transcript frames by it. `turn_id` (the wire `turn-<uuid>` the dispatch
/// minted for this turn) rides on the interrupt/retry events; `None` when
/// the caller has no turn id (the projection then omits the field).
pub fn live_event_callback(
    event_bus: &Arc<EventBus>,
    agent_id: &str,
    session_id: &str,
    turn_id: Option<String>,
) -> crate::LiveEventCallback {
    let bus = Arc::clone(event_bus);
    let agent_id = agent_id.to_string();
    let session_id = session_id.to_string();
    Arc::new(move |live_event: LiveEvent| {
        if let Some(event) =
            live_event_to_session_event(live_event, &agent_id, &session_id, turn_id.as_deref())
        {
            bus.publish(event);
        }
    })
}
