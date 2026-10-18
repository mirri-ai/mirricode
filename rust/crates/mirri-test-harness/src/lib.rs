//! `mirri-test-harness` — black-box test harness for the Mirri server.
//!
//! This crate lets the same test suite run against any server speaking the
//! Mirri wire protocol (the Rust `mirri-server`, the TypeScript server, or
//! anything else implementing the same REST + WebSocket surface):
//!
//! - [`MirriHarness`] — the trait abstracting every server operation
//!   (session lifecycle, messages, tools, approvals, questions, events,
//!   metadata, health).
//! - [`HttpHarness`] — a [`MirriHarness`] implementation that talks to a
//!   server over HTTP/WS.
//! - [`MockLlm`] / [`MockLlmFactory`] — a scripted LLM provider so engine
//!   turns run in tests without real API keys: canned responses, per-token
//!   streaming with pacing, never-respond mode, and cancellation
//!   observation.
//!
//! The wire view mirrors the TypeScript harness: HTTP responses use the
//! `{code, msg, data, request_id}` envelope (see [`HarnessError`] for how
//! non-zero codes map to errors), and session events arrive through the
//! multiplexed WebSocket endpoint ([`MirriHarness::subscribe`]).

pub mod http_client;
pub mod mock_llm;
pub mod types;

use async_trait::async_trait;
use mirri_core::SessionEvent;
use tokio::sync::broadcast;

pub use http_client::HttpHarness;
pub use mock_llm::{MockLlm, MockLlmFactory, MockResponse};
pub use types::*;

/// Errors surfaced by a [`MirriHarness`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Transport-level failure (connection, timeout, malformed payload).
    #[error("HTTP error: {0}")]
    Http(String),
    /// The server answered with a non-zero `envelope.code`.
    #[error("API error: code={code} msg={msg}")]
    Api { code: i32, msg: String },
    /// The addressed resource does not exist (RPC code `-32001`).
    #[error("not found: {0}")]
    NotFound(String),
    /// Local I/O failure (not exchanged with the server).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The operations every server under test must support.
///
/// Implementations are cheap to clone and share (`Send + Sync`), so a single
/// harness can be handed to concurrent tests. Session identifiers are opaque
/// strings as returned by [`create_session`](Self::create_session).
#[async_trait]
pub trait MirriHarness: Send + Sync {
    /// Create a new session, returning its id.
    async fn create_session(&self, title: &str) -> Result<String, HarnessError>;

    /// List all sessions.
    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, HarnessError>;

    /// Get session details.
    async fn get_session(&self, session_id: &str) -> Result<SessionDetails, HarnessError>;

    /// Close a session.
    async fn close_session(&self, session_id: &str) -> Result<(), HarnessError>;

    /// Send a message to a session's agent and wait for the engine turn.
    async fn send_message(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<SendMessageResult, HarnessError>;

    /// List available tools.
    async fn list_tools(&self, session_id: &str) -> Result<Vec<ToolSummary>, HarnessError>;

    /// Subscribe to session events (returns a receiver).
    ///
    /// The returned receiver observes [`SessionEvent`]s published after the
    /// subscription is established; like the server's own event bus, a slow
    /// consumer observes `RecvError::Lagged` instead of blocking the
    /// transport. Dropping the receiver ends the subscription.
    async fn subscribe(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<SessionEvent>, HarnessError>;

    /// Resolve an approval request.
    async fn resolve_approval(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), HarnessError>;

    /// Answer a question.
    async fn answer_question(
        &self,
        session_id: &str,
        request_id: &str,
        answer: &str,
    ) -> Result<(), HarnessError>;

    /// Server metadata.
    async fn meta(&self) -> Result<ServerMeta, HarnessError>;

    /// Health check.
    async fn health(&self) -> Result<HealthStatus, HarnessError>;
}
