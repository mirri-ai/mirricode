//! A running collaboration: the session, its state machine, and agent handles.

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::CollaborationMode;
use crate::protocol::{AgentMessage, AgentStatus};

/// The state machine of a collaboration session (design doc §6.2, simplified).
///
/// Serialized `snake_case`-tagged so tool output is self-describing, e.g.
/// `{"in_progress": {"round": 1}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationState {
    /// The session was created but has not started yet.
    NotStarted,
    /// The collaboration is actively progressing; `round` counts completed
    /// task-result rounds (starts at 1).
    InProgress { round: u32 },
    /// The collaboration is paused — e.g. a conflict was escalated and is
    /// awaiting arbitration, or the round-limit loop guard tripped. Paused
    /// sessions accept no regular traffic until resumed.
    Paused,
    /// The collaboration finished successfully.
    Completed,
    /// The collaboration ended in an unrecoverable state; the payload is the
    /// reason (e.g. `"max_rounds (3) exceeded"` or `"terminated"`).
    Failed(String),
}

impl fmt::Display for CollaborationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CollaborationState::NotStarted => f.write_str("not_started"),
            CollaborationState::InProgress { round } => {
                write!(f, "in_progress (round {round})")
            }
            CollaborationState::Paused => f.write_str("paused"),
            CollaborationState::Completed => f.write_str("completed"),
            CollaborationState::Failed(reason) => write!(f, "failed ({reason})"),
        }
    }
}

/// Handle to an agent participating in a collaboration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHandle {
    /// The role this agent serves, e.g. `coordinator` (equals the map key in
    /// [`CollaborationSession::agents`]).
    pub role: String,
    /// The concrete agent instance serving the role.
    pub agent_id: String,
    /// The agent's current status within the collaboration.
    pub status: AgentStatus,
}

/// A collaboration session — manages agents working together on a task.
#[derive(Debug, Clone, PartialEq)]
pub struct CollaborationSession {
    pub id: Uuid,
    pub mode: CollaborationMode,
    /// Agents participating in the collaboration, keyed by role name.
    pub agents: HashMap<String, AgentHandle>,
    /// Every message exchanged during the collaboration, in arrival order.
    pub message_log: Vec<AgentMessage>,
    pub state: CollaborationState,
    /// The shared objective the collaboration is working toward.
    pub task: String,
    pub created_at: DateTime<Utc>,
}

impl CollaborationSession {
    /// Creates a session in the [`CollaborationState::InProgress`] state with
    /// one agent handle per role in `mode`.
    ///
    /// # Preconditions
    ///
    /// `mode` must pass [`CollaborationMode::validate`] — the caller (the
    /// manager) is responsible for checking that before constructing a
    /// session. This is enforced with a `debug_assert` so that invalid modes
    /// are caught in tests and debug builds.
    pub fn new(id: Uuid, mode: CollaborationMode, task: String) -> Self {
        debug_assert!(
            mode.validate().is_ok(),
            "CollaborationSession::new requires a validated mode"
        );

        let agents = mode
            .roles
            .iter()
            .map(|role| {
                let handle = AgentHandle {
                    role: role.name.clone(),
                    agent_id: format!("{}-{}", role.name, Uuid::new_v4().simple()),
                    status: AgentStatus::Idle,
                };
                (role.name.clone(), handle)
            })
            .collect();

        Self {
            id,
            mode,
            agents,
            message_log: Vec::new(),
            state: CollaborationState::InProgress { round: 1 },
            task,
            created_at: Utc::now(),
        }
    }
}
