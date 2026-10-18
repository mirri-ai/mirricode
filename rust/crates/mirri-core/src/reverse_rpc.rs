//! Engine-initiated reverse RPC callback (engine → client).
//!
//! When the engine needs client-side action — permission approval or a user
//! question — it calls a [`ReverseRpcCallback`]. The transport layer (NAPI
//! `ThreadsafeFunction`, WS JSON-RPC, etc.) provides the concrete
//! implementation.
//!
//! This trait lives in `mirri-core` (not `mirri-transport`) so the engine
//! core can reference it without a circular dependency. The transport layer
//! provides the concrete implementation via a wrapper around
//! [`ReverseRpcHandler`](mirri_transport::adapter::ReverseRpcHandler).

use serde_json::Value;

/// Callback for engine-initiated reverse RPC requests (engine → client).
///
/// When the engine needs to ask the client for permission approval
/// (`approval.ask`) or to present a structured question to the user
/// (`question.ask`), it calls this trait. The concrete implementation is
/// provided by the transport layer and forwarded to the actual client.
///
/// The trait is `Send + Sync` so it can be stored in `Arc` and shared across
/// async tasks (the loop engine, tool scheduler, hooks).
#[async_trait::async_trait]
pub trait ReverseRpcCallback: Send + Sync {
    /// Call a reverse RPC method and await the result.
    ///
    /// Returns `Err(message)` if the request fails or times out.
    async fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// A mutable, shared slot holding the current reverse-RPC callback.
///
/// The transport installs the concrete callback when a client subscribes —
/// which can happen after the agents of the session were created. Tools
/// that issue reverse RPCs (e.g. `AskUserQuestion`) hold the SLOT rather
/// than a callback snapshot, so they always resolve the current value at
/// call time instead of a create-time copy that goes stale.
pub type ReverseRpcSlot =
    std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<dyn ReverseRpcCallback>>>>;
