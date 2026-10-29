//! EngineAdapter trait — unified engine interface regardless of transport.
//!
//! The trait lives in `mirri-transport` (not `mirri-core`) because it
//! references transport-layer types (`JsonRpcFrame`, `JsonRpcNotification`).
//! `mirri-core` defines the domain logic; `mirri-transport` bridges it to
//! the wire.
//!
//! The real engine implementation lives in `mirri-embedded`
//! (`EmbeddedServer`); this crate defines only the trait surface that
//! bridges the engine to the wire.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::frame::JsonRpcNotification;

/// Handler for reverse RPC requests (server → client).
pub type ReverseRpcHandler = Arc<
    dyn Fn(
            String,
            Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

/// Unified engine interface.
///
/// All three frontends (TUI/Web/ACP) interact with the engine through this
/// trait, regardless of whether the engine is embedded or remote.
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    /// Call an RPC method and await the result.
    async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError>;

    /// Subscribe to engine events. Returns a receiver of notifications.
    ///
    /// The notifications are JSON-RPC notifications with `event.<type>`
    /// methods and `params` containing the flat event payload merged into
    /// the envelope (see `mirri-core`'s event projection).
    async fn subscribe_events(&self) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError>;

    /// Register a handler for reverse RPC requests.
    ///
    /// When the engine needs client-side action (approval, question, fs
    /// proxy, terminal), it sends a reverse RPC request. The handler is
    /// called with the method name and params, and must return a result.
    async fn on_reverse_rpc(&self, handler: ReverseRpcHandler) -> Result<(), EngineError>;

    /// Subscribe to the engine's app-level MCP pool events (M7, F-9): the
    /// `event.mcp.server.status` payloads published when any pool's server
    /// changes state or config — independent of any live session, so a
    /// serve-wire connection can observe the pool surface without holding a
    /// session subscription.
    ///
    /// The broadcast carries the full notification `params` shape
    /// (`{"server": {name, transport, status, toolCount, pool, changeKind?,
    /// error?}}` — the session-level projection's payload without the
    /// session envelope). Engines without an app-level MCP surface answer
    /// `MethodNotFound`; the embedded server overrides this with its
    /// pool-status broadcast.
    fn subscribe_mcp_pool_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<std::sync::Arc<serde_json::Value>>, EngineError>
    {
        Err(EngineError::MethodNotFound("mcp.subscribe".to_string()))
    }

    /// Subscribe to the engine's app-level skill-catalog change events
    /// (ADR-010 Task 6): the `skill.catalogChanged` payloads broadcast when
    /// any workspace's skill catalog changes on a rescan (file watcher,
    /// config write, plugin reload, or the manual `skill.reload` RPC) —
    /// independent of any live session, so a serve-wire connection can
    /// observe the catalog surface without holding a session subscription.
    ///
    /// The broadcast carries the full notification `params` shape
    /// (`{workspaceId, added: string[], removed: string[], updated:
    /// string[], trigger}` — the name lists; counts derive from the array
    /// lengths). Engines without a skill-catalog surface answer
    /// `MethodNotFound`; the embedded server overrides this with its
    /// skill-reload orchestrator's broadcast.
    fn subscribe_skill_catalog_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<serde_json::Value>, EngineError> {
        Err(EngineError::MethodNotFound(
            "skill.catalogChanged".to_string(),
        ))
    }

    /// The sessions that currently have an engine turn in flight — any
    /// dispatch origin (user prompts, parked-notification reactivations,
    /// cron). Transport read faces (`session.status` / `session.snapshot` /
    /// `session.get`) union this with their own fire-mode registry so
    /// driver-injected turns are not reported idle. Default: none
    /// (adapters without an engine-side turn registry).
    async fn in_flight_sessions(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether this adapter is a remote client of a standalone server
    /// (design 14 D2/D6's "remote mode" signal): the ACP host routes
    /// cwd → `workspaceId` before session-creating calls and pins its
    /// `session/list` to the host start root's workspace ONLY through a
    /// remote adapter. Default: `false` (an embedded engine needs no
    /// translation — it binds the workspace itself).
    fn is_remote(&self) -> bool {
        false
    }

    /// Release this connection's subscription to a session (design 14
    /// §D5: 会话关闭即退订) — only a remote adapter has a server-side
    /// subscription to release, so the default is a no-op and
    /// `RemoteAdapter` overrides it by sending `session.unsubscribe`.
    /// The close that follows must not be blocked by this call's outcome.
    async fn unsubscribe_session(&self, _session_id: &str) -> Result<(), EngineError> {
        Ok(())
    }

    /// The sessions with an unfinished turn right now, with the identity
    /// and interaction fields the graceful-exit record needs
    /// (`graceful.json`, ADR-001) — a snapshot taken on the shutdown path
    /// BEFORE the drain. Sessions waiting on a user decision
    /// (`pending_interaction`) are reported so the writer can exclude them
    /// (D-M6: waiting-on-user is not an interruption). Default: none
    /// (adapters without an engine-side turn registry).
    async fn snapshot_running_sessions(&self) -> Vec<RunningSessionSnapshot> {
        Vec::new()
    }

    /// Dispose of the engine, cancelling all in-flight work.
    async fn dispose(&self) -> Result<(), EngineError>;
}

/// One entry of the engine's running-session snapshot (the graceful-exit
/// record's input, spec §5): a session with a turn in flight at the
/// shutdown moment, plus the fields the record carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningSessionSnapshot {
    /// The session identifier.
    pub session_id: String,
    /// The owning workspace's identifier.
    pub workspace_key: String,
    /// The session's display title.
    pub title: String,
    /// Epoch-milliseconds timestamp of the session's last visible
    /// activity — the most truthful per-session recency the engine can
    /// offer at the shutdown moment (the live `last_used_at`/`updated_at`
    /// of the session's row), NOT the shutdown call time. Maps to the
    /// graceful record's `interrupted_at` (spec §1.3: "最后一次活动可见事实").
    pub last_activity_ms: u64,
    /// Whether the session is waiting on a user decision (approval or
    /// question) — such sessions are NOT interrupted by a graceful exit.
    pub pending_interaction: bool,
}

/// Engine-level errors.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("rpc error: code={code} message={message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("engine not connected")]
    NotConnected,
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}
