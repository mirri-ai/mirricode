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

/// The stable error-signature prefix for CHANNEL-class reverse-RPC failures
/// (ADR-008 / D-1): connection closed, outbox closed, responder dropped —
/// failures of the TRANSPORT, as opposed to BUSINESS-class failures
/// (e.g. "not negotiated by the client", a domain-level refusal that no
/// amount of reconnecting can fix).
///
/// Error contract (ADR-008): `ReverseRpcCallback` implementors MUST
/// construct channel-class error strings with this constant; consumers
/// (e.g. the `AskUserQuestion` delivery loop) MUST classify them via
/// [`is_channel_error`] — never via scattered literal matching. A
/// channel-class error tells the consumer "the connection is gone; the
/// callback may be re-installed later", which is the signal the
/// question-delivery resilience loop waits on.
pub const CHANNEL_ERROR_PREFIX: &str = "reverse-rpc channel error: ";

/// Whether an error message from [`ReverseRpcCallback::call`] is a
/// CHANNEL-class failure (transport gone — reconnect may fix it) rather
/// than a business-class one (the request itself was refused; retrying
/// would be pointless). See [`CHANNEL_ERROR_PREFIX`] for the contract.
pub fn is_channel_error(msg: &str) -> bool {
    msg.starts_with(CHANNEL_ERROR_PREFIX)
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
