//! Multi-agent autonomous collaboration for the Mirri kernel.
//!
//! This crate implements the autonomous collaboration mode described in
//! `docs/superpowers/specs/2026-08-28-autonomous-collaboration-design.md`:
//! several agents in different roles communicate with one another, autonomously
//!推进 a shared task, and only escalate to a human when the built-in
//! arbitration cannot resolve a conflict.
//!
//! Layering (each layer depends only on the ones below it):
//!
//! - [`domain`] — the user-facing collaboration model: [`CollaborationMode`],
//!   [`RoleDefinition`], and [`CollaborationRules`] (communication pattern,
//!   intervention policy, conflict resolution, round limit).
//! - [`protocol`] — the wire protocol between agents: [`AgentMessage`],
//!   [`TaskStatus`], [`AgentStatus`]. Messages are JSON with a `type` tag, so a
//!   journal written today stays readable when new message kinds are added
//!   (unknown tags deserialize into [`AgentMessage::Unknown`]).
//! - [`session`] — a running collaboration: [`CollaborationSession`] holds the
//!   mode, one [`AgentHandle`] per role, the recorded [`AgentMessage`] log, and
//!   the [`CollaborationState`] state machine.
//! - [`manager`] — the [`CollaborationManager`]: starts sessions and mediates
//!   every protocol operation with validation (known participants, legal state
//!   transitions, round-limit guardrail).

#![forbid(unsafe_code)]

pub mod domain;
pub mod error;
pub mod manager;
pub mod protocol;
pub mod session;

pub use domain::{
    CollaborationMode, CollaborationRules, CommunicationPattern, ConflictResolution,
    InterventionPolicy, RoleDefinition,
};
pub use error::{CollaborationError, CollaborationResult};
pub use manager::CollaborationManager;
pub use protocol::{AgentMessage, AgentStatus, TaskStatus};
pub use session::{AgentHandle, CollaborationSession, CollaborationState};
