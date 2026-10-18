//! Collaboration tools — thin wrappers over [`CollaborationManager`].
//!
//! These tools let the LLM start, message, query, intervene, escalate, and
//! terminate collaboration sessions. Each tool delegates to the shared
//! session-scoped [`CollaborationManager`], so every agent in the session
//! sees the same state.

use std::sync::Arc;

use mirri_collaboration::{AgentMessage, CollaborationManager, CollaborationMode};
use serde_json::json;
use uuid::Uuid;

use crate::error::EngineResult;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

fn err_result(text: impl Into<String>) -> EngineResult<ToolExecution> {
    Ok(ToolExecution::Error(ToolErrorResult {
        message: text.into(),
    }))
}

// The `Err` arm reuses `EngineResult<ToolExecution>` so call sites can
// surface the parse failure as a tool error verbatim; boxing the error type
// would ripple through the engine API for a size-only concern.
#[allow(clippy::result_large_err)]
fn parse_session_id(input: &serde_json::Value) -> Result<Uuid, EngineResult<ToolExecution>> {
    let s = input
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| err_result("missing required 'session_id' string argument"))?;
    Uuid::parse_str(s).map_err(|_| err_result("invalid session_id UUID"))
}

fn make_runnable(
    display_summary: String,
    f: impl FnOnce() -> ToolResult + Send + 'static,
) -> EngineResult<ToolExecution> {
    let plan = ExecutionPlan {
        accesses: Vec::new(),
        display: ToolInputDisplay::Generic {
            summary: display_summary,
        },
        approval_rule: "Collaboration".to_string(),
        stop_batch_after_this: false,
    };
    Ok(ToolExecution::Runnable {
        plan,
        execute: Box::new(move |_ctx| Box::pin(async move { f() })),
    })
}

/// The names of the 7 collaboration tools — the mode-gated set the host
/// toggles as a whole via `collaboration.enter` / `collaboration.exit`
/// (see [`crate::tools::register_builtin_tools`]).
pub const COLLABORATION_TOOL_NAMES: [&str; 7] = [
    "CollaborationStart",
    "CollaborationSendMessage",
    "CollaborationGetMessages",
    "CollaborationGetState",
    "CollaborationIntervene",
    "CollaborationEscalate",
    "CollaborationTerminate",
];

// ── collaboration.start ──────────────────────────────────────────────

pub struct CollaborationStartTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationStartTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationStartTool {
    fn name(&self) -> &str {
        "CollaborationStart"
    }

    fn description(&self) -> &str {
        "Start a new multi-agent collaboration session with a user-defined \
         mode (roles + rules). Returns the session ID and the list of agent \
         (role) names. The roles define the agents that take part in the \
         collaboration; the from/to fields of every protocol message must \
         name one of the returned agents. If the orchestrator (the main \
         agent) itself needs to send or receive messages, it should define \
         a coordinator role for itself in mode.roles."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "object",
                        "description": "The collaboration mode defining roles and rules.",
                        "properties": {
                            "id": { "type": "string" },
                            "name": { "type": "string" },
                            "description": { "type": "string" },
                            "roles": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "description": { "type": "string" },
                                        "system_prompt": { "type": "string" },
                                        "capabilities": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        },
                                        "max_agents": { "type": "integer", "minimum": 1 }
                                    },
                                    "required": ["name", "description", "system_prompt", "capabilities"]
                                }
                            },
                            "rules": {
                                "type": "object",
                                "properties": {
                                    "communication_pattern": {
                                        "type": "string",
                                        "enum": ["sequential", "parallel", "hierarchical"]
                                    },
                                    "intervention_policy": {
                                        "type": "string",
                                        "enum": ["human", "main_agent", "any_agent"]
                                    },
                                    "conflict_resolution": {
                                        "type": "string",
                                        "enum": ["human_arbitration", "main_agent_arbitration", "majority_vote"]
                                    },
                                    "max_rounds": { "type": "integer", "minimum": 1 }
                                }
                            }
                        },
                        "required": ["id", "name", "description", "roles", "rules"]
                    },
                    "task": { "type": "string", "description": "The shared objective." }
                },
                "required": ["mode", "task"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let mode_json = match input.get("mode") {
            Some(v) => v,
            None => return err_result("CollaborationStart requires a 'mode' object"),
        };
        let task = match input.get("task").and_then(serde_json::Value::as_str) {
            Some(t) => t.to_string(),
            None => return err_result("CollaborationStart requires a 'task' string"),
        };
        let mode: CollaborationMode = match serde_json::from_value(mode_json.clone()) {
            Ok(m) => m,
            Err(e) => return err_result(format!("invalid collaboration mode: {e}")),
        };
        // Validate before making runnable so the LLM gets a structured error
        // instead of a panic inside `manager.start`.
        if let Err(e) = mode.validate() {
            return err_result(format!("invalid collaboration mode: {e}"));
        }
        // The agent (role) names ride the success payload: the caller needs
        // them to address every follow-up message (from/to must be one of
        // these names).
        let agents: Vec<String> = mode.roles.iter().map(|role| role.name.clone()).collect();
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.start".into(), move || {
            match manager.start(mode, task) {
                Ok(session_id) => ToolResult::success(
                    json!({ "session_id": session_id, "agents": agents }).to_string(),
                ),
                Err(error) => ToolResult::error(error.to_string()),
            }
        })
    }
}

// ── collaboration.send_message ───────────────────────────────────────

/// The valid `type` tags of an `AgentMessage` payload — the protocol
/// variants an agent can send. Mirrors `mirri_collaboration::AgentMessage`
/// (minus the `Unknown` journal-compatibility variant, which is not
/// sendable).
const VALID_MESSAGE_TYPES: [&str; 7] = [
    "task_assignment",
    "task_result",
    "question",
    "answer",
    "conflict_escalation",
    "intervention",
    "status_update",
];

/// The error text for a missing or unknown `message.type` tag — lists every
/// valid type so the LLM can correct the call in one step.
fn unknown_message_type_error(type_name: Option<&str>) -> String {
    format!(
        "unknown message type '{}'; valid types: {}",
        type_name.unwrap_or("<missing>"),
        VALID_MESSAGE_TYPES.join(", ")
    )
}

pub struct CollaborationSendMessageTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationSendMessageTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationSendMessageTool {
    fn name(&self) -> &str {
        "CollaborationSendMessage"
    }

    fn description(&self) -> &str {
        "Send a protocol message from one agent to another in a collaboration session. \
         The message 'type' must be one of: task_assignment, task_result, question, \
         answer, conflict_escalation, intervention, status_update. The 'from' and 'to' \
         fields must be agent (role) names returned by CollaborationStart."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "message": {
                        "type": "object",
                        "description": "The AgentMessage to send. 'from'/'to' must be \
                                        agent (role) names from the CollaborationStart \
                                        result. Exactly one of the 7 variants below.",
                        // `anyOf` (not `oneOf`): the final fallback arm admits
                        // structurally-invalid messages so they reach the engine's
                        // own validation, which answers with the precise error
                        // (unknown type → the valid-type list; missing field →
                        // serde's missing-field message) instead of the generic
                        // "not valid under any of the schemas" rejection.
                        "anyOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "task_assignment" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "to": { "type": "string", "description": "Recipient agent (role) name." },
                                    "task": { "type": "string", "description": "The task to perform." },
                                    "context": { "type": "object", "description": "Supporting context for the task." }
                                },
                                "required": ["type", "from", "to", "task", "context"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "task_result" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "to": { "type": "string", "description": "Recipient agent (role) name." },
                                    "task_id": { "type": "string", "description": "The id of the assigned task." },
                                    "result": { "type": "object", "description": "The task's outcome payload." },
                                    "status": { "type": "string", "enum": ["completed", "failed", "in_progress", "blocked"] }
                                },
                                "required": ["type", "from", "to", "task_id", "result", "status"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "question" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "to": { "type": "string", "description": "Recipient agent (role) name." },
                                    "question": { "type": "string" }
                                },
                                "required": ["type", "from", "to", "question"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "answer" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "to": { "type": "string", "description": "Recipient agent (role) name." },
                                    "question_id": { "type": "string", "description": "The id of the question being answered." },
                                    "answer": { "type": "string" }
                                },
                                "required": ["type", "from", "to", "question_id", "answer"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "conflict_escalation" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "description": { "type": "string", "description": "What the dispute is about." },
                                    "parties": { "type": "array", "items": { "type": "string" }, "description": "Agent (role) names in conflict." }
                                },
                                "required": ["type", "from", "description", "parties"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "intervention" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "to": { "type": "string", "description": "Recipient agent (role) name." },
                                    "directive": { "type": "string" }
                                },
                                "required": ["type", "from", "to", "directive"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "type": { "const": "status_update" },
                                    "from": { "type": "string", "description": "Sender agent (role) name." },
                                    "status": { "type": "string", "enum": ["idle", "working", "waiting", "done", "error"] }
                                },
                                "required": ["type", "from", "status"],
                                "additionalProperties": false
                            },
                            {
                                // The fallback arm: any other object. The engine
                                // rejects it with a precise error (the valid-type
                                // list for an unknown `type`, serde's
                                // missing-field message for an incomplete
                                // variant) — see `resolve_execution`.
                                "type": "object",
                                "description": "Fallback: a message that matches \
                                                none of the 7 variants above. It \
                                                is accepted here only so the engine \
                                                can reject it with a precise error."
                            }
                        ]
                    }
                },
                "required": ["session_id", "message"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let message_json = match input.get("message") {
            Some(v) => v,
            None => return err_result("CollaborationSendMessage requires a 'message'"),
        };
        // Validate the type tag before deserializing: serde's untagged-error
        // for an unknown tag is opaque ("unknown variant"), and the
        // `#[serde(other)]` journal-compatibility arm would silently turn
        // the message into `Unknown` (which cannot be routed). Listing the
        // valid types lets the LLM correct the call in one step.
        let type_name = message_json.get("type").and_then(serde_json::Value::as_str);
        if !type_name.is_some_and(|t| VALID_MESSAGE_TYPES.contains(&t)) {
            return err_result(unknown_message_type_error(type_name));
        }
        let message: AgentMessage = match serde_json::from_value(message_json.clone()) {
            Ok(m) => m,
            Err(e) => return err_result(format!("invalid agent message: {e}")),
        };
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.send_message".into(), move || {
            match manager.send_message(session_id, message) {
                Ok(()) => ToolResult::success("message sent"),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}

// ── collaboration.get_messages ───────────────────────────────────────

pub struct CollaborationGetMessagesTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationGetMessagesTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationGetMessagesTool {
    fn name(&self) -> &str {
        "CollaborationGetMessages"
    }

    fn description(&self) -> &str {
        "Retrieve all messages from a collaboration session in arrival order."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.get_messages".into(), move || {
            match manager.messages(session_id) {
                Ok(msgs) => ToolResult::success(
                    serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into()),
                ),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}

// ── collaboration.get_state ──────────────────────────────────────────

pub struct CollaborationGetStateTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationGetStateTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationGetStateTool {
    fn name(&self) -> &str {
        "CollaborationGetState"
    }

    fn description(&self) -> &str {
        "Get the current state (not_started, in_progress, paused, completed, \
         failed) and round counter of a collaboration session."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.get_state".into(), move || {
            match manager.state(session_id) {
                Ok(state) => ToolResult::success(
                    serde_json::to_string(&state).unwrap_or_else(|_| "{}".into()),
                ),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}

// ── collaboration.intervene ──────────────────────────────────────────

pub struct CollaborationInterveneTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationInterveneTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationInterveneTool {
    fn name(&self) -> &str {
        "CollaborationIntervene"
    }

    fn description(&self) -> &str {
        "Inject a directive from the human or main agent into a specific \
         agent's context to steer its next turn."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "target": { "type": "string", "description": "Role name of the target agent." },
                    "directive": { "type": "string" }
                },
                "required": ["session_id", "target", "directive"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let target = match input.get("target").and_then(serde_json::Value::as_str) {
            Some(s) => s.to_string(),
            None => return err_result("CollaborationIntervene requires a 'target'"),
        };
        let directive = match input.get("directive").and_then(serde_json::Value::as_str) {
            Some(s) => s.to_string(),
            None => return err_result("CollaborationIntervene requires a 'directive'"),
        };
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.intervene".into(), move || {
            match manager.intervene(session_id, &target, &directive) {
                Ok(()) => ToolResult::success("intervention directive delivered"),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}

// ── collaboration.escalate ───────────────────────────────────────────

pub struct CollaborationEscalateTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationEscalateTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationEscalateTool {
    fn name(&self) -> &str {
        "CollaborationEscalate"
    }

    fn description(&self) -> &str {
        "Request conflict resolution: pause the collaboration and escalate a \
         dispute to the human or main agent for arbitration."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" },
                    "from": { "type": "string" },
                    "description": { "type": "string" },
                    "parties": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["session_id", "from", "description", "parties"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let from = match input.get("from").and_then(serde_json::Value::as_str) {
            Some(s) => s.to_string(),
            None => return err_result("CollaborationEscalate requires a 'from'"),
        };
        let description = match input.get("description").and_then(serde_json::Value::as_str) {
            Some(s) => s.to_string(),
            None => return err_result("CollaborationEscalate requires a 'description'"),
        };
        let parties: Vec<String> = input
            .get("parties")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.escalate".into(), move || {
            match manager.escalate_conflict(session_id, &from, &description, &parties) {
                Ok(()) => {
                    ToolResult::success("conflict escalated — collaboration paused for arbitration")
                }
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}

// ── collaboration.terminate ──────────────────────────────────────────

pub struct CollaborationTerminateTool {
    manager: Arc<CollaborationManager>,
}

impl CollaborationTerminateTool {
    pub fn new(manager: Arc<CollaborationManager>) -> Self {
        Self { manager }
    }
}

impl Tool for CollaborationTerminateTool {
    fn name(&self) -> &str {
        "CollaborationTerminate"
    }

    fn description(&self) -> &str {
        "Terminate a collaboration session. The session is marked as failed \
         with reason 'terminated'. Messages remain readable afterwards."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "format": "uuid" }
                },
                "required": ["session_id"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let session_id = match parse_session_id(&input) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let manager = Arc::clone(&self.manager);
        make_runnable("collaboration.terminate".into(), move || {
            match manager.terminate(session_id) {
                Ok(()) => ToolResult::success("collaboration session terminated"),
                Err(e) => ToolResult::error(e.to_string()),
            }
        })
    }
}
