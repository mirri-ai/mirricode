//! Error types for session lifecycle management.
//!
//! [`SessionError`] covers all failure modes that arise during session
//! create / resume / fork / close operations, agent lifecycle operations,
//! and session persistence (wire log + state file I/O).

use thiserror::Error;

/// Errors produced by session management operations.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A session with the given ID does not exist in the workspace.
    #[error("Session not found: {0}")]
    NotFound(String),

    /// A session with the given ID already exists in the workspace.
    #[error("Session already exists: {0}")]
    AlreadyExists(String),

    /// An agent with the given ID does not exist in the session.
    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    /// An agent with the given ID already exists in the session.
    #[error("Agent already exists: {0}")]
    AgentAlreadyExists(String),

    /// A wire log (`wire.jsonl`) read or write failed.
    #[error("Wire log error: {0}")]
    WireLog(String),

    /// A session fork targeted a turn that no `turn.ended` record in the
    /// wire log matches (fork pipeline, F1).
    #[error("Fork target turn not found: {0}")]
    ForkTargetNotFound(String),

    /// A state file (`state.json`) read or write failed.
    #[error("State file error: {0}")]
    StateFile(String),

    /// A snapshot file (`snapshot.json`) read or write failed.
    #[error("Snapshot error: {0}")]
    Snapshot(String),

    /// Session resume failed — the wire log could not be replayed or the
    /// state could not be reconstructed.
    #[error("Session resume failed: {0}")]
    ResumeFailed(String),

    /// The session has already been closed and cannot accept further
    /// operations.
    #[error("Session already closed: {0}")]
    AlreadyClosed(String),

    /// An underlying I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization or deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias used throughout the session module.
pub type SessionResult<T> = Result<T, SessionError>;

impl SessionError {
    /// Map each variant to a domain [`ErrorCode`] for the wire response.
    ///
    /// Only variants with clear TS counterparts get domain-specific codes;
    /// infrastructure failures (I/O, JSON, wire-log, state-file, snapshot,
    /// agent-already-exists) collapse to [`ErrorCode::Internal`].
    pub fn code(&self) -> crate::error::ErrorCode {
        use crate::error::ErrorCode;
        match self {
            Self::NotFound(_) => ErrorCode::SessionNotFound,
            Self::AlreadyExists(_) => ErrorCode::SessionAlreadyExists,
            Self::AgentNotFound(_) => ErrorCode::AgentNotFound,
            Self::AlreadyClosed(_) => ErrorCode::SessionClosed,
            Self::ResumeFailed(_) => ErrorCode::SessionInitFailed,
            // Infrastructure failures have no domain-specific code.
            Self::AgentAlreadyExists(_)
            | Self::WireLog(_)
            | Self::ForkTargetNotFound(_)
            | Self::StateFile(_)
            | Self::Snapshot(_)
            | Self::Io(_)
            | Self::Json(_) => ErrorCode::Internal,
        }
    }
}
