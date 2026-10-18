//! Per-session in-flight-turn shadow: the volatile stream accumulator
//! behind `session.snapshot`'s `in_flight_turn` (the R2 在途回合 wire).
//!
//! Streaming deltas (`AgentMessageChunk` / `ThinkingDelta`), running tool
//! state (`ToolCallStarted` / `ToolProgress` / `ToolCallCompleted`) and the
//! turn lifecycle brackets (`TurnStarted` / `TurnStepStarted` /
//! `TurnCompleted` / `TurnFailed`) are volatile or engine-side facts that
//! never land in the durable event log, so a reconnecting client cannot
//! replay them. This shadow subscribes to the session's own event bus and
//! folds those events into the "current turn so far" view the snapshot
//! reads — the rust analogue of the TS `inFlightTurnTracker` (same
//! main-agent-only scoping).
//!
//! The shadow is a derived cache, not a second source of truth: it can be
//! dropped and rebuilt at any time, and its content is superseded by the
//! durable records the moment the turn ends. Text and thinking accumulate
//! **per step** (reset on `TurnStepStarted`) — earlier steps' content has
//! already landed in context memory and is visible through the snapshot's
//! message page.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::events::{EventBus, SessionEvent};
use crate::tool::{ToolUpdate, ToolUpdateKind};

/// The agent identity the transcript-scoped in-flight view tracks. Matches
/// the web projector's `MAIN_AGENT_ID` and the TS `InFlightTurnTracker`
/// scoping: subagent activity renders through its own task panel, not the
/// main conversation's in-flight bubble.
const MAIN_AGENT: &str = "main";

/// One running tool call, as the `in_flight_turn.running_tools` wire row.
#[derive(Debug, Clone)]
struct RunningTool {
    tool_call_id: String,
    tool_name: String,
    args: Value,
    /// Most recent `tool.progress` update (the wire `last_progress` shape:
    /// `{kind, text?, percent?}`), when any arrived yet.
    last_progress: Option<Value>,
}

/// The accumulated state of the current turn (empty = no turn in flight).
#[derive(Debug, Default)]
struct ShadowState {
    turn_id: Option<String>,
    assistant_text: String,
    thinking_text: String,
    running_tools: Vec<RunningTool>,
}

impl ShadowState {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The per-session shadow accumulator. Cheap to share (`Arc`), lock-guarded
/// reads for the snapshot path; the fold loop runs on a spawned task that
/// exits when the session's event bus closes.
pub struct InFlightTurnShadow {
    state: Mutex<ShadowState>,
}

impl std::fmt::Debug for InFlightTurnShadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightTurnShadow")
            .field(
                "in_flight",
                &self
                    .state
                    .lock()
                    .map(|s| s.turn_id.is_some())
                    .unwrap_or(false),
            )
            .finish()
    }
}

impl InFlightTurnShadow {
    /// Create the shadow and start folding the given bus's events into it.
    ///
    /// The fold task is spawned on the current tokio runtime; without one
    /// (unit-test contexts constructing artifacts outside a runtime) the
    /// shadow simply stays empty — it is a cache, never a correctness
    /// dependency.
    pub fn spawn(event_bus: &Arc<EventBus>) -> Arc<Self> {
        let shadow = Arc::new(Self {
            state: Mutex::new(ShadowState::default()),
        });
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("in-flight shadow not started: no tokio runtime");
            return shadow;
        }
        let mut receiver = event_bus.subscribe();
        let task_shadow = Arc::clone(&shadow);
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => task_shadow.apply(event),
                    Err(RecvError::Lagged(missed)) => {
                        // A lagged shadow may have missed turn boundaries —
                        // its text could be pasted onto the wrong turn. Reset
                        // to an honest "unknown" state rather than risk
                        // presenting content that belongs to a previous turn.
                        tracing::warn!(missed, "in-flight shadow lagged; resetting");
                        task_shadow.reset();
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        shadow
    }

    /// Fold one event into the shadow state.
    fn apply(&self, event: SessionEvent) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match event {
            SessionEvent::TurnStarted { turn_id, .. } => {
                state.clear();
                state.turn_id = Some(turn_id);
            }
            // Fresh step → fresh stream accumulation: earlier steps' content
            // has landed in context memory (visible through the message
            // page), so the shadow tracks only the current step.
            SessionEvent::TurnStepStarted { .. } => {
                state.assistant_text.clear();
                state.thinking_text.clear();
            }
            SessionEvent::AgentMessageChunk { agent_id, text }
                if agent_id == MAIN_AGENT && state.turn_id.is_some() =>
            {
                state.assistant_text.push_str(&text);
            }
            SessionEvent::ThinkingDelta { agent_id, text }
                if agent_id == MAIN_AGENT && state.turn_id.is_some() =>
            {
                state.thinking_text.push_str(&text);
            }
            // Tool events carry the emitting agent — only the main agent's calls
            // are this turn's running tools; a member's (agent-<uuid>) live
            // elsewhere (its own task row / focus view).
            SessionEvent::ToolCallStarted {
                agent_id,
                tool_call_id,
                tool_name,
                args,
                ..
            } if agent_id == MAIN_AGENT => {
                state.running_tools.push(RunningTool {
                    tool_call_id,
                    tool_name,
                    args,
                    last_progress: None,
                });
            }
            SessionEvent::ToolProgress {
                agent_id,
                tool_call_id,
                update,
            } if agent_id == MAIN_AGENT => {
                if let Some(tool) = state
                    .running_tools
                    .iter_mut()
                    .find(|t| t.tool_call_id == tool_call_id)
                {
                    tool.last_progress = Some(tool_update_wire(&update));
                }
            }
            SessionEvent::ToolCallCompleted {
                agent_id,
                tool_call_id,
                ..
            } if agent_id == MAIN_AGENT => {
                state
                    .running_tools
                    .retain(|t| t.tool_call_id != tool_call_id);
            }
            SessionEvent::TurnCompleted { .. } | SessionEvent::TurnFailed { .. } => {
                state.clear();
            }
            _ => {}
        }
    }

    /// Reset to the idle state (a lagged consumer's honest fallback).
    fn reset(&self) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.clear();
    }

    /// The `in_flight_turn` wire value (`None` when no turn is running) —
    /// the TS `rest-snapshot` `in_flight_turn` shape: snake_case keys,
    /// `turn_id` / `assistant_text` / `thinking_text` /
    /// `running_tools[{tool_call_id, name, args, last_progress}]`.
    /// `current_prompt_id` is NOT accumulated here — the shadow tracks the
    /// event-bus turn, not the wire-log fold — but
    /// [`SessionSnapshotHandler`](mirri_server::rpc) stamps the fold's
    /// still-open tail bracket key onto this value so the client's in-flight
    /// seed and the transcript rows share one turn identity.
    pub fn snapshot_value(&self) -> Option<Value> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let turn_id = state.turn_id.as_deref()?;
        Some(json!({
            "turn_id": turn_id,
            "assistant_text": state.assistant_text,
            "thinking_text": state.thinking_text,
            "running_tools": state
                .running_tools
                .iter()
                .map(|tool| {
                    let mut row = json!({
                        "tool_call_id": tool.tool_call_id,
                        "name": tool.tool_name,
                        "args": tool.args,
                    });
                    if let Some(progress) = &tool.last_progress {
                        row["last_progress"] = progress.clone();
                    }
                    row
                })
                .collect::<Vec<Value>>(),
        }))
    }
}

/// Map a tool progress update onto the wire `last_progress` shape
/// (`{kind, text?, percent?}` — the TS `inFlightToolCallSchema` field).
fn tool_update_wire(update: &ToolUpdate) -> Value {
    let kind = match update.kind {
        ToolUpdateKind::Stdout => "stdout",
        ToolUpdateKind::Stderr => "stderr",
        ToolUpdateKind::Progress => "progress",
        ToolUpdateKind::Status => "status",
        ToolUpdateKind::Custom => "custom",
    };
    let mut wire = json!({ "kind": kind });
    if let Some(text) = &update.text {
        wire["text"] = json!(text);
    }
    if let Some(percent) = update.percent {
        wire["percent"] = json!(percent);
    }
    wire
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::events::EventBus;

    fn shadow_with_bus() -> (Arc<InFlightTurnShadow>, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(64));
        let shadow = InFlightTurnShadow::spawn(&bus);
        (shadow, bus)
    }

    /// Given an idle session, when the snapshot reads the shadow, then the
    /// in-flight turn is absent (no ghost in-flight state).
    #[test]
    fn should_report_no_in_flight_turn_when_session_is_idle() {
        let (shadow, _bus) = shadow_with_bus();
        assert!(shadow.snapshot_value().is_none());
    }

    /// Given a running turn, when deltas and tool events fold in, then the
    /// snapshot carries the accumulated text, thinking, and running tools —
    /// and the turn end clears it.
    #[test]
    fn should_accumulate_and_clear_when_turn_runs_and_ends() {
        let (shadow, _bus) = shadow_with_bus();

        shadow.apply(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "s1".to_string(),
            turn_id: "turn-1".to_string(),
        });
        shadow.apply(SessionEvent::AgentMessageChunk {
            agent_id: "main".to_string(),
            text: "hello ".to_string(),
        });
        shadow.apply(SessionEvent::AgentMessageChunk {
            agent_id: "main".to_string(),
            text: "world".to_string(),
        });
        shadow.apply(SessionEvent::ThinkingDelta {
            agent_id: "main".to_string(),
            text: "thinking hard".to_string(),
        });
        shadow.apply(SessionEvent::ToolCallStarted {
            agent_id: "main".to_string(),
            session_id: "s1".to_string(),
            tool_name: "Bash".to_string(),
            tool_call_id: "call-1".to_string(),
            args: json!({ "command": "ls" }),
        });

        let value = shadow.snapshot_value().expect("turn in flight");
        assert_eq!(value["turn_id"], "turn-1");
        assert_eq!(value["assistant_text"], "hello world");
        assert_eq!(value["thinking_text"], "thinking hard");
        assert_eq!(value["running_tools"][0]["tool_call_id"], "call-1");
        assert_eq!(value["running_tools"][0]["name"], "Bash");

        shadow.apply(SessionEvent::ToolCallCompleted {
            agent_id: "main".to_string(),
            session_id: "s1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "Bash".to_string(),
            is_error: false,
            output: crate::tool::ToolOutput::text("ok"),
        });
        let value = shadow.snapshot_value().expect("turn still in flight");
        assert!(
            value["running_tools"].as_array().unwrap().is_empty(),
            "completed tool leaves the running set"
        );

        shadow.apply(SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "s1".to_string(),
            turn_id: "turn-1".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 1,
            duration_ms: None,
        });
        assert!(shadow.snapshot_value().is_none());
    }

    /// Given subagent-scoped deltas, when they fold in, then they are
    /// excluded from the main conversation's in-flight view.
    #[test]
    fn should_exclude_subagent_chunks_from_main_in_flight_view() {
        let (shadow, _bus) = shadow_with_bus();
        shadow.apply(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "s1".to_string(),
            turn_id: "turn-1".to_string(),
        });
        shadow.apply(SessionEvent::AgentMessageChunk {
            agent_id: "agent-1234".to_string(),
            text: "subagent text".to_string(),
        });
        let value = shadow.snapshot_value().expect("turn in flight");
        assert_eq!(value["assistant_text"], "");
    }
}
