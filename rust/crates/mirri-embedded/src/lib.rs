//! `mirri-embedded` — the in-process engine embedding for ACP and NAPI.
//!
//! This crate turns a [`SessionManager`](mirri_core::SessionManager) into an
//! [`EngineAdapter`](mirri_transport::adapter::EngineAdapter) so the engine
//! can be driven directly — no HTTP, no WebSocket, no IPC. The TUI NAPI
//! binding (`mirri-node`) and the ACP host (`mirri-acp`) both consume the
//! engine through this crate:
//!
//! ```text
//! mirri-acp / mirri-node ──► mirri-embedded (EmbeddedServer)
//!                                   │
//!                                   ▼
//!                           mirri-core (SessionManager + Agent + TurnFlow)
//! ```
//!
//! [`EmbeddedServer`] mirrors the dispatch surface of
//! [`mirri-server`](https://crates.io/crates/mirri-server)'s `ServerState`
//! (the reference wire implementation) but runs it in-process: RPC methods
//! are dispatched straight into the session manager, and session events are
//! fanned out to `EngineAdapter::subscribe_events` subscribers as
//! `event.<type>` notifications (the shared `mirri-core` event projection,
//! with a per-session `seq` and the instance `epoch` stamped on durable
//! events) — the same event view the WebSocket endpoint produces, minus the
//! per-session cursor log.
//!
//! Because `mirri-server` is deliberately not a dependency (it carries the
//! HTTP/WS stack), the [`LlmFactory`] trait used to be defined here rather
//! than shared — since M5 it lives once in `mirri-core` and is re-exported
//! through [`llm_factory`], so `mirri-test-harness`'s `MockLlmFactory`
//! implements a single trait injectable into either engine.

pub mod bootstrap;
pub mod embedded_server;
pub mod llm_factory;
pub mod model_catalog;
pub mod providers;
pub mod skill_reload;
pub mod skill_watcher;
pub mod user_mcp_pool;
pub mod workspace_registry;
pub mod workspace_trust;

/// The engine-flag id gating the skill-catalog hot-reload watchers
/// (plan Task 5): default-on since the notification surface (plan Task 6's
/// `skill.reload` RPC + the `skill.catalogChanged` event) landed. The
/// flag gates ONLY the watcher mounting — the reload orchestrator
/// itself, the config-write resync and the plugin-reload replay run
/// unconditionally (their triggers are not file events).
pub const SKILL_HOT_RELOAD_FLAG: &str = "skillHotReload";

// The files store lives in `mirri-persistence` (its production home — the
// server crate reaches it without taking an engine dependency); re-exported
// here because the `message.send` entry resolution is this crate's surface.
pub use mirri_persistence::file_store;

pub use embedded_server::EmbeddedServer;
pub use llm_factory::LlmFactory;
pub use model_catalog::ConfigModelCatalog;
pub use providers::{
    AnthropicLlm, ConfigAwareLlmFactory, LocalFetchUrlProvider, ManagedFetchConfig,
    ManagedFetchUrlProvider, ManagedSearchConfig, ManagedWebSearchProvider, OpenAILlm,
    ProviderLlmFactory,
};
pub use user_mcp_pool::{UserMcpPool, shared_user_mcp_pool_for_home};
pub use workspace_trust::WorkspaceTrustStore;
