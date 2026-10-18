//! Error types for the collaboration domain.

use uuid::Uuid;

use crate::session::CollaborationState;

/// Result alias used across the collaboration crate.
pub type CollaborationResult<T> = Result<T, CollaborationError>;

/// Errors raised by the collaboration manager and its protocols.
#[derive(Debug, thiserror::Error)]
pub enum CollaborationError {
    /// The referenced collaboration session does not exist (or was never started).
    #[error("collaboration session {session_id} not found")]
    SessionNotFound { session_id: Uuid },

    /// A message or directive named an agent (sender, recipient, or conflict
    /// party) that is not part of the session.
    #[error("agent '{agent}' is not part of the collaboration session")]
    AgentNotFound { agent: String },

    /// The operation is not permitted in the session's current state, e.g.
    /// sending messages after the session was paused, terminated, or completed.
    #[error("cannot {operation} while the collaboration is {state}")]
    InvalidState {
        operation: &'static str,
        state: CollaborationState,
    },

    /// The collaboration mode definition violates the design-doc validation
    /// rules (§5.2): duplicate roles, empty role list, nonsensical limits, ...
    #[error("invalid collaboration mode: {0}")]
    InvalidMode(String),

    /// A message violates the inter-agent protocol, e.g. it came from an
    /// unknown message type that cannot be routed between agents.
    #[error("protocol violation: {0}")]
    ProtocolViolation(String),
}
