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

    /// Dispose of the engine, cancelling all in-flight work.
    async fn dispose(&self) -> Result<(), EngineError>;
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
