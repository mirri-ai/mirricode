//! The inter-agent message protocol.
//!
//! [`AgentMessage`] is the single wire format exchanged between agents inside
//! a collaboration. It is an internally tagged JSON enum: every message
//! carries a `type` tag (e.g. `{"type": "task_assignment", ...}`), which keeps
//! the journal human-readable and self-describing.
//!
//! Forward compatibility: a message with an unknown `type` tag deserializes
//! into [`AgentMessage::Unknown`] instead of failing, so journals written by
//! older binaries stay readable after new message kinds are introduced.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The status of a task assigned to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task was finished successfully.
    Completed,
    /// The task failed and cannot be completed as-is.
    Failed,
    /// The task is still being worked on.
    InProgress,
    /// The task cannot proceed until a blocker is resolved.
    Blocked,
}

/// The status of an agent inside a collaboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Ready and waiting for work.
    Idle,
    /// Currently executing a task.
    Working,
    /// Waiting for another agent (or a human) to respond.
    Waiting,
    /// Finished its share of the collaboration.
    Done,
    /// Hit an unrecoverable error.
    Error,
}

/// Messages exchanged between agents in a collaboration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentMessage {
    /// A task assignment from the main agent (or coordinator) to a sub-agent.
    #[serde(rename = "task_assignment")]
    TaskAssignment {
        from: String,
        to: String,
        task: String,
        context: serde_json::Value,
    },

    /// A task result from a sub-agent back to the main agent.
    #[serde(rename = "task_result")]
    TaskResult {
        from: String,
        to: String,
        task_id: String,
        result: serde_json::Value,
        status: TaskStatus,
    },

    /// A question from one agent to another.
    #[serde(rename = "question")]
    Question {
        from: String,
        to: String,
        question: String,
    },

    /// An answer to a question.
    #[serde(rename = "answer")]
    Answer {
        from: String,
        to: String,
        question_id: String,
        answer: String,
    },

    /// A conflict escalation request.
    #[serde(rename = "conflict_escalation")]
    ConflictEscalation {
        from: String,
        description: String,
        parties: Vec<String>,
    },

    /// An intervention from a human or the main agent.
    #[serde(rename = "intervention")]
    Intervention {
        from: String,
        to: String,
        directive: String,
    },

    /// A status update broadcast.
    #[serde(rename = "status_update")]
    StatusUpdate { from: String, status: AgentStatus },

    /// A message whose `type` tag is not known to this binary. Kept so that
    /// deserializing a journal containing a future message kind never fails.
    #[serde(other)]
    Unknown,
}

impl AgentMessage {
    /// The name of the agent that produced this message.
    ///
    /// Returns `None` for [`AgentMessage::Unknown`], which carries no routing
    /// information and therefore can never be sent between agents.
    pub fn sender(&self) -> Option<&str> {
        match self {
            AgentMessage::TaskAssignment { from, .. }
            | AgentMessage::TaskResult { from, .. }
            | AgentMessage::Question { from, .. }
            | AgentMessage::Answer { from, .. }
            | AgentMessage::ConflictEscalation { from, .. }
            | AgentMessage::Intervention { from, .. }
            | AgentMessage::StatusUpdate { from, .. } => Some(from),
            AgentMessage::Unknown => None,
        }
    }

    /// The name of the agent this message is addressed to.
    ///
    /// Returns `None` for broadcast-style messages
    /// ([`AgentMessage::ConflictEscalation`], [`AgentMessage::StatusUpdate`])
    /// and for [`AgentMessage::Unknown`].
    pub fn recipient(&self) -> Option<&str> {
        match self {
            AgentMessage::TaskAssignment { to, .. }
            | AgentMessage::TaskResult { to, .. }
            | AgentMessage::Question { to, .. }
            | AgentMessage::Answer { to, .. }
            | AgentMessage::Intervention { to, .. } => Some(to),
            AgentMessage::ConflictEscalation { .. }
            | AgentMessage::StatusUpdate { .. }
            | AgentMessage::Unknown => None,
        }
    }

    /// The `type` tag this message serializes with.
    pub fn message_type(&self) -> &'static str {
        match self {
            AgentMessage::TaskAssignment { .. } => "task_assignment",
            AgentMessage::TaskResult { .. } => "task_result",
            AgentMessage::Question { .. } => "question",
            AgentMessage::Answer { .. } => "answer",
            AgentMessage::ConflictEscalation { .. } => "conflict_escalation",
            AgentMessage::Intervention { .. } => "intervention",
            AgentMessage::StatusUpdate { .. } => "status_update",
            AgentMessage::Unknown => "unknown",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Blocked => "blocked",
        })
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Waiting => "waiting",
            AgentStatus::Done => "done",
            AgentStatus::Error => "error",
        })
    }
}
