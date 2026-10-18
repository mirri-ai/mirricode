//! Session lifecycle management — create, resume, fork, close, list.
//!
//! This module provides the session management layer that sits above the
//! scope hierarchy ([`WorkspaceScope`] → [`SessionScope`] → [`AgentScope`]).
//!
//! ## Architecture
//!
//! ```text
//! SessionManager (per workspace)
//! ├── manages: create / resume / fork / close / list sessions
//! ├── owns: SessionPersistence (state.json + wire.jsonl paths)
//! ├── owns: per-session SessionArtifacts (event bus + approval/question gates)
//! └── delegates: session storage → WorkspaceScope
//!
//! AgentLifecycle (per session)
//! ├── manages: create / fork / remove / list agents
//! └── delegates: agent storage → SessionScope
//!
//! SessionPersistence
//! ├── computes: <sessions_dir>/<session_id>/state.json
//! ├── computes: <sessions_dir>/<session_id>/wire.jsonl
//! ├── computes: <sessions_dir>/<session_id>/snapshot.json
//! └── uses: mirri_persistence::AtomicDocument for crash-safe writes
//! ```
//!
//! ## Sub-modules
//!
//! - [`manager`] — [`SessionManager`] for session-level lifecycle.
//! - [`agent_lifecycle`] — [`AgentLifecycle`] for agent-level lifecycle.
//! - [`persistence`] — [`SessionPersistence`] for on-disk state.
//! - [`replay`] — wire-log replay (state folding + [`ContextMemory`]
//!   reconstruction) used by session resume.
//! - [`events`] — [`EventBus`] broadcast event bus and [`SessionEvent`]s.
//! - [`gates`] — [`ApprovalGate`] / [`QuestionGate`] for human-in-the-loop
//!   tool-approval and question decisions.
//! - [`interaction`] — [`InteractionKernel`], the composition view over the
//!   two gates the `interaction.*` RPC methods speak (B4-a).
//! - [`shutdown`] — [`SessionShutdown`] graceful gate draining on close.
//! - [`error`] — [`SessionError`] and [`SessionResult`].

pub mod activity_shadow;
pub mod agent_lifecycle;
pub mod agent_status;
pub mod artifacts;
pub mod consistency;
pub mod error;
pub mod event_projection;
pub use event_projection::{ServerEvent, WorkspaceSnapshot, project_server_event};
pub mod events;
pub mod fork_pipeline;
pub mod gates;
pub mod in_flight_shadow;
pub mod interaction;
pub mod journal_route;
pub mod live_relay;
pub mod manager;
pub mod persistence;
pub(crate) mod replay;
pub mod shutdown;
pub mod title;
pub mod title_llm;
pub mod turn_usage;
pub mod workdir_key;

pub use agent_lifecycle::{AgentInfo, AgentLifecycle};
pub use artifacts::SessionArtifacts;
pub use error::{SessionError, SessionResult};
pub use events::{EventBus, SessionEvent, uuid_from_id};
pub use gates::{
    AdditionalDirGrantSink, ApprovalDecision, ApprovalGate, ApprovalSnapshot, QuestionGate,
    QuestionSnapshot, ResolvedRing,
};
pub use in_flight_shadow::InFlightTurnShadow;
pub use interaction::{InteractionKernel, InteractionRespondError};
pub use manager::{
    CloseReason, ForkOptions, SessionInfo, SessionManager, SessionSource, SessionStatus,
    session_scope_config_from,
};
pub use persistence::{LocatedSessionDir, PersistedSessionInfo, SessionPersistence};
pub use replay::{reconstruct_context_memory, reconstruct_context_memory_with_blobs};
pub use shutdown::SessionShutdown;
pub use workdir_key::{encode_work_dir_key, lexically_resolve};
