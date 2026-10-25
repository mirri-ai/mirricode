//! [`EmbeddedServer`] — an in-process engine implementing
//! [`EngineAdapter`] on top of a bare [`SessionManager`].
//!
//! The dispatch mirrors `mirri-server`'s RPC handlers (the reference wire
//! implementation) but runs them directly against `mirri-core`, without the
//! HTTP/WS/IPC layers.
//!
//! ## Method surface
//!
//! Phase 4.4 expanded the surface from 5 methods to the full 94-method
//! `CoreAPI` of the TS client (`packages/node-sdk/src/sdk-rpc-client-rust.ts`);
//! every TS wire name dispatches to a handler here — 94/94 (100%, target
//! 80%+). The dispatch accepts **both naming conventions**: the TS client's
//! camelCase wire names (`createSession`, `prompt`, `getTools`, …) and the
//! dotted protocol names (`session.create`, `message.send`, `tool.list`, …).
//! Parameter fields are likewise accepted in both spellings (`sessionId` and
//! `session_id`, `agentId` and `agent_id`, `turnId` and `turn_id`).
//!
//! Implemented against real `mirri-core` services:
//!
//! - Session lifecycle: `session.create` (provisions the default main agent,
//!   see below), `session.list`, `session.resume`, `session.close`,
//!   `session.reload`, `session.fork`, `session.rename`, `session.updateMeta`,
//!   `session.getMeta`, `session.export`, `session.warnings`
//! - Agent/turn: `message.send` (wait/fire), `message.abort` (cancels
//!   in-flight fire-mode turns), `message.steer`, `agent.getContext`,
//!   `agent.getUsage`, `agent.getModel`, `agent.getPermission`,
//!   `agent.getConfig`, `agent.setActiveTools`, `session.undo`
//! - Task/cron: `task.list`, `task.cancel`, `task.output`, `cron.list`
//! - Plan/goal/swarm: `plan.enter`/`plan.cancel`/`plan.clear`/`plan.status`,
//!   `goal.create`/`goal.get`/`goal.pause`/`goal.resume`/`goal.cancel`,
//!   `swarm.enter`/`swarm.exit`/`swarm.get`,
//!   `collaboration.enter`/`collaboration.exit`
//! - Skills: `skill.list`, `agent.activateSkill`
//!
//! Everything else in the TS surface is either genuinely implemented
//! (plugins, MCP, compaction, agent reconfiguration, …) or answers with an
//! explicit `not_implemented` error (`domainCode: "not_implemented"` in the
//! error `data` — see `not_implemented()`), never
//! [`EngineError::MethodNotFound`], which is reserved for methods outside
//! the embedded dispatch surface (the exact, registry-driven count is
//! pinned by the route-table tests; the Task 7 session-retention batch
//! added the six `retention.*` management routes — five of them answer
//! `not_implemented` until Task 9 lands their handlers).
//!
//! ## Difference from `mirri-server::session.create`
//!
//! The server creates agent-less sessions and relies on an out-of-band agent
//! creation step (its e2e tests call `SessionScope::create_agent` through the
//! session manager directly — there is no agent-creation RPC). Embedded
//! consumers (ACP hosts, the NAPI TUI) have no such out-of-band channel, so
//! the embedded `session.create` provisions the default main agent
//! (`main`, the same id `SessionManager::resume_session` recreates)
//! itself, making every created session immediately promptable.
//!
//! ## Events
//!
//! Every subscriber obtained through [`EngineAdapter::subscribe_events`]
//! receives a single stream of `event.<type>` notifications — the shared
//! projection of `mirri-core`'s [`SessionEvent`](mirri_core::SessionEvent)s
//! (flat camelCase payload fields merged into the envelope params, plus a
//! per-session `seq` and the instance `epoch` on durable events) — covering
//! **all** sessions; each notification carries the emitting session's id.
//! Engine turns additionally surface
//! [`SessionEvent::AgentMessageChunk`] events for every incremental
//! assistant `TextDelta`, so UIs can stream replies token-by-token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use mirri_core::{
    AgentId, AgentScope, CancelToken, ContentPart, EngineError as CoreEngineError, Goal,
    GoalManager, ModelCatalog, ProfileManager, ReverseRpcCallback, ServerConfig, ServerEvent,
    SessionActivityState, SessionInfo, SessionManager, SessionPendingInteraction, SessionScope,
    SessionTurnOutcome, SteerBuffer, TokenUsage, TurnEndResult, TurnFlow, TurnFlowConfig,
    TurnStopReason, WorkspaceId, activity_view::AgentActivityState, compaction::FullCompaction,
    event_projection, permission::PermissionRule, session::agent_status::AgentStatusPublisher,
    turn_flow::ActivityTracker,
};
use mirri_os::SyncFs as _;
use mirri_proto::WireOp;
use mirri_transport::adapter::{EngineAdapter, EngineError, ReverseRpcHandler};
use mirri_transport::frame::JsonRpcNotification;

use crate::LlmFactory;
use crate::bootstrap::{EngineParts, WorkspacePolicy, assemble};
use crate::workspace_registry::{WorkspaceRegistry, WorkspaceRuntime};

use self::responses::{
    CommandOutputResult, McpConnectResult, McpServerState, McpStartupMetricsResult,
    McpTestConnectResult, McpToggleStateResult, ReloadSummaryResult, ack, to_result,
};

// Items used by the crate-local tests (they glob `super::*` from
// `embedded_server/tests.rs`), not by this module's own code.
#[cfg(test)]
use mirri_core::AgentScopeConfig;
#[cfg(test)]
use mirri_core::SessionId;
#[cfg(test)]
use mirri_core::{WorkspaceScope, WorkspaceScopeConfig};
#[cfg(test)]
use std::time::Duration;

/// Capacity of the per-subscriber event channel handed out by
/// [`EngineAdapter::subscribe_events`]. A subscriber that does not drain the
/// channel fast enough is dropped by the relay (the documented event-stream
/// backpressure: slow consumers are resync'd, never blocking the engine).
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the server-scoped lifecycle event broadcast (workspace
/// create/touch/rename/delete) — the same bounded-broadcast philosophy as
/// the per-session event buses: a subscriber that lags is lagged, never
/// blocks the emitter.
const SERVER_EVENT_CAPACITY: usize = 256;

/// The agent id provisioned by the embedded `session.create` — the same id
/// `SessionManager::resume_session` recreates for resumed sessions.
///
/// Matches the TS SDK's `MAIN_AGENT_ID = 'main'` so that `agentId: 'main'`
/// from the client resolves to this agent without any aliasing layer.
const DEFAULT_MAIN_AGENT_ID: &str = "main";

/// The models.dev directory endpoint the
/// `providerDiscovery.refreshProviderModels` pull fetches (the TS upstream
/// constant `MODELS_DEV_URL`; tests inject a wiremock URL through
/// [`EmbeddedServer::with_models_dev_url`]).
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Build the flag registry with the engine's own gate flags registered and
/// the config's `[experimental]` section seeded as the config layer.
///
/// The engine flags today are [`mirri_core::HOOK_COMMAND_REWRITE_FLAG`]
/// (TS parity: the TS registry's `hook-command-rewrite` entry, same env and
/// same `false` default) and
/// [`mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG`] (the session-consistency
/// follow driver's gate, batch 1). The runtime gate resolves env → config →
/// defaults (the TS `FlagService` chain), so a config `[experimental]`
/// override enables the flag without any environment variable. Config
/// changes later in the server's life re-seed the layer (see
/// `dispatch_set_mirri_config`). Also used by the `mirri doctor` CLI to
/// determine which experimental flags the engine registers.
pub fn new_engine_flag_registry(
    config: &std::sync::RwLock<ServerConfig>,
) -> Arc<mirri_core::FlagRegistry> {
    let registry = Arc::new(mirri_core::FlagRegistry::new());
    registry.register(mirri_core::FlagDef {
        id: mirri_core::HOOK_COMMAND_REWRITE_FLAG.to_string(),
        default: false,
        description: "Allow PreToolUse hooks to return updatedInput rewriting tool arguments before execution (e.g. rtk integration). Security risk: a malicious hook can transparently rewrite commands.".to_string(),
        title: None,
        surface: mirri_core::FlagSurface::Core,
        env_name: None,
    });
    registry.register(mirri_core::FlagDef {
        id: mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG.to_string(),
        default: false,
        description: "Spawn a per-session follow driver on resume that incrementally absorbs foreign appends to the session's main journal (multi-instance read consistency, batch 1).".to_string(),
        title: None,
        surface: mirri_core::FlagSurface::Core,
        env_name: None,
    });
    registry.register(mirri_core::FlagDef {
        id: crate::SKILL_HOT_RELOAD_FLAG.to_string(),
        default: false,
        description: "Mount per-workspace skill directory watchers that hot-reload the skill catalog on file changes (ADR-010). Off by default while the feature settles; enable via the experimental flags layer. The manual reload (skill.reload RPC) stays available either way.".to_string(),
        title: None,
        surface: mirri_core::FlagSurface::Core,
        env_name: None,
    });
    let experimental = config
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .experimental
        .clone()
        .unwrap_or_default();
    registry.set_config_overrides(experimental);
    registry
}

/// The engine's registered experimental-flag ids (the snapshot the doctor
/// CLI passes as `known_flags`).
pub fn engine_flag_registry_ids() -> Vec<String> {
    vec![
        mirri_core::HOOK_COMMAND_REWRITE_FLAG.to_string(),
        mirri_core::SESSION_CONSISTENCY_FOLLOW_FLAG.to_string(),
        crate::SKILL_HOT_RELOAD_FLAG.to_string(),
    ]
}

/// How long the models.dev fetch may take before it is treated as failed
/// (the TS `UPSTREAM_FETCH_TIMEOUT_MS`).
const MODELS_DEV_FETCH_TIMEOUT_SECS: u64 = 10;

/// How long the manual `skill.reload` waits for outstanding watcher
/// mounts before answering (an NFS-hung home must not hang the RPC).
const MOUNT_AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// JSON-RPC error codes and the domain-code → JSON-RPC-code mapping come
// from `mirri-proto`'s `rpc` module — the single fact source for the
// contract (plan decision D5) — instead of hand-copied constants.

/// Treat a missing/`null` params value as an empty object so callers may
/// omit the parameters entirely.
fn normalize_params(params: Value) -> Value {
    if params.is_null() { json!({}) } else { params }
}

/// Build an `invalid params` engine error.
fn invalid_params(message: impl Into<String>) -> EngineError {
    EngineError::Rpc {
        code: mirri_proto::rpc::JSON_RPC_INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

/// Build a `not found` engine error.
fn not_found(message: impl Into<String>) -> EngineError {
    EngineError::Rpc {
        code: mirri_proto::rpc::JSON_RPC_NOT_FOUND,
        message: message.into(),
        data: None,
    }
}

/// Build a `not found` engine error carrying a specific domain code (the
/// TS `ModelCatalogErrors` codes — `provider.not_found` / `model.not_found`
/// — the wire contract branches on).
fn not_found_domain(domain_code: &str, message: impl Into<String>) -> EngineError {
    EngineError::Rpc {
        code: mirri_proto::rpc::JSON_RPC_NOT_FOUND,
        message: message.into(),
        data: Some(mirri_proto::rpc::domain_error_data(domain_code)),
    }
}

/// Wrap a [`CoreEngineError`] as an engine RPC error carrying its domain
/// code (e.g. `"provider.api_error"`) in the `data` field.
fn engine_error(core_error: CoreEngineError) -> EngineError {
    let domain_code = core_error.code().as_str();
    let mut data = mirri_proto::rpc::domain_error_data(domain_code);
    // The quota-exhausted error's `Retry-After` hint rides the data payload
    // (P11a F-P8-13) — the TS `APIProviderQuotaExhaustedError` parity, so
    // clients surface "retry after N" without re-parsing the raw message.
    if let CoreEngineError::ProviderQuotaExhausted {
        retry_after_ms: Some(retry_after_ms),
        ..
    } = &core_error
        && let Some(object) = data.as_object_mut()
    {
        object.insert("retryAfterMs".to_string(), json!(retry_after_ms));
    }
    EngineError::Rpc {
        // `request.invalid` stays a client error (-32602); everything else
        // is internal (-32603) — the shared mapping decides (D5).
        code: mirri_proto::rpc::json_rpc_code_for_domain_code(domain_code),
        message: core_error.to_string(),
        data: Some(data),
    }
}

/// Build a `not implemented` engine error for a known-but-unimplemented
/// surface method.
///
/// Carries the `not_implemented` domain code (D5: JSON-RPC `-32603`,
/// non-retryable — matching the TS `NOT_IMPLEMENTED` code and
/// `MIRRI_ERROR_INFO.not_implemented` metadata), so clients can distinguish
/// a retired/unimplemented method from a genuinely unknown one
/// (`EngineError::MethodNotFound`).
fn not_implemented(message: impl Into<String>) -> EngineError {
    engine_error(CoreEngineError::NotImplemented(message.into()))
}

/// Wrap a [`mirri_core::SessionError`] as an engine RPC error carrying its
/// domain code (e.g. `"session.not_found"`) in the `data` field.
///
/// The code and payload come from the shared `mirri-proto` mapping (D5):
/// `session.not_found` / `agent.not_found` keep the `-32001` not-found code
/// so clients that branch on it continue to work; all other variants use
/// `-32603`, preserving the `SessionError` domain codes on the wire instead
/// of flattening them into a generic internal error.
fn session_error(session_error: mirri_core::SessionError) -> EngineError {
    // A fork anchor that does not exist is a caller-correctable error, not
    // an internal one: the request names a turn the log does not carry
    // (F2) — the shared mapping has no domain code for it, so it keeps the
    // `request.invalid` client code (-32602) with its range-bearing
    // message instead of collapsing to internal.
    if let mirri_core::SessionError::ForkTargetNotFound(_) = &session_error {
        return invalid_params(session_error.to_string());
    }
    let domain_code = session_error.code().as_str();
    EngineError::Rpc {
        code: mirri_proto::rpc::json_rpc_code_for_domain_code(domain_code),
        message: session_error.to_string(),
        data: Some(mirri_proto::rpc::domain_error_data(domain_code)),
    }
}

/// Map a [`mirri_plugin_host::PluginError`] to an engine RPC error with an
/// appropriate JSON-RPC code.
///
/// Not-found and command-not-found failures map to `-32001` (not found);
/// user-correctable failures (manifest, install, path, already installed)
/// map to `-32602` (invalid params); template, I/O and JSON failures map to
/// `-32603` (internal).
fn plugin_error_to_engine(e: mirri_plugin_host::PluginError) -> EngineError {
    match e {
        mirri_plugin_host::PluginError::NotFound(id) => {
            not_found(format!("plugin not found: {id}"))
        }
        mirri_plugin_host::PluginError::CommandNotFound(name) => {
            not_found(format!("plugin command not found: {name}"))
        }
        mirri_plugin_host::PluginError::McpServerNotFound(name) => not_found_domain(
            "mcp.server_not_found",
            format!("plugin MCP server not found: {name}"),
        ),
        mirri_plugin_host::PluginError::AlreadyInstalled(id) => {
            invalid_params(format!("plugin already installed: {id}"))
        }
        mirri_plugin_host::PluginError::InvalidManifest(msg) => {
            invalid_params(format!("invalid plugin manifest: {msg}"))
        }
        mirri_plugin_host::PluginError::InstallFailed(msg) => {
            invalid_params(format!("plugin install failed: {msg}"))
        }
        mirri_plugin_host::PluginError::InstallRollbackFailed { install, rollback } => {
            // The TS `AggregateError` from `PluginManager.install` (A2): an
            // upgrade failure whose rollback also failed.
            invalid_params(format!(
                "plugin install failed (and the previous copy could not be \
                 restored): install error: {install}; rollback error: {rollback}"
            ))
        }
        mirri_plugin_host::PluginError::Template(msg) => {
            EngineError::Internal(format!("plugin template error: {msg}"))
        }
        mirri_plugin_host::PluginError::PathOutsideRoot(p) => {
            invalid_params(format!("plugin path outside root: {p}"))
        }
        mirri_plugin_host::PluginError::Io(e) => {
            EngineError::Internal(format!("plugin I/O error: {e}"))
        }
        mirri_plugin_host::PluginError::Json(e) => {
            EngineError::Internal(format!("plugin JSON error: {e}"))
        }
    }
}

/// Truncate long message content to a bounded event summary.
fn summarize(text: &str, max_chars: usize) -> String {
    let mut truncated: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

/// Concatenate the text parts of a prompt into one display string (used for
/// event summaries and error messages).
fn prompt_text(prompt: &[ContentPart]) -> String {
    prompt
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolve the agent addressed by an optional `agentId`, defaulting to the
/// session's first agent — the same addressing rule `message.send` uses.
/// Returns `None` when the named agent does not exist or the session has no
/// agents at all.
async fn resolve_session_agent(
    session: &SessionScope,
    agent_id: Option<&str>,
) -> Option<Arc<AgentScope>> {
    match agent_id {
        Some(id) => session.get_agent(&AgentId::new(id)).await,
        None => match session.list_agents().await.into_iter().next() {
            Some(first) => session.get_agent(&first).await,
            None => None,
        },
    }
}

/// The shared rejection a model-carrying entry point answers when addressed
/// at a subagent (spec 11 §2.4 / ADR-006): a model switch is a session-level
/// record with no agent attribution on the wire, so the only legal target is
/// the session's main agent — a subagent's model is pinned at dispatch time
/// (A14) and a child-addressed switch would fold onto the main agent at
/// resume (A17). Both model-carrying entry points that accept an `agentId`
/// (`agent.setModel` and `message.send`'s model param) answer with this one
/// error so the guidance stays identical wherever the combination fires.
pub(crate) fn subagent_model_target_rejection(agent_id: &str) -> EngineError {
    engine_error(CoreEngineError::RequestInvalid(format!(
        "model switching is session-scoped and targets the session's main agent; subagent \
         '{agent_id}' keeps its model pinned at dispatch time — drop the agentId to switch the \
         session model, or drop the model param to keep addressing the subagent"
    )))
}

/// Build a [`TurnFlowConfig`] from the server's config snapshot and the
/// agent's model window. M1 wiring: `[loop_control]` was parse-only.
pub(crate) fn turn_flow_config_from(
    server_config: &ServerConfig,
    max_context_tokens: u64,
) -> TurnFlowConfig {
    let mut config = TurnFlowConfig::default();
    config.compaction_strategy.max_context_tokens = max_context_tokens;
    if let Some(lc) = &server_config.loop_control {
        // An absent key keeps the default (unlimited — TS parity); an
        // explicit value takes effect, including 0 as a no-op spelling
        // of unlimited.
        if let Some(steps) = lc.max_steps_per_turn {
            config.max_steps = Some(steps);
        }
        if let Some(retries) = lc.max_retries_per_step {
            config.max_retry_attempts = retries;
        }
        if let Some(reserved) = lc.reserved_context_size {
            config.compaction_strategy.reserved_context_size = reserved;
        }
        if let Some(ratio) = lc.compaction_trigger_ratio {
            config.compaction_strategy.trigger_ratio = ratio;
        }
        if let Some(ratio) = lc.retention_tail_ratio {
            config.compaction_strategy.retention_tail_ratio = ratio;
        }
        if let Some(tokens) = lc.retention_tail_tokens {
            config.compaction_strategy.retention_tail_tokens = Some(tokens);
        }
        // An absent key keeps the strategy's default of 3 overflow retries
        // (a partial `[loop_control]` section must not change it); an
        // explicit 0 means "fail on the first overflow".
        if let Some(attempts) = lc.max_overflow_compaction_attempts {
            config.compaction_strategy.max_overflow_compaction_attempts = attempts;
        }
        // Resume-time capacity caps: an absent key keeps the strategy's
        // 10 000 / 1 000 production defaults; an explicit 0 is legal (a
        // fully-rejecting cap). The turn flow applies them via
        // `set_limits` every turn, and the resume / close-time-snapshot
        // fold simulations read them from the strategy through
        // `ReplayLimits`.
        if let Some(cap) = lc.max_history_messages {
            config.compaction_strategy.max_history_messages = cap as usize;
        }
        if let Some(cap) = lc.max_deferred_messages {
            config.compaction_strategy.max_deferred_messages = cap as usize;
        }
    }
    config
}

/// The max context window for `model_id` from the model catalog behind
/// `server_config` (0 = unknown — ratio triggers stay off until an
/// overflow teaches the engine the real limit). The catalog takes its
/// own config read and drops it before returning, so callers are free
/// to acquire the config lock afterwards.
pub(crate) fn max_context_window_of(
    server_config: &Arc<RwLock<ServerConfig>>,
    model_id: &str,
) -> u64 {
    let catalog = crate::model_catalog::ConfigModelCatalog::new(Arc::clone(server_config));
    catalog
        .get_by_id(model_id)
        .and_then(|entry| entry.max_context_size)
        .filter(|size| *size > 0)
        .map(|size| size as u64)
        .unwrap_or(0)
}

/// [`EmbeddedServer::turn_flow_config_for`](EmbeddedServer) for call sites
/// that hold the shared server config but not the server itself (the cron
/// driver's spawned task): the loop-control limits plus the target
/// model's catalog window, the same construction the `&self` method
/// applies. The reads are strictly sequential — the catalog's config
/// read settles before the loop-control snapshot read opens, so no
/// writer queued between them can deadlock.
pub(crate) fn turn_flow_config_for_shared(
    server_config: &Arc<RwLock<ServerConfig>>,
    model_id: &str,
) -> TurnFlowConfig {
    let window = max_context_window_of(server_config, model_id);
    let guard = server_config.read().unwrap_or_else(|e| e.into_inner());
    turn_flow_config_from(&guard, window)
}

/// Serialize a [`TokenUsage`] snapshot with camelCase Rust-wire keys
/// (`inputTokens`/`outputTokens`/`cacheReadTokens`/`cacheCreationTokens`).
/// This is the shape the session view's `usage` block reports (ADR-012:
/// kept Rust-spelled — no TS consumer reads it). `agent.getUsage` does NOT
/// use this function; it answers the TS `TokenUsage` spelling via
/// [`token_usage_ts_json`].
fn token_usage_to_json(usage: &TokenUsage) -> Value {
    json!({
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
        "cacheReadTokens": usage.cache_read_tokens,
        "cacheCreationTokens": usage.cache_creation_tokens,
    })
}

/// Serialize a [`TokenUsage`] snapshot with the TS `TokenUsage` wire keys
/// (`inputOther`/`output`/`inputCacheRead`/`inputCacheCreation`) — the
/// `agent.getUsage` response spelling, matching the TS kernel's
/// `event_projection::wire_token_usage` mapping (ADR-012).
fn token_usage_ts_json(usage: &TokenUsage) -> Value {
    json!({
        "inputOther": usage.input_tokens,
        "output": usage.output_tokens,
        "inputCacheRead": usage.cache_read_tokens,
        "inputCacheCreation": usage.cache_creation_tokens,
    })
}

/// Serialize a goal to the TS `GoalSnapshot` wire shape — the core
/// [`mirri_core::goal_snapshot_wire`] (moved down from this module so the
/// agent-side goal tools and the RPC handlers share one serializer).
fn goal_snapshot_json(goal: &Goal, manager: &GoalManager) -> Value {
    mirri_core::goal_snapshot_wire(goal, manager)
}

/// The four-bucket token usage as the wire `SessionView.usage` carries it —
/// deliberately Rust-spelled camelCase, distinct from `agent.getUsage`'s TS
/// `TokenUsage` spelling (ADR-012: no TS consumer reads the session view's
/// bucket keys).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// The session's first LLM call — the cold-start baseline (SDR-004).
    /// Absent before any call or for history predating the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_call: Option<SessionFirstCallView>,
}

impl From<&mirri_core::TokenUsage> for SessionUsageView {
    fn from(usage: &mirri_core::TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            first_call: None,
        }
    }
}

impl From<mirri_core::TokenUsage> for SessionUsageView {
    fn from(usage: mirri_core::TokenUsage) -> Self {
        Self::from(&usage)
    }
}

/// The first LLM call's four-bucket usage as the wire `SessionView.usage`
/// carries it — the cold-start baseline (SDR-004), same Rust-spelled
/// camelCase as [`SessionUsageView`].
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFirstCallView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl From<&mirri_core::TokenUsage> for SessionFirstCallView {
    fn from(usage: &mirri_core::TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        }
    }
}

/// Wire view of a session (`camelCase`, matching the TS server).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub agent_count: usize,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether a turn of this session is in flight (any agent).
    pub busy: bool,
    /// The work-facts tuple (US4.4/SDR-013): the main conversation's turn is
    /// in flight — the sidebar "running" indicator's input. Background work
    /// and subagent turns do NOT set it.
    pub main_turn_active: bool,
    /// The pending-human-interaction kind ("none"/"approval"/"question").
    pub pending_interaction: String,
    /// The main agent's latest turn outcome; absent before any turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<String>,
    /// The first agent's model id (empty string before any agent exists —
    /// a plain String keeps every list row shape-identical).
    pub model: String,
    /// The session's context root — the session's OWN workspace's root
    /// (W4: the routed create's manager cwd, recorded as `work_dir`).
    /// Echoed on the view (P11a F-P9-8) so clients can resolve
    /// workspace-relative paths without a second lookup (the desktop
    /// bridge's "open in editor" tier); absent only on views built
    /// without a live workspace (none today — the From arm below
    /// post-fills it at the dispatch site).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether the session is archived (the persisted `state.json` flag,
    /// TS parity). `false` for live sessions except those resumed from an
    /// archived state without `session.restore`; the web maps
    /// `archived ?? false`.
    pub archived: bool,
    /// The session's aggregated token usage (four buckets) across every
    /// agent — main and subagents alike (SDR-002 full aggregation).
    pub usage: SessionUsageView,
    /// The first agent's context message count.
    pub message_count: usize,
    /// The session's last durable event seq (0 before any durable event).
    pub last_seq: u64,
    /// The text of the session's most recent **user** input, when it ever
    /// received one (SDR-004: regardless of the turn's outcome — "the
    /// conversation failed" is not empty). Carried on the list rows (the
    /// wire `Session.last_prompt`, TS `toWireSession`); `None` for a
    /// never-conversed session (skipped on the wire, the web maps it onto
    /// `AppSession.lastPrompt`'s optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    /// When the session was last "used" (SDR-014 / event-delivery
    /// ADR-003): the ONLY recency key the sidebar sorts on, bumped by a
    /// user send or a `session.touch` — never by a turn settling. `None`
    /// on rows predating the field (and on closed index rows); the reader
    /// falls back to `updatedAt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    /// Whether the session's wire log is gzip-compressed on disk — the
    /// ADR-007 shape verdict for persisted rows (the state.json marker
    /// reconciled against the disk shape), always `false` for live
    /// sessions (ADR-004: a working session is never compressed). The
    /// listing/search "已压缩" badge's input (US2.1 第 4 条, plan Task 10).
    #[serde(default)]
    pub compressed: bool,
    /// When the wire log was compressed (`compressedAt`), when recorded —
    /// `None` for live sessions and never-compressed rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_at: Option<String>,
}

impl From<&SessionInfo> for SessionView {
    fn from(info: &SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            title: info.title.clone(),
            created_at: info.created_at,
            updated_at: info.updated_at,
            agent_count: info.agent_count,
            tags: info.tags.clone(),
            status: Some(match info.status {
                mirri_core::SessionStatus::Running => "running".to_string(),
                mirri_core::SessionStatus::Closed => "closed".to_string(),
            }),
            // `SessionInfo` is a static summary (no live scope to read the
            // agents from), so the runtime fields carry neutral values. The
            // cwd is post-filled at the dispatch site (the engine binds one
            // workspace, so it is known even for persisted-only sessions).
            busy: false,
            // Neutral work-facts tuple: a persisted-only summary has no live
            // scope to read the activity shadow from, so the fields carry
            // their resting values.
            main_turn_active: false,
            pending_interaction: "none".to_string(),
            last_turn_reason: None,
            model: String::new(),
            usage: SessionUsageView::default(),
            message_count: 0,
            last_seq: 0,
            cwd: None,
            archived: info.archived,
            last_prompt: None,
            last_used_at: info.last_used_at,
            // The retention stamps ride the static summary (Task 10): a
            // persisted row reports its compression state (ADR-007's
            // shape-reconciled marker as the index row carries it); a
            // live session is never compressed (ADR-004).
            compressed: info.compressed,
            compressed_at: info.compressed_at.map(stamp_rfc3339),
        }
    }
}

/// Build the wire view of a live session scope: the static metadata plus the
/// runtime fields — busy from the in-flight turn registry, model and
/// messageCount from the first agent, usage summed across every agent
/// (SDR-002), and lastSeq from the session's durable relay watermark.
async fn build_session_view(server: &EmbeddedServer, session: &SessionScope) -> SessionView {
    let metadata = session.metadata().await;
    let session_id = session.id();
    let extras = session.runtime_extras().await;
    let session_prefix = format!("{}:", session_id.as_str());
    let in_flight_busy = {
        let turns = server
            .in_flight_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        turns.keys().any(|key| key.starts_with(&session_prefix))
    };
    let activity = match server.resolve_session_artifacts(session_id.as_str()) {
        Ok(artifacts) => artifacts.activity_shadow.snapshot_value(),
        Err(_) => SessionActivityState::default(),
    };
    // `busy` is the aggregate "any work is happening" relay: the in-flight
    // turn registry (agent turns) OR the activity shadow (background tasks
    // light busy through the fold). `main_turn_active` is the shadow's
    // authoritative main-agent LED alone — the in-flight registry keys don't
    // distinguish main from subagent turns, but fire-mode main turns publish
    // `TurnStarted{agent_id:"main"}` which the shadow folds.
    let busy = activity.busy || in_flight_busy;
    let last_seq = server
        .relay_seq_watermarks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id.as_str())
        .copied()
        .unwrap_or(0);
    SessionView {
        id: session_id.as_str().to_string(),
        title: metadata.title,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
        agent_count: session.list_agents().await.len(),
        tags: metadata.tags.into_iter().collect(),
        status: Some("running".to_string()),
        busy,
        main_turn_active: activity.main_turn_active,
        pending_interaction: pending_kind(&activity.pending_interaction),
        last_turn_reason: activity.last_turn_reason.as_ref().map(outcome_str),
        model: extras.model,
        usage: SessionUsageView {
            first_call: extras.first_call.as_ref().map(SessionFirstCallView::from),
            ..SessionUsageView::from(extras.usage)
        },
        message_count: extras.message_count,
        last_seq,
        archived: metadata.archived,
        // The session's OWN workspace root (W4): `work_dir` is backfilled at
        // create from the owning (routed) manager's cwd, so a session of a
        // non-default workspace reports its own root — never the process
        // default root. The fallback keeps the historical value for sessions
        // whose state predates the field.
        cwd: Some(metadata.work_dir.clone().unwrap_or_else(|| {
            server
                .manager
                .workspace()
                .cwd()
                .to_string_lossy()
                .into_owned()
        })),
        // The live scope carries no last_prompt — the listing overlays the
        // index row's value onto the view (W5).
        last_prompt: None,
        // The user-interaction recency key (SDR-014): the live scope's
        // metadata is the authoritative source.
        last_used_at: metadata.last_used_at,
        // A live session is never compressed (ADR-004: the write path
        // forces the plain shape) — the retention fields stay at rest.
        compressed: false,
        compressed_at: None,
    }
}

/// RFC 3339 with second precision (the retention stamps' wire form —
/// the same spelling `state.json`'s `compressedAt` carries).
fn stamp_rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Render a [`SessionPendingInteraction`] as the wire's string token
/// ("none"/"approval"/"question" — the ternary payload vocabulary).
fn pending_kind(kind: &SessionPendingInteraction) -> String {
    match kind {
        SessionPendingInteraction::None => "none".to_string(),
        SessionPendingInteraction::Approval => "approval".to_string(),
        SessionPendingInteraction::Question => "question".to_string(),
    }
}

/// Render a [`SessionTurnOutcome`] as the wire's string token
/// ("completed"/"cancelled"/"failed").
fn outcome_str(outcome: &SessionTurnOutcome) -> String {
    match outcome {
        SessionTurnOutcome::Completed => "completed".to_string(),
        SessionTurnOutcome::Cancelled => "cancelled".to_string(),
        SessionTurnOutcome::Stalled => "stalled".to_string(),
        SessionTurnOutcome::Failed => "failed".to_string(),
    }
}

/// Common params for session-addressed handlers. Accepts both wire
/// conventions: `session_id` (the embedded engine's dotted-protocol
/// spelling) and `sessionId` (the TS client's camelCase spelling).
#[derive(Debug, Deserialize)]
struct SessionIdParams {
    #[serde(alias = "sessionId")]
    session_id: String,
}

/// Common params for agent-addressed handlers: session id plus an optional
/// `agentId` defaulting to the session's first agent.
#[derive(Debug, Deserialize)]
struct AgentRefParams {
    #[serde(alias = "sessionId")]
    session_id: String,
    #[serde(default, alias = "agentId")]
    agent_id: Option<String>,
}

/// Serialize a [`TurnEndResult`] to the wire shape of `message.send`
/// responses: the engine's per-turn summary lives under the `turn` key.
fn turn_end_to_json(result: TurnEndResult) -> Value {
    let stop_reason = match result.stop_reason {
        TurnStopReason::EndTurn => "end_turn",
        TurnStopReason::MaxTokens => "max_tokens",
        TurnStopReason::Filtered => "filtered",
        TurnStopReason::Paused => "paused",
        TurnStopReason::Unknown => "unknown",
        TurnStopReason::Aborted => "aborted",
        // ADR-004: the watchdog's no-progress termination — its own word.
        TurnStopReason::Stalled => "stalled",
        TurnStopReason::BlockedByHook => "blocked_by_hook",
    };
    json!({
        "turnId": result.turn_id,
        "stopReason": stop_reason,
        "steps": result.steps,
        "usage": {
            "inputTokens": result.usage.input_tokens,
            "outputTokens": result.usage.output_tokens,
            "cacheReadTokens": result.usage.cache_read_tokens,
            "cacheCreationTokens": result.usage.cache_creation_tokens,
        },
        "durationMs": result.duration_ms,
        "blockedByPromptHook": result.blocked_by_prompt_hook,
    })
}

/// Adapter that wraps a [`ReverseRpcHandler`] into a [`ReverseRpcCallback`]
/// for the engine core.
///
/// The transport-layer handler is a bare `Arc<dyn Fn(String, Value) -> Future>`
/// — this struct implements the `ReverseRpcCallback` trait so the engine core
/// can consume it without depending on `mirri-transport`.
struct ReverseRpcCallbackImpl {
    handler: ReverseRpcHandler,
}

#[async_trait::async_trait]
impl ReverseRpcCallback for ReverseRpcCallbackImpl {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        (self.handler)(method.to_string(), params).await
    }
}

// ===========================================================================
// EmbeddedServer
// ===========================================================================

/// A tracked in-flight turn, keyed by `"{session_id}:{agent_id}:{turn_id}"`
/// for cancellation via `message.abort`.
///
/// - **Fire mode** carries the spawned task's handle: a hard `abort()`
///   cuts the task short, and the aborting side publishes the `TurnFailed`
///   that closes the turn bracket (the dead task cannot).
/// - **Wait mode** runs inline on the `message.send` future and settles
///   itself — only its cooperative cancel token is registered. The token
///   ends the turn at the next checkpoint (loop top, a permission gate's
///   select, a cancel-aware provider stream), and the wait arm always
///   closes its own bracket with `TurnCompleted`; aborting the RPC future
///   instead would strand the turn's terminal wire records (`turn.ended`
///   would never be written).
///
/// `pub(super)` because [`InFlightTurnsMap`] — the registry shape the
/// dispatch handlers and the task/cron drivers share — exposes it.
pub(super) enum InFlightTurn {
    Fire {
        handle: tokio::task::JoinHandle<()>,
        cancel: CancelToken,
    },
    Wait {
        cancel: CancelToken,
    },
}

impl InFlightTurn {
    /// The cooperative cancel token — the one cancellation mechanism both
    /// modes share.
    fn cancel_token(&self) -> &CancelToken {
        match self {
            InFlightTurn::Fire { cancel, .. } => cancel,
            InFlightTurn::Wait { cancel } => cancel,
        }
    }
}

/// The engine-wide in-flight turn registry's map type — shared by the
/// dispatch handlers and the task/cron drivers so every engine turn
/// (user-dispatched or driver-injected) is cancellable via
/// `message.abort`.
pub(super) type InFlightTurnsMap = Arc<std::sync::Mutex<HashMap<String, InFlightTurn>>>;

/// Removes a wait-registered turn's `in_flight_turns` entry when the
/// turn's inline future settles (success, error, or panic) — the entry
/// must not outlive the turn, or a later `message.abort` would cancel a
/// dead token.
pub(super) struct WaitTurnGuard {
    pub(super) turns: InFlightTurnsMap,
    pub(super) key: String,
}

impl Drop for WaitTurnGuard {
    fn drop(&mut self) {
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

/// Wire the structured agent-status publisher (CR4) onto an agent's turn
/// flow — the free-function core of [`EmbeddedServer::attach_status_publisher`],
/// shared with the task/cron drivers (which hold the session artifacts
/// directly, not `&EmbeddedServer`).
pub(super) fn wire_agent_status_publisher(
    event_bus: &Arc<mirri_core::session::EventBus>,
    agent: &AgentScope,
    flow_handle: &Arc<std::sync::Mutex<TurnFlow>>,
) {
    let mut flow = flow_handle.lock().unwrap_or_else(|e| e.into_inner());
    let compaction_handle = flow.compaction_handle();
    flow.set_status_publisher(Some(Arc::new(AgentStatusPublisher::new(
        Arc::clone(event_bus),
        agent.id().as_str().to_string(),
        agent.context_memory(),
        compaction_handle,
        agent.usage_service(),
        agent.config(),
        agent.plan(),
        agent.swarm_mode(),
    ))));
}

/// Coerce a turn's end result to `Aborted` when its cancel token fired —
/// a cancelled turn reports `Aborted` even when the provider stream ended
/// cleanly after observing the token (the awaiting client cancelled and
/// must hear the turn ended because of that). Shared by the message.send
/// wait arm and the task/cron drivers' inline injected turns.
///
/// ADR-004 batch d: a watchdog-sourced cancellation reports `Stalled`
/// regardless of what the engine surfaced (a racing clean settle included)
/// — the vocabulary is the attribution; a system no-progress termination
/// must never render as a user cancel.
pub(super) fn coerce_aborted_if_cancelled(
    turn_end: TurnEndResult,
    cancel: &CancelToken,
) -> TurnEndResult {
    if cancel.is_cancelled() && cancel.is_stalled() {
        TurnEndResult {
            stop_reason: TurnStopReason::Stalled,
            ..turn_end
        }
    } else if cancel.is_cancelled() && turn_end.stop_reason != TurnStopReason::Aborted {
        TurnEndResult {
            stop_reason: TurnStopReason::Aborted,
            ..turn_end
        }
    } else {
        turn_end
    }
}

/// The listing/search default workspace scope (multi-workspace design §2.2 —
/// W5). See [`EmbeddedServer::listing_visibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ListingVisibility {
    /// Every registered workspace — serve's view (and the effective view of
    /// every single-workspace registry, where "all" is exactly one).
    #[default]
    AllRegistered,
    /// Only the default (pinned) workspace — the embedded-host constructor
    /// [`EmbeddedServer::for_workspace_root`] sets this (NAPI/ACP hosts).
    PinnedToDefault,
}

/// A turn flow's shared usage-model-key handle — the `by_model`
/// attribution key (the agent's configured model alias, spec 11 A13) the
/// flow's step-usage recording closure reads at each step. Shared through
/// the embedded server's side map so a mid-turn model switch can update it
/// without taking the turn-long `TurnFlow` mutex.
pub(crate) type UsageModelKeyHandle = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// An in-process engine exposing the `session.*`/`message.send` RPC surface
/// over the [`EngineAdapter`] interface.
///
/// Cloning is cheap — but unlike `ServerState` the server is *shared*
/// through [`Arc`] by its consumers (AcpHandler, EngineBridge), since the
/// event-subscriber list is updated in place.
pub struct EmbeddedServer {
    /// The session manager every RPC method operates on.
    manager: Arc<SessionManager>,
    /// The workspace registry this server dispatches through: the default
    /// workspace's manager is `manager`, non-default workspaces are
    /// registered-but-lazy (D5), and the routing seam
    /// ([`WorkspaceRegistry::manager_for`](crate::workspace_registry::WorkspaceRegistry::manager_for))
    /// is where W3's per-session resolution plugs in. Every constructor
    /// installs one: the direct constructors wrap the bare manager through
    /// [`WorkspaceRegistry::single`](crate::workspace_registry::WorkspaceRegistry::single),
    /// [`EmbeddedServer::from_parts`] takes the assembled registry.
    registry: Arc<WorkspaceRegistry>,
    /// The listing/search default workspace scope (multi-workspace design
    /// §2.2): `PinnedToDefault` restricts `session.list` / `session.search`
    /// to the default (pinned) workspace — the embedded-host constructor
    /// path (NAPI/ACP roots share a home whose `workspaces.json` may register
    /// more workspaces than the host may see). `AllRegistered` (the default,
    /// and serve's) lists every registered workspace. An explicit
    /// `workspace_id` filter overrides the pin (still alias-folded).
    listing_visibility: ListingVisibility,
    /// Optional [`LlmFactory`] that `message.send` uses to run engine turns;
    /// `None` (the default) disables the engine loop.
    llm_factory: Option<Arc<dyn LlmFactory>>,
    /// Per-agent [`TurnFlow`] instances, reused across turns so steering,
    /// compaction, and dedup state survive between messages. The inner mutex
    /// serializes turns of the same agent.
    turn_flows: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<TurnFlow>>>>>,
    /// Per-agent activity trackers (the `agent.activity` fold), keyed like
    /// `turn_flows` (`"{session_id}:{agent_id}"`). The tracker's Arc is
    /// captured at turn-flow creation so an `agent.activity` read never has
    /// to lock a `TurnFlow` that a running turn holds.
    activity_trackers: Arc<std::sync::Mutex<HashMap<String, Arc<ActivityTracker>>>>,
    /// Per-agent shared steer buffers, keyed like `turn_flows`
    /// (`"{session_id}:{agent_id}"`) and created alongside the flow (same
    /// handle the turn's hooks flush). `message.steer` and the shell
    /// backgrounded notice park content here instead of locking the
    /// [`TurnFlow`] — that mutex is held for the whole turn by the run
    /// loop, so a mid-turn steer through the flow would block until the
    /// turn ends (one turn late) or deadlock a single-threaded runtime.
    steer_buffers: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<SteerBuffer>>>>>,
    /// Per-agent compaction orchestrator handles, keyed like `turn_flows`
    /// (`"{session_id}:{agent_id}"`) and created alongside the flow (the
    /// same `Arc<Mutex<FullCompaction>>` the flow's compaction engine and
    /// the activity fold share). `setModel` / `updateMeta` push the new
    /// model's context window through this handle instead of locking the
    /// [`TurnFlow`] — that mutex is held for the whole turn by the run
    /// loop, so a mid-turn window update through the flow would block
    /// until the turn ends.
    compaction_handles:
        Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<FullCompaction>>>>>,
    /// Per-agent usage-model-key handles, keyed like `turn_flows`
    /// (`"{session_id}:{agent_id}"`) and created alongside the flow (the
    /// same shared `Arc<Mutex<Option<String>>>` the flow's step-recording
    /// closure reads). `setModel` / `updateMeta` push the switched model's
    /// alias through this handle instead of locking the [`TurnFlow`] —
    /// that mutex is held for the whole turn by the run loop, so a
    /// mid-turn usage re-attribution through the flow would block until
    /// the turn ends (spec 11 A13).
    usage_model_key_handles: Arc<std::sync::Mutex<HashMap<String, UsageModelKeyHandle>>>,
    /// In-flight turn tasks keyed by `"{session_id}:{agent_id}:{turn_id}"`,
    /// for cancellation via `message.abort`. A task removes its own entry
    /// when it finishes; an entry whose task finished in the race window is
    /// swept by the insert path, so the map never accumulates completed
    /// turns. Wait-mode turns (the `message.send` wait arm and the
    /// task/cron drivers' injected turns) register the same shape — only
    /// their cooperative cancel token is tracked, and a
    /// [`WaitTurnGuard`] removes the entry when the turn settles.
    ///
    /// KEY FORMAT CONTRACT: every key is exactly
    /// `"{session_id}:{agent_id}:{turn_id}"` with a session id that is a
    /// UUID and therefore contains no colon — so the FIRST colon always
    /// terminates the session-id segment and prefix matching on
    /// `"{session_id}:"` (the follow driver's in-flight probe) can never
    /// collide with another session's keys. See
    /// `follow_scheduler.rs` `spawn_follow_driver` for the consuming side.
    in_flight_turns: InFlightTurnsMap,
    /// Live event subscribers, registered by [`EngineAdapter::subscribe_events`].
    /// Every session relay fans out to all of them.
    subscribers: Arc<std::sync::Mutex<Vec<mpsc::Sender<JsonRpcNotification>>>>,
    /// The server-scoped lifecycle event broadcast — workspace
    /// create/touch/rename/delete ([`ServerEvent`]). Every `subscribe_events`
    /// call installs a forwarder from this broadcast to the caller's channel,
    /// so the workspace lifecycle events ride the single `EngineAdapter`
    /// event stream (contract 3 — the vocabulary's consumer sync), never the
    /// per-session relay/seq machinery.
    server_events: tokio::sync::broadcast::Sender<ServerEvent>,
    /// The instance epoch stamped on every durable event notification the
    /// relays forward — one UUID minted per [`EmbeddedServer`]
    /// construction, mirroring the server's instance-id semantics: a change
    /// of epoch means the per-session seq counters restarted from scratch.
    epoch: String,
    /// Per-session event relays keyed by session id; a relay forwards its
    /// session's bus events to the subscribers until the session closes.
    relays: Arc<std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Per-session follow-driver tasks keyed by session id (the same shape
    /// as [`Self::relays`]). A driver ticks the incremental foreign-journal
    /// absorb until the session closes or the journal is externally
    /// truncated. A re-resume drains the previous driver before spawning
    /// the next one (the relay drain's pattern), so a close→resume race
    /// never stacks two drivers on one session.
    follow_drivers: Arc<std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// Last durable seq each session's relay stamped — the watermark a
    /// resumed session's new relay continues from, so a same-process
    /// close→resume keeps the durable stream monotonic instead of silently
    /// restarting at 1 (a cross-process resume cannot carry the watermark;
    /// there the new epoch is the documented restart signal). Entries are
    /// never removed: session ids are unique and the map is bounded by the
    /// process's session count.
    relay_seq_watermarks: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    /// Per-session goal-chain bookkeeping (the goal continuation driver's
    /// state: chain_running / idle_streak / continuations), keyed by session
    /// id. The turn settlement points and the goal chain task both reach it
    /// through this cloned Arc — neither holds `&EmbeddedServer`. Entries
    /// are never removed on close: session ids are unique and the map is
    /// bounded by the process's session count (the same stance as
    /// [`Self::relay_seq_watermarks`]).
    goal_chain_states: goal_driver::GoalChainStateMap,
    /// Reverse-RPC handler registered through
    /// [`EngineAdapter::on_reverse_rpc`]. Wrapped in a
    /// [`ReverseRpcCallback`] adapter and passed to agents and turn flows so
    /// the engine can issue `approval.ask` and `question.ask` requests to the
    /// client.
    reverse_rpc: Arc<std::sync::Mutex<Option<ReverseRpcHandler>>>,
    /// The [`ReverseRpcCallback`] adapter wrapping [`Self::reverse_rpc`]. This
    /// is what gets passed to [`AgentScopeConfig`] and [`TurnFlow`] — the
    /// engine core consumes the trait, not the raw handler.
    reverse_rpc_callback: Arc<std::sync::Mutex<Option<Arc<dyn ReverseRpcCallback>>>>,
    /// Optional [`ProfileManager`] for resolving subagent profiles. When
    /// present, a [`SessionSubagentHost`] is wired into the main agent's
    /// config, enabling the `Agent` and `AgentSwarm` tools.
    profile_manager: Option<Arc<ProfileManager>>,
    /// Optional [`WebSearchProvider`] for the `WebSearch` tool.
    web_search_provider: Option<Arc<dyn mirri_core::tools::WebSearchProvider>>,
    /// Optional [`FetchUrlProvider`] for the `FetchURL` tool.
    fetch_url_provider: Option<Arc<dyn mirri_core::tools::FetchUrlProvider>>,
    /// Optional [`McpConnectionManager`] for MCP tool registration. When
    /// present, MCP tools from connected servers are registered into each
    /// agent's tool manager at session creation and resume time, and the
    /// `mcp.*` RPC methods return real data instead of stubs. Under the
    /// three-pool model this is the **workspace pool**: the project layers'
    /// and plugin-contributed servers' connections.
    mcp_manager: Option<Arc<mirri_mcp::McpConnectionManager>>,
    /// The app-level **user pool**: the user config layer's connections,
    /// an app-level asset shared across workspace engines (spec §4.2).
    /// Injected at the composition root (`with_user_mcp_pool` /
    /// `shared_user_mcp_pool_for_home`); assembled defaults are private
    /// pools, so tests and single-workspace processes stay isolated.
    user_mcp_pool: Option<Arc<crate::user_mcp_pool::UserMcpPool>>,
    /// Per-session MCP **overlay** managers — the fourth source (the D6
    /// ruling, spec §4.2): session-scoped servers passed with ACP
    /// `session/new`'s / `session/load`'s `mcpServers`, born and dying with
    /// the session, never persisted (the client re-sends them on every
    /// load), shadowing same-name entries of both pools. Each entry owns a
    /// private [`McpConnectionManager`], disconnected when the session
    /// closes or archives.
    session_overlays: Arc<std::sync::Mutex<HashMap<String, SessionOverlay>>>,
    /// MCP server configs discovered at startup, awaiting background
    /// connection, with each layer's provenance kept (the pool-routing
    /// view — the user layer feeds [`Self::user_mcp_pool`], the project
    /// layers the workspace manager). The actual `connect_all` is deferred
    /// until the first `call_method` invocation, which is guaranteed to run
    /// inside a Tokio runtime (unlike `for_workspace_root()`, which may
    /// execute before the runtime is created in `EngineBridge::new`).
    pending_mcp_configs: std::sync::Mutex<Option<mirri_mcp::config::LayeredMcpConfig>>,
    /// Guards `pending_mcp_configs` so `connect_all` is spawned exactly once.
    mcp_connect_spawned: std::sync::atomic::AtomicBool,
    /// Guards [`Self::ensure_mcp_pool_listeners`] so each pool's status
    /// listener is attached exactly once per engine — both the startup
    /// lazy-connect path and the config-write reconcile path call it, and
    /// either may arrive first.
    mcp_pool_listeners_attached: std::sync::atomic::AtomicBool,
    /// The workspace-layer names that shadow same-name user-pool entries
    /// (spec §4.2 view semantics) — **shared with the user pool's status
    /// listener and refreshed on every reconcile** (T1 review F-T1-3: the
    /// startup snapshot alone went stale when a config write hot-added a
    /// same-name workspace server, letting the user pool's events re-take
    /// the name).
    mcp_user_pool_shadow: std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Serializes the background pool reconcile tasks (spec §4.3): rapid
    /// config writes must apply their pool operations in write order, not
    /// interleaved. Shared behind an `Arc` so the detached tasks can hold
    /// it.
    mcp_reconcile_lock: Arc<tokio::sync::Mutex<()>>,
    /// The app-level MCP pool status broadcast (M7, F-9, ADR-010): every
    /// `event.mcp.server.status` payload `publish_pool_status_event` emits
    /// to the live sessions' buses is also sent here, so consumers with no
    /// session at all (the serve wire's `mcp.subscribe`) observe the pool
    /// surface. Capacity 256 — the payload is a cheap `Arc<Value>` clone,
    /// and a lagged receiver self-disposes by dropping events (the
    /// management plane tolerates gaps; the durable record is the config
    /// layer itself). A `send` failure means "no subscribers" and is
    /// ignored.
    mcp_pool_events: tokio::sync::broadcast::Sender<std::sync::Arc<Value>>,
    /// The serialized skill-catalog rescan orchestrator (ADR-010 decisions
    /// 3-4): every rescan trigger path — the config-write resync
    /// (`resync_skill_catalogs`), the `plugin.reload` skill replay
    /// (`refresh_plugin_consumers`), the file watcher (plan Task 5), and
    /// the manual `skill.reload` RPC (plan Task 6) — funnels through this
    /// single entry point, so snapshot → scratch replay → atomic swap →
    /// diff → session-mark + broadcast runs under one lock with one
    /// notification shape.
    skill_reload: Arc<crate::skill_reload::SkillReloadOrchestrator>,
    /// The per-workspace skill-directory watcher handles (plan Task 5,
    /// ADR-010 decision 1): one [`SkillDirectoryWatcher`] per BUILT
    /// runtime, mounted on the first dispatcher call (the same lazy-attach
    /// pattern as `maybe_spawn_workspace_pool_connects` — construction may
    /// precede the Tokio runtime, the first RPC call never does) and
    /// dropped when the registry removes the workspace (the handle's Drop
    /// signals the watcher task to exit — the anti-leak shutdown channel).
    /// The map holds STRONG handles: the watcher task itself holds the
    /// orchestrator and the runtime strongly, so nothing points back at
    /// this map — no cycle, and the registry's remove (which drops the
    /// runtime) actually reclaims the watcher.
    skill_watchers_shared:
        Arc<
            std::sync::Mutex<
                HashMap<String, Option<crate::skill_watcher::SkillDirectoryWatcher>>,
            >,
        >,
    /// Outstanding fire-and-forget watcher-mount tasks (the lazy call_method
    /// pass spawns them); `skill.reload` drains this list so its `watching`
    /// answers are settled before it responds.
    pending_mounts: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The measured MCP startup window (see [`McpStartupTiming`]) behind
    /// the `mcp.startupMetrics` RPC — this engine's own startup connect
    /// set, from the first spawned connect task until the last settles.
    mcp_startup: Arc<std::sync::Mutex<McpStartupTiming>>,
    /// Optional [`PluginManager`] for plugin lifecycle management. When
    /// present, `plugin.*` RPC methods return real data instead of stubs,
    /// and plugin-provided MCP servers, hooks, skills, and commands are
    /// wired into the engine.
    plugin_manager: Option<Arc<mirri_plugin_host::PluginManager>>,
    /// The current server configuration snapshot, loaded from config.toml
    /// (migrated, env-expanded — see the `bootstrap` module's
    /// `load_config_from_home`). Read by `config.get`; a future
    /// `config.set` layer will mutate it in place.
    config: Arc<RwLock<ServerConfig>>,
    /// Path of the config.toml the config was loaded from / the write
    /// surface persists. `Some` whenever the Mirri home resolved — the
    /// well-known `<home>/config.toml` even when the file does not exist
    /// yet (the first write creates it); `None` only when the home could
    /// not be resolved or the existing file cannot be parsed.
    config_path: Option<PathBuf>,
    /// The explicitly injected Mirri home directory, when the server was
    /// built through [`EmbeddedServer::for_workspace_root`] with one —
    /// the directory holding `config.toml`, `profiles/`, `plugins/`, MCP
    /// config, and the session persistence root. `None` (the default) means
    /// every home consumer resolves through the env-based
    /// [`mirri_core::config::mirri_home_dir`] (`MIRRICODE_HOME`, then
    /// `$HOME/.mirri-code`) exactly as before the injection existed.
    home_dir: Option<PathBuf>,
    /// The SDK's explicit skill directories (v1/v2 `skillDirs`) the assembly
    /// received (B8) — replayed by [`Self::resync_skill_catalogs`] so a
    /// config write that touches the skill-derived keys re-derives every
    /// built workspace's catalog from the same inputs the assembly used.
    skill_dirs: Vec<PathBuf>,
    /// The MCP user-layer base the bootstrap's pool load resolved — the
    /// directory whose `/.mirri-code/mcp.json` the RPC-time MCP consumers
    /// (`mcp.config.*` reads/writes, the reconcile kernel, the tool-filter
    /// merge) address, so they always see the very file the pools loaded.
    /// `None` when the server was not built through
    /// [`crate::bootstrap::assemble`] — consumers then re-resolve through
    /// [`crate::bootstrap::resolve_mcp_home_base`].
    mcp_home_base: Option<PathBuf>,
    /// The home-level workspace trust store (W7 D6 / W8): the same handle
    /// the registry's runtime builder consulted at build time. The W8
    /// `workspace.setTrust` face writes it and the `workspace.list` rows
    /// read it (the live store answer, never the frozen runtime verdict).
    /// `None` on the low-level constructors (no home — their registries
    /// hold only the trusted-by-construction default, so nothing consults
    /// it) and on no-home assemblies (trust then cannot persist and
    /// `workspace.setTrust` answers invalid_params).
    trust_store: Option<crate::workspace_trust::WorkspaceTrustStore>,
    /// The explicitly injected files-store root (see
    /// [`Self::with_files_dir`]); `None` resolves through the home-dir chain
    /// at call time.
    files_dir: Option<PathBuf>,
    /// The experimental-flag registry the `flag.*` RPC methods read. Shared
    /// behind an `Arc` so hosts (and tests) can register definitions at any
    /// time; the embedded composition root registers the engine's own
    /// `hook-command-rewrite` gate (TS parity — see
    /// [`mirri_core::HOOK_COMMAND_REWRITE_FLAG`]) so `flag.snapshot` /
    /// `explainAll` answer the real registered set, and hosts may add more.
    flag_registry: Arc<mirri_core::FlagRegistry>,
    /// Registry of live foreground `!` shell commands — the entries
    /// `shell.cancel` (by command id) and `task.detach` (by task id)
    /// address. Shared behind an `Arc` so spawned run futures and the
    /// dispatchers observe the same set.
    shell_commands: Arc<mirri_core::shell_command::ShellCommandRegistry>,
    /// How long a `shell.run` command may run in the foreground before it
    /// is detached to a background task
    /// ([`mirri_core::shell_command::SHELL_FOREGROUND_TIMEOUT_SECS`] by
    /// default; shrunk by tests through
    /// [`Self::with_shell_foreground_timeout`]).
    shell_foreground_timeout: std::time::Duration,
    /// The models.dev upstream the `providerDiscovery` refresh pulls,
    /// `api.json` by default. `None` disables the remote fetch entirely
    /// (the hermetic test seam — the answer is the honest empty success)
    /// so no test or embedded host hits the real network by accident.
    models_dev_url: Option<String>,
    /// The last fetched (or cache-loaded) models.dev directory that
    /// feeds the `model.list` display-name fallback, the browse/import
    /// surfaces, and the refresh machinery. `None` until the
    /// first refresh settles, a lazy load restores a cache file, or the
    /// background preheat lands first — first-arrival-wins on the mutex
    /// (P5 C1). Behind an `Arc` so the detached preheat task can hold it.
    models_dev_catalog: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<Value>>>>,
    /// Guards the models.dev catalog preheat spawn (see
    /// [`Self::maybe_spawn_models_catalog_preheat`]) so it fires exactly
    /// once per engine — the [`Self::maybe_spawn_mcp_connect`] pattern.
    models_preheat_spawned: std::sync::atomic::AtomicBool,
    /// Guards the one-time legacy flat-layout session migration (see
    /// [`Self::migrate_legacy_sessions_once`]) so it runs exactly once per
    /// engine, before the first dispatched RPC observes the sessions root.
    legacy_sessions_migrated: std::sync::atomic::AtomicBool,
    /// The SQLite session/workspace index at `<home>/session-index.db`
    /// (Phase 3, see [`Self::session_index`]) — opened lazily on first use
    /// so the home attached by [`Self::from_parts`] /
    /// [`Self::with_home_dir`] after construction is already in effect.
    /// `None` inside the cell means no home is injected (or the open
    /// failed) and every consumer degrades to the plain disk scan.
    session_index: std::sync::OnceLock<Option<Arc<mirri_session_index::SessionIndex>>>,
    /// Guards the one-time index converge (see
    /// [`Self::reconcile_session_index_once`]) so it runs exactly once per
    /// engine, after the legacy migration settles.
    session_index_reconciled: std::sync::atomic::AtomicBool,
    /// The models.dev refresh degradation warning surfaced by
    /// `config.diagnostics`: set when the last refresh failed and the
    /// cached directory is being served instead, cleared by the next
    /// successful refresh.
    models_dev_warning: std::sync::Mutex<Option<String>>,
    /// The cron driver's tick interval (tests shrink it to sub-second
    /// values; the production default is [`CRON_DRIVER_TICK_SECS`]).
    cron_tick: std::time::Duration,
    /// The session-consistency follow driver's tick interval (batch 1:
    /// tests shrink it to sub-second values; the production default is
    /// [`FOREGROUND_FOLLOW_TICK_SECS`], itself bounded by the 5-second
    /// follow latency ceiling of US C1.2).
    follow_tick: std::time::Duration,
    /// The index-refresh driver's tick interval (batch 1: tests shrink it
    /// to sub-second values; the production default is
    /// [`INDEX_REFRESH_TICK_SECS`], the C1.1 list-freshness ceiling).
    index_refresh_tick: std::time::Duration,
    /// Guards the one-time index-refresh driver spawn (see
    /// [`Self::maybe_spawn_index_refresh_driver`]) so exactly one
    /// per-server task runs — the
    /// [`Self::maybe_spawn_models_catalog_preheat`] pattern.
    index_refresh_spawned: std::sync::atomic::AtomicBool,
    /// Guards the one-time startup orphan census + lifetime-sweeper spawn
    /// (see [`Self::run_startup_orphan_governance`]) so exactly one census
    /// and one sweeper run per server, and every concurrent first
    /// dispatch awaits the same settlement (the OnceCell wait semantics —
    /// the Task 3 ordering ledger holds under concurrent first calls).
    orphan_governance_settled: tokio::sync::OnceCell<()>,
    /// The task-lifecycle sweeper's tick interval (B1: tests shrink it to
    /// sub-second values; the production default is
    /// [`TASK_SWEEP_TICK_SECS`]).
    task_sweep_tick: std::time::Duration,
    /// The engine-turn inactivity deadline (`message.send`'s stall
    /// watchdog, 15E): tests shrink it to sub-second values; the
    /// production default is
    /// [`ENGINE_TURN_STALL_TIMEOUT`](message_handlers::ENGINE_TURN_STALL_TIMEOUT).
    turn_stall_timeout: std::time::Duration,
    /// The ADR-006 long backstop window — the third-tier deadline the
    /// stall watchdog applies to Degraded waits (Unknown, probe-degraded,
    /// all-settled-but-not-returning). Default
    /// [`ENGINE_TURN_BACKSTOP_TIMEOUT`](message_handlers::ENGINE_TURN_BACKSTOP_TIMEOUT)
    /// (30 minutes); a separate `with_turn_backstop_timeout` test seam
    /// (NOT the `turnStallTimeoutMs` config key — the two windows have
    /// different semantics and must not be scaled together).
    turn_backstop_timeout: std::time::Duration,
    /// Guards the one-time retention task spawn (see
    /// [`Self::maybe_spawn_retention_task`]) so exactly one periodic task
    /// runs per server — the
    /// [`Self::maybe_spawn_index_refresh_driver`] pattern. The guard is
    /// unconditional (independent of the enabled gate): a config that
    /// flips `enabled` between two first-round RPCs must not mint a
    /// second task — every round re-reads the config anyway, so the
    /// single task honors the flip.
    retention_task_spawned: std::sync::atomic::AtomicBool,
    /// The shared blob-ledger handle (see
    /// [`retention_delete`](super::retention_delete)): the process-wide
    /// cache the delete executor's tail reclamation and the periodic
    /// recheck both address (SDR-004's "same instance" ruling). The mutex
    /// rides an `Arc` so the retention task's spawn captures the SAME slot
    /// (a task-held second slot would be a second ledger — the
    /// double-instance gap). Built lazily; `None` until the first
    /// delete/recheck needs it.
    blob_ledger:
        std::sync::Arc<tokio::sync::Mutex<Option<Arc<mirri_core::blob_ledger::BlobLedger>>>>,
    /// The print-steer bookkeeping map (B1 D-59) — see
    /// [`print_mode`](self::print_mode).
    print_states: print_mode::PrintSteerStates,
}

/// How often each session's task sweeper ticks
/// [`TaskManager::terminate_expired`](mirri_core::task::TaskManager::terminate_expired):
/// one second — fine-grained enough that a runaway subagent dies promptly
/// after its deadline, coarse enough to be noise on an idle session.
const TASK_SWEEP_TICK_SECS: u64 = 1;

/// How often each session's cron driver ticks [`CronManager::check_due`]:
/// coarse on purpose — cron expressions have minute granularity, so a
/// half-minute worst-case fire latency is within the scheduling noise.
const CRON_DRIVER_TICK_SECS: u64 = 30;

/// How often each session's follow driver ticks an incremental absorb of
/// foreign journal appends: two seconds, comfortably under the 5-second
/// follow latency ceiling (US C1.2) while keeping the stat-probe traffic
/// per session at a negligible level.
const FOREGROUND_FOLLOW_TICK_SECS: u64 = 2;

/// How often the per-server index refresh driver ticks
/// [`EmbeddedServer::reconcile_session_index`]: ten seconds, the C1.1
/// list-freshness ceiling — a foreign entry's session mutation (rename,
/// archive, delete, create) must surface in this engine's `session.list`
/// within the tick, while a full-bucket mtime scan stays negligible at
/// that cadence. Each pass converges from the engine's own disk view,
/// serialized across processes through the `<home>/session-index.reconcile.lock`
/// file lock (ADR-004's D8 create_new + stale-takeover pattern — batch 2,
/// see `session_index.rs`); a lock wait timeout skips the round and the
/// next tick retries.
const INDEX_REFRESH_TICK_SECS: u64 = 10;

/// Capacity of the app-level MCP pool status broadcast (see
/// [`EmbeddedServer::mcp_pool_events`]) — the management-plane event volume
/// (a handful of transitions per config write), never a streaming surface.
const MCP_POOL_EVENT_CAPACITY: usize = 256;

/// One session's MCP overlay (the D6 ruling): its private connection
/// manager plus the server **names** it was configured with. The names
/// shadow same-name workspace/user entries from the moment the overlay is
/// installed — before any connection settles (the connects are spawned in
/// the background) — so a same-name workspace server can never slip into
/// the session's tool set while the overlay connects.
#[derive(Debug, Clone)]
struct SessionOverlay {
    manager: Arc<mirri_mcp::McpConnectionManager>,
    names: std::collections::HashSet<String>,
}

/// The MCP startup measurement window behind `mcp.startupMetrics` (the
/// TS `McpConnectionManager.initialLoadDurationMs` counterpart): `started`
/// is set when the first background connect task is spawned, `outstanding`
/// counts tasks still in flight, and `finished` is set when the last one
/// settles. An engine with no startup connect set keeps `started` at
/// `None` and reports 0; while tasks are still in flight the elapsed time
/// so far is reported.
#[derive(Debug, Default)]
struct McpStartupTiming {
    started: Option<std::time::Instant>,
    outstanding: usize,
    finished: Option<std::time::Instant>,
}

impl McpStartupTiming {
    /// Record one connect task about to be spawned (sets the window's
    /// start on the first task).
    fn add_task(&mut self) {
        self.started.get_or_insert_with(std::time::Instant::now);
        self.outstanding += 1;
    }

    /// Record one connect task settling; sets the window's end when the
    /// last outstanding task lands.
    fn finish_task(&mut self) {
        self.outstanding = self.outstanding.saturating_sub(1);
        if self.outstanding == 0 {
            self.finished = Some(std::time::Instant::now());
        }
    }

    /// The measured span: start-to-finished once settled, elapsed-so-far
    /// while in flight, 0 when no startup connect set exists.
    fn duration_ms(&self) -> u32 {
        match self.started {
            Some(started) => {
                let end = self.finished.unwrap_or_else(std::time::Instant::now);
                end.saturating_duration_since(started).as_millis() as u32
            }
            None => 0,
        }
    }
}

impl EmbeddedServer {
    /// Create a server bound to the given session manager, without an LLM
    /// provider factory (`message.send` reports that no provider is
    /// configured).
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        Self::with_llm_factory(session_manager, None)
    }

    /// Create a server bound to the given session manager, with an optional
    /// [`LlmFactory`] for the engine loop.
    pub fn with_llm_factory(
        session_manager: Arc<SessionManager>,
        llm_factory: Option<Arc<dyn LlmFactory>>,
    ) -> Self {
        Self::with_profile_manager(session_manager, llm_factory, None)
    }

    /// Create a server with an optional [`LlmFactory`] and
    /// [`ProfileManager`]. When both are present, the `Agent` and
    /// `AgentSwarm` tools are registered for the main agent.
    pub fn with_profile_manager(
        session_manager: Arc<SessionManager>,
        llm_factory: Option<Arc<dyn LlmFactory>>,
        profile_manager: Option<Arc<ProfileManager>>,
    ) -> Self {
        Self::with_providers(
            session_manager,
            llm_factory,
            profile_manager,
            None,
            None,
            Arc::new(RwLock::new(ServerConfig::default())),
            None,
        )
    }

    /// Create a server with all optional providers, a configuration
    /// snapshot, and its source path.
    ///
    /// `web_search_provider` and `fetch_url_provider` inject the `WebSearch`
    /// and `FetchURL` builtin tools. When `None`, the corresponding tool is
    /// not registered. `config` is the current server configuration read by
    /// `config.get`; `config_path` records where it was loaded from
    /// (`None` when it fell back to defaults).
    pub fn with_providers(
        session_manager: Arc<SessionManager>,
        llm_factory: Option<Arc<dyn LlmFactory>>,
        profile_manager: Option<Arc<ProfileManager>>,
        web_search_provider: Option<Arc<dyn mirri_core::tools::WebSearchProvider>>,
        fetch_url_provider: Option<Arc<dyn mirri_core::tools::FetchUrlProvider>>,
        config: Arc<RwLock<ServerConfig>>,
        config_path: Option<PathBuf>,
    ) -> Self {
        // Panic location capture (task 4): thread-local record + chained
        // hook so catch_unwind surfaces can append "(panic at file:line:col)".
        mirri_core::panic_capture::install_panic_location_capture();
        // Every constructor path gets a registry: the bare manager becomes
        // a single-workspace registry (one pre-built default entry keyed by
        // the manager's persistence bucket, no-op resolver) — so
        // `note_active` / `manager_for` never have to special-case an
        // absent registry. `from_parts` replaces it with the assembled
        // registry right after.
        let registry = Arc::new(WorkspaceRegistry::single(Arc::clone(&session_manager)));
        // Built before the struct literal: the `config` field initializer
        // moves the Arc, and the flag registry seeds from the config's
        // `[experimental]` section.
        let flag_registry = new_engine_flag_registry(&config);
        Self {
            manager: session_manager,
            registry,
            // The default scope: every registered workspace. Single-workspace
            // registries (every direct constructor) are unaffected by the
            // choice; multi-workspace serve keeps the unrestricted view.
            listing_visibility: ListingVisibility::default(),
            llm_factory,
            turn_flows: Arc::new(std::sync::Mutex::new(HashMap::new())),
            activity_trackers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            steer_buffers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            compaction_handles: Arc::new(std::sync::Mutex::new(HashMap::new())),
            usage_model_key_handles: Arc::new(std::sync::Mutex::new(HashMap::new())),
            in_flight_turns: Arc::new(std::sync::Mutex::new(HashMap::new())),
            subscribers: Arc::new(std::sync::Mutex::new(Vec::new())),
            server_events: tokio::sync::broadcast::channel(SERVER_EVENT_CAPACITY).0,
            epoch: uuid::Uuid::new_v4().to_string(),
            relays: Arc::new(std::sync::Mutex::new(HashMap::new())),
            follow_drivers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            relay_seq_watermarks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            goal_chain_states: Arc::new(std::sync::Mutex::new(HashMap::new())),
            reverse_rpc: Arc::new(std::sync::Mutex::new(None)),
            reverse_rpc_callback: Arc::new(std::sync::Mutex::new(None)),
            profile_manager,
            web_search_provider,
            fetch_url_provider,
            mcp_manager: None,
            user_mcp_pool: None,
            session_overlays: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_mcp_configs: std::sync::Mutex::new(None),
            mcp_connect_spawned: std::sync::atomic::AtomicBool::new(false),
            mcp_pool_listeners_attached: std::sync::atomic::AtomicBool::new(false),
            mcp_user_pool_shadow: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            mcp_reconcile_lock: Arc::new(tokio::sync::Mutex::new(())),
            mcp_pool_events: tokio::sync::broadcast::channel(MCP_POOL_EVENT_CAPACITY).0,
            skill_reload: Arc::new(crate::skill_reload::SkillReloadOrchestrator::new()),
            skill_watchers_shared: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_mounts: std::sync::Mutex::new(Vec::new()),
            mcp_startup: Arc::new(std::sync::Mutex::new(McpStartupTiming::default())),
            plugin_manager: None,
            config,
            config_path,
            home_dir: None,
            // No explicit skill dirs on the low-level constructors (B8):
            // only `from_parts` carries the assembly's `skillDirs` through.
            skill_dirs: Vec::new(),
            mcp_home_base: None,
            trust_store: None,
            files_dir: None,
            flag_registry,
            shell_commands: Arc::new(mirri_core::shell_command::ShellCommandRegistry::new()),
            shell_foreground_timeout: std::time::Duration::from_secs(
                mirri_core::shell_command::SHELL_FOREGROUND_TIMEOUT_SECS,
            ),
            models_dev_url: Some(MODELS_DEV_URL.to_string()),
            models_dev_catalog: std::sync::Arc::new(std::sync::Mutex::new(None)),
            models_preheat_spawned: std::sync::atomic::AtomicBool::new(false),
            legacy_sessions_migrated: std::sync::atomic::AtomicBool::new(false),
            session_index: std::sync::OnceLock::new(),
            session_index_reconciled: std::sync::atomic::AtomicBool::new(false),
            models_dev_warning: std::sync::Mutex::new(None),
            cron_tick: std::time::Duration::from_secs(CRON_DRIVER_TICK_SECS),
            follow_tick: std::time::Duration::from_secs(FOREGROUND_FOLLOW_TICK_SECS),
            index_refresh_tick: std::time::Duration::from_secs(INDEX_REFRESH_TICK_SECS),
            index_refresh_spawned: std::sync::atomic::AtomicBool::new(false),
            retention_task_spawned: std::sync::atomic::AtomicBool::new(false),
            orphan_governance_settled: tokio::sync::OnceCell::new(),
            task_sweep_tick: std::time::Duration::from_secs(TASK_SWEEP_TICK_SECS),
            turn_stall_timeout: message_handlers::ENGINE_TURN_STALL_TIMEOUT,
            turn_backstop_timeout: message_handlers::ENGINE_TURN_BACKSTOP_TIMEOUT,
            print_states: Arc::new(std::sync::Mutex::new(HashMap::new())),
            blob_ledger: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Shrink the cron driver's tick interval (tests inject sub-second
    /// values so a due cron task fires without a multi-second wait; the
    /// production default lives at [`CRON_DRIVER_TICK_SECS`]). Builder
    /// style: consumes and returns the server.
    pub fn with_cron_tick(mut self, tick: std::time::Duration) -> Self {
        self.cron_tick = tick;
        self
    }

    /// Shrink the session-consistency follow driver's tick interval (tests
    /// inject sub-second values so a foreign append is absorbed without a
    /// multi-second wait; the production default lives at
    /// [`FOREGROUND_FOLLOW_TICK_SECS`]). Builder style: consumes and returns
    /// the server.
    pub fn with_follow_tick(mut self, tick: std::time::Duration) -> Self {
        self.follow_tick = tick;
        self
    }

    /// Shrink the index-refresh driver's tick interval (tests inject
    /// sub-second values so a foreign mutation surfaces in the listing
    /// without a multi-second wait; the production default lives at
    /// [`INDEX_REFRESH_TICK_SECS`]). Builder style: consumes and returns
    /// the server.
    pub fn with_index_refresh_tick(mut self, tick: std::time::Duration) -> Self {
        self.index_refresh_tick = tick;
        self
    }

    /// Shrink the task-lifecycle sweeper's tick interval (tests inject
    /// sub-second values so an expired task terminates without a
    /// multi-second wait; the production default lives at
    /// [`TASK_SWEEP_TICK_SECS`]). Builder style: consumes and returns the
    /// server.
    pub fn with_task_sweep_tick(mut self, tick: std::time::Duration) -> Self {
        self.task_sweep_tick = tick;
        self
    }

    /// Shrink the engine-turn inactivity deadline — the stall watchdog
    /// `message.send` runs each turn under (tests inject sub-second values
    /// to exercise the stalled-turn cascade deterministically; production
    /// callers keep the default at
    /// [`ENGINE_TURN_STALL_TIMEOUT`](message_handlers::ENGINE_TURN_STALL_TIMEOUT)).
    /// Builder style: consumes and returns the server.
    pub fn with_turn_stall_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.turn_stall_timeout = timeout;
        self
    }

    /// Shrink the ADR-006 long backstop window (tests inject sub-second
    /// values so the Degraded-tier cascade runs deterministically; the
    /// production default lives at
    /// [`ENGINE_TURN_BACKSTOP_TIMEOUT`](message_handlers::ENGINE_TURN_BACKSTOP_TIMEOUT)).
    /// A separate seam from `with_turn_stall_timeout` BY DESIGN: the two
    /// windows have different semantics (the classic window is the
    /// per-kind / engine-silence window; the backstop is the framework-
    /// bug escape) and must never be scaled together. Builder style:
    /// consumes and returns the server.
    pub fn with_turn_backstop_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.turn_backstop_timeout = timeout;
        self
    }

    /// The engine-turn inactivity deadline the five turn drivers apply
    /// (the stall watchdog window). Read-only access for contract pins and
    /// diagnostics — the value is set at construction (from
    /// `[loop_control] turnStallTimeoutMs` via [`Self::from_parts`], or the
    /// [`ENGINE_TURN_STALL_TIMEOUT`](message_handlers::ENGINE_TURN_STALL_TIMEOUT)
    /// default when the config omits the key) and never mutated afterwards.
    pub fn turn_stall_timeout(&self) -> std::time::Duration {
        self.turn_stall_timeout
    }

    /// Shrink the `shell.run` foreground window (tests inject sub-second
    /// values to exercise the timeout→detach conversion deterministically;
    /// production callers keep the 2-minute default).
    pub fn with_shell_foreground_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.shell_foreground_timeout = timeout;
        self
    }

    /// Create a fully-wired server from the parts produced by
    /// [`crate::bootstrap::assemble`] — the canonical construction path for
    /// an assembled engine, shared by `for_cwd`, `for_workspace_root` and
    /// (via `ServerState` wiring) `mirri serve`.
    ///
    /// [`Self::with_providers`] remains the low-level provider-injection
    /// constructor for callers that assemble components by hand (tests, the
    /// NAPI mock factories); `from_parts` builds through it and additionally
    /// attaches the MCP manager + pending configs, the plugin manager and the
    /// injected home — the pieces `assemble` discovers that the low-level
    /// constructor does not know about.
    pub fn from_parts(parts: EngineParts) -> Self {
        let mut server = Self::with_providers(
            parts.session_manager,
            Some(parts.llm_factory),
            Some(parts.profile_manager),
            None,
            parts.fetch_url_provider,
            parts.config,
            parts.config_path,
        );
        if let Some(configs) = parts.pending_mcp_configs {
            *server
                .pending_mcp_configs
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(configs);
        }
        if let Some(manager) = parts.mcp_manager {
            server = server.with_mcp_manager(manager);
        }
        if let Some(pool) = parts.user_mcp_pool {
            server = server.with_user_mcp_pool(pool);
        }
        if let Some(plugin_mgr) = parts.plugin_manager {
            server = server.with_plugin_manager(plugin_mgr);
        }
        server.home_dir = parts.home_dir;
        server.skill_dirs = parts.skill_dirs;
        server.mcp_home_base = parts.mcp_home_base;
        server.trust_store = parts.trust_store;
        // The assembled registry replaces the single-workspace wrapper
        // `with_providers` installed: the default runtime's manager is the
        // same `parts.session_manager`, so `self.manager` stays untouched.
        server.registry = parts.workspace_registry;
        // The `[loop_control] turnStallTimeoutMs` wiring (O14): the
        // watchdog value is read once at construction — a config reload
        // takes effect on the next boot, like the rest of the section's
        // construction-time keys. Absent keeps the engine default (600 s);
        // 0 disables the watchdog (`Duration::MAX`, the `[subagent]
        // timeout_ms` "0 = no timeout" convention — an unreachable
        // deadline, not a huge-but-finite one).
        let stall_ms = server
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .loop_control
            .as_ref()
            .and_then(|lc| lc.turn_stall_timeout_ms);
        if let Some(ms) = stall_ms {
            let timeout = if ms == 0 {
                std::time::Duration::MAX
            } else {
                std::time::Duration::from_millis(ms)
            };
            tracing::info!(
                timeout_ms = ms,
                "turn stall watchdog armed from [loop_control] turnStallTimeoutMs"
            );
            server = server.with_turn_stall_timeout(timeout);
        } else {
            tracing::info!(
                timeout_secs = message_handlers::ENGINE_TURN_STALL_TIMEOUT.as_secs(),
                "turn stall watchdog armed at the engine default \
                 ([loop_control] turnStallTimeoutMs unset)"
            );
        }
        // D5 guard: an explicit stall window smaller than the provider-side
        // stream-read failure windows means a silent stream dies at the
        // watchdog (wrapped as a cancellation) instead of the provider's
        // retryable read error — worth a warning, never a rejection. The
        // first-responder for a wedged stream is kosong's data-line idle
        // timeout `SSE_IDLE_TIMEOUT` (300 s, `mirri-kosong/src/
        // streaming.rs`, `pub(crate)` — not importable from here; keep the
        // literal in sync when the kosong constant changes), with the
        // byte-level `HTTP_READ_TIMEOUT` (240 s,
        // `mirri-kosong/src/provider.rs`) beneath it.
        const PROVIDER_SSE_IDLE_TIMEOUT_MS: u64 = 300_000;
        if let Some(ms) = stall_ms.filter(|ms| *ms > 0 && *ms < PROVIDER_SSE_IDLE_TIMEOUT_MS) {
            tracing::warn!(
                stall_ms = ms,
                sse_idle_timeout_ms = PROVIDER_SSE_IDLE_TIMEOUT_MS,
                "[loop_control] turnStallTimeoutMs is smaller than the \
                 provider SSE idle timeout: a silent LLM stream will be \
                 cancelled by the engine watchdog (a cancellation error) \
                 rather than failing with the provider's retryable read \
                 timeout — raise the stall window above the SSE idle \
                 timeout to keep the provider-side error semantics"
            );
        }
        server
    }

    /// Attach an [`McpConnectionManager`] to the server. When present, MCP
    /// tools from connected servers are registered into each agent at session
    /// creation/resume, and `mcp.*` RPC methods return real data.
    pub fn with_mcp_manager(mut self, manager: Arc<mirri_mcp::McpConnectionManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Attach the app-level user MCP pool (see
    /// [`crate::user_mcp_pool`]). The composition root calls this to swap
    /// the assembled engine's private pool for the process-level shared one
    /// (the NAPI path), so the user layer connects once per home however
    /// many engines share it.
    pub fn with_user_mcp_pool(mut self, pool: Arc<crate::user_mcp_pool::UserMcpPool>) -> Self {
        self.user_mcp_pool = Some(pool);
        self
    }

    /// Read-only access to the user MCP pool, when one is attached.
    pub fn user_mcp_pool(&self) -> Option<&Arc<crate::user_mcp_pool::UserMcpPool>> {
        self.user_mcp_pool.as_ref()
    }

    /// Read-only access to the MCP manager, when one is attached.
    pub fn mcp_manager(&self) -> Option<&Arc<mirri_mcp::McpConnectionManager>> {
        self.mcp_manager.as_ref()
    }

    /// Read-only access to the workspace registry (W8): the multi-workspace
    /// owner whose lazily built per-workspace runtimes carry the live trust
    /// verdicts. External hosts and the cross-backend contract pins reach
    /// a workspace's built runtime through this (the embedded tests read
    /// the field directly — same handle).
    pub fn workspace_registry(&self) -> &Arc<crate::workspace_registry::WorkspaceRegistry> {
        &self.registry
    }

    /// Push a new user-level permission rule set into every live session of
    /// every built workspace runtime (D-34) — the fan-out the
    /// `approval.addRule` / `approval.removeRule` write pipeline and the
    /// config.toml hot-reload call after the shared config snapshot swaps.
    /// Registered-but-unbuilt runtimes pick the new rules up when they are
    /// first built (session create/resume reads the shared snapshot). The
    /// push is idempotent: a repeated same-value push is a harmless no-op
    /// in effect.
    pub async fn apply_user_permission_rules(&self, rules: Vec<PermissionRule>) {
        for runtime in self.registry.built_runtimes() {
            runtime
                .manager
                .apply_user_permission_rules(rules.clone())
                .await;
        }
    }

    /// Replay the full skill discovery sequence onto every **built**
    /// workspace runtime (B8), through the serialized reload orchestrator
    /// (ADR-010): called by the config-write pipeline when the
    /// skill-derived keys (`extra_skill_dirs` / `disabled_skills`) change,
    /// so the new config takes effect without a restart. Registered-but-
    /// unbuilt runtimes pick the new config up when they are first built.
    /// Each workspace's rescan diffs and notifies through the same
    /// notification loop every other trigger uses (config trigger).
    pub(crate) fn resync_skill_catalogs(&self) {
        let config = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for runtime in self.registry.built_runtimes() {
            let ctx = crate::skill_reload::SkillReloadContext {
                config: &config,
                home_dir: self.home_dir.as_deref(),
                plugin_manager: self.plugin_manager.as_deref(),
                skill_dirs: &self.skill_dirs,
            };
            self.skill_reload
                .reload_workspace(&runtime, &ctx, crate::skill_reload::SkillReloadTrigger::Config);
        }
    }

    /// The config-write tail hook (B8): replay skill discovery onto every
    /// built workspace runtime when the skill-derived keys changed between
    /// `before` (the pre-write config snapshot) and the current in-memory
    /// config. Shared by every config write path (`config.set`'s inline
    /// pipeline and `mutate_config_raw`).
    pub(crate) fn resync_skill_catalogs_if_changed(&self, before: &ServerConfig) {
        let skill_keys = |c: &ServerConfig| {
            (
                c.extra_skill_dirs.clone().unwrap_or_default(),
                c.disabled_skills.clone().unwrap_or_default(),
            )
        };
        let after = self.config.read().unwrap_or_else(|e| e.into_inner());
        if skill_keys(before) != skill_keys(&after) {
            drop(after);
            self.resync_skill_catalogs();
        }
    }

    /// Attach a [`PluginManager`] to the server. When present, `plugin.*`
    /// RPC methods return real data, and plugin-provided MCP servers, hooks,
    /// skills, and commands are wired into the engine.
    pub fn with_plugin_manager(mut self, manager: Arc<mirri_plugin_host::PluginManager>) -> Self {
        self.plugin_manager = Some(manager);
        self
    }

    /// Mount the skill-directory watcher on every BUILT runtime that does
    /// not have one yet (plan Task 5, ADR-010 decision 1's lifetime rule:
    /// the watcher rides the runtime). Idempotent — a runtime that already
    /// holds a watcher is skipped — and best-effort (decision 8): a runtime
    /// whose watcher cannot start logs a `warn!` and keeps running without
    /// automatic awareness, leaving the manual reload path unaffected.
    ///
    /// The snapshot inputs (config / home / plugin manager / skill dirs)
    /// are captured per-mount; the watcher task itself re-derives its
    /// listen set after every completed reload, so config-driven extra
    /// dirs and plugin-root changes land on the next refresh without a
    /// re-mount.
    /// Fire-and-forget mount pass (the lazy call_method seam): every
    /// pending workspace's watcher spawns; the mount itself runs on the
    /// blocking pool inside the spawned task. The first RPC pays ~nothing
    /// (the t8 slow-MCP pin keeps its budget).
    pub(crate) fn spawn_skill_watchers(&self) {
        // Nothing to mount when automatic awareness is disabled for the
        // whole engine (the gate's honest-off semantics: no watchers, no
        // file triggers — the manual reload RPC stays available). Tests
        // that need determinism disable the flag explicitly.
        if !self.flag_registry.enabled(crate::SKILL_HOT_RELOAD_FLAG) {
            return;
        }
        // Collect the not-yet-mounted workspaces under the lock, then
        // drop the guard before the awaited spawns (a std MutexGuard is
        // not Send; the lock must never span an await — the same
        // discipline the reload sweep follows).
        let pending: Vec<(Arc<WorkspaceRuntime>, crate::skill_watcher::SkillWatcherInputs)> = {
            let watchers = self
                .skill_watchers_shared
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.registry
                .built_runtimes()
                .into_iter()
                .filter(|runtime| !watchers.contains_key(&runtime.workspace_id))
                .map(|runtime| {
                    let inputs = crate::skill_watcher::SkillWatcherInputs {
                        config: Arc::clone(&self.config),
                        home_dir: self.home_dir.clone(),
                        plugin_manager: self.plugin_manager.clone(),
                        skill_dirs: self.skill_dirs.clone(),
                    };
                    (runtime, inputs)
                })
                .collect()
        };
        for (runtime, inputs) in pending {
            let skill_reload = Arc::clone(&self.skill_reload);
            let skill_watchers = Arc::clone(&self.skill_watchers_shared);
            // The spawned mount task: the async spawn awaits the
            // blocking-pool mount, then registers the handle (or the
            // tombstone). The JoinHandle is tracked so a manual
            // `skill.reload` can await the outstanding mounts before it
            // answers `watching` — the truthfulness seam.
            let task = tokio::spawn(async move {
                let mounted =
                    crate::skill_watcher::spawn_skill_watcher(&runtime, &skill_reload, inputs)
                        .await;
                let mut watchers = skill_watchers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                match mounted {
                    Some(watcher) => {
                        watchers.insert(runtime.workspace_id.clone(), Some(watcher));
                    }
                    None => {
                        // Decision 8's degraded-honesty surface: no watcher
                        // for this workspace — a tombstone records the
                        // absence so the lazy pass does not retry the
                        // failed spawn on every call (the manual reload RPC
                        // remains the fallback; `watching` in its response
                        // is derived from live handles only, so a
                        // tombstone answers false).
                        tracing::warn!(
                            workspace = %runtime.workspace_id,
                            "skill hot-reload is disabled for this workspace (watcher creation failed)"
                        );
                        watchers.insert(runtime.workspace_id.clone(), None);
                    }
                }
            });
            // Track the outstanding mount for the awaitable seam.
            self.pending_mounts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(task);
        }
    }

    /// Await every outstanding watcher mount (the `skill.reload` seam):
    /// when this resolves, every workspace's `watching` answer is settled
    /// and the watchers are live — a caller that then writes a skill file
    /// is observed (no spawn-to-mount registration race).
    pub(crate) async fn await_skill_watcher_mounts(&self) {
        let tasks: Vec<_> = std::mem::take(
            &mut *self.pending_mounts.lock().unwrap_or_else(|e| e.into_inner()),
        );
        for task in tasks {
            // Bounded: a mount over an unreachable home directory (NFS
            // hang, dead mount) must not turn the manual reload RPC into
            // an unbounded wait. On timeout the task keeps running (a
            // late mount still registers — the tombstone path covers the
            // failure case); the reload's `watching` answers settle on
            // whatever registered by then.
            let _ = tokio::time::timeout(MOUNT_AWAIT_TIMEOUT, task).await;
        }
    }

    /// Unmount the skill-directory watcher of a removed workspace (the
    /// registry-remove path): dropping the handle signals the watcher
    /// task's shutdown channel and the task exits (the anti-leak design —
    /// a spawned task holding a notify watcher never stops on its own).
    pub(crate) fn unmount_skill_watcher(&self, workspace_id: &str) {
        self.skill_watchers_shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(workspace_id);
    }

    /// The skill-catalog reload orchestrator (ADR-010): the serialized
    /// entry point every skill rescan goes through, with its workspace-level
    /// change-event broadcast. Shared behind an `Arc` so the per-workspace
    /// watcher tasks (plan Task 5) hold the SAME orchestrator — a reload
    /// they trigger fans out on this server's broadcast channel. Read-only
    /// consumers (the `skill.reload` RPC handler and the event subscription
    /// surface, plan Tasks 5/6) go through this handle.
    pub fn skill_reload(&self) -> &Arc<crate::skill_reload::SkillReloadOrchestrator> {
        &self.skill_reload
    }

    /// Attach an explicit Mirri home directory to a server built through
    /// the provider-injection constructors ([`Self::with_profile_manager`],
    /// [`Self::with_providers`], …) — the same value
    /// [`Self::for_workspace_root`] sets for a full home-rooted build.
    ///
    /// RPC handlers that resolve home-relative paths at call time
    /// (`session.export`) prefer it over the env-based resolution. The NAPI
    /// mock factories ([`mirri_node`]'s `createEngineWithMockLlm` /
    /// `createEngineWithScriptedMockLlm`) thread the TS harness's isolated
    /// `homeDir` through here so mock engines never persist into the real
    /// home or the process working directory.
    pub fn with_home_dir(mut self, home_dir: Option<PathBuf>) -> Self {
        self.home_dir = home_dir;
        self
    }

    /// Attach an explicit MCP user-layer base to a server built through the
    /// provider-injection constructors — the directory whose
    /// `/.mirri-code/mcp.json` the `mcp.config.*` surface and the reconcile
    /// kernel address. [`Self::from_parts`] sets it from the bootstrap's pool
    /// load automatically; this is the seam for hosts (and tests) that wire
    /// the pools themselves.
    pub fn with_mcp_home_base(mut self, mcp_home_base: Option<PathBuf>) -> Self {
        self.mcp_home_base = mcp_home_base;
        self
    }

    /// Attach an explicit files-store root to a server built through the
    /// provider-injection constructors — the directory the `POST /files`
    /// uploads and the `message.send` entry resolution share. Without it
    /// the root resolves at call time from the injected Mirri home (or the
    /// ambient `MIRRICODE_HOME` / `$HOME/.mirri-code`, the same resolution
    /// [`Self::home_root`] uses) plus `files/`.
    pub fn with_files_dir(mut self, files_dir: PathBuf) -> Self {
        self.files_dir = Some(files_dir);
        self
    }

    /// Override the models.dev upstream the
    /// `providerDiscovery.refreshProviderModels` pull fetches — the test
    /// seam pointing the refresh at a wiremock endpoint, or `None` to
    /// disable the remote fetch entirely (the hermetic default the test
    /// servers set, so no test hits the real network).
    pub fn with_models_dev_url(mut self, url: Option<String>) -> Self {
        self.models_dev_url = url;
        self
    }

    /// The Mirri home this server roots its home-scoped consumers in: the
    /// injected override when present, else the ambient resolution chain
    /// (`MIRRICODE_HOME`, then `$HOME/.mirri-code`) — the same resolution
    /// [`Self::home_root`] builds on. Profile **discovery** runs in
    /// the funnel (bootstrap) over the user agents roots plus this home's
    /// legacy `profiles/`; this root feeds the write channel and the
    /// provenance probes (R3 lifts both onto the discovery result).
    pub(crate) fn home_root(&self) -> PathBuf {
        if let Some(home) = &self.home_dir {
            home.clone()
        } else if let Some(mirri_home) =
            std::env::var_os("MIRRICODE_HOME").filter(|value| !value.is_empty())
        {
            PathBuf::from(mirri_home)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".mirri-code")
        } else {
            PathBuf::from(".mirri-code")
        }
    }

    /// Read `<brandHome>/SYSTEM.md` as the default-base override (D21): the
    /// raw text when the file exists and is non-empty (whitespace-only
    /// counts as empty — the TS `loadSystemMdProfile` trim check), `None`
    /// otherwise. A read failure degrades to `None` with a warn instead of
    /// failing the caller — a transient fs error never poisons a session
    /// (the same degradation the TS loader applies). Shared by the session
    /// config seeding (the runtime consumption) and the profile wire rows
    /// (the settings surface), both through the injected [`SyncFs`] state
    /// plane (zero-IO T13b).
    pub(super) fn read_system_md_override(&self) -> Option<String> {
        let path = self.home_root().join("SYSTEM.md");
        let fs = mirri_os::LocalSyncFs;
        if !fs.exists(&path) {
            return None;
        }
        match fs.read_text(&path) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "SYSTEM.md read failed; keeping the builtin base prompt"
                );
                None
            }
        }
    }

    /// The files-store root: the injected override when present, else the
    /// Mirri home's `files/` subdirectory (the TS engine's convention,
    /// resolved exactly like [`Self::home_root`]).
    fn files_root(&self) -> PathBuf {
        if let Some(files_dir) = &self.files_dir {
            return files_dir.clone();
        }
        let home = if let Some(home) = &self.home_dir {
            home.clone()
        } else if let Some(mirri_home) =
            std::env::var_os("MIRRICODE_HOME").filter(|value| !value.is_empty())
        {
            PathBuf::from(mirri_home)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".mirri-code")
        } else {
            PathBuf::from(".mirri-code")
        };
        home.join("files")
    }

    /// Read-only access to the plugin manager, when one is attached.
    pub fn plugin_manager(&self) -> Option<&Arc<mirri_plugin_host::PluginManager>> {
        self.plugin_manager.as_ref()
    }

    /// The managed Mirri env merged into plugin stdio servers at connection
    /// time (item A7 — the TS `managedMirriCodeEnvForPlugins` +
    /// `withManagedMirriPluginEnv` pair in pluginService.ts): an ambient
    /// `MIRRICODE_BASE_URL` (trailing slashes stripped — the TS
    /// `replace(/\/+$/, '')` is applied only to the env override) or the
    /// configured `mirri-code` provider's `baseUrl`. Only resolved values
    /// are returned (the TS `if (baseUrl !== undefined)` gating); an empty
    /// map means "inject nothing" (the TS early return when the managed
    /// env has no keys). The EMB engine has no external base URL of its
    /// own — the HTTP socket lives in the host process (`mirri-server` /
    /// `mirri-cli`) — so there is no self-derived default (decision D-A7).
    fn managed_mirri_code_env_for_plugins(&self) -> HashMap<String, String> {
        // Single-source spelling of the managed provider id, mirroring the
        // removed `MIRRICODE_PROVIDER_NAME` constant from mirri-auth.
        const MANAGED_PROVIDER_ID: &str = "mirri-code";
        let env_base_url = std::env::var("MIRRICODE_BASE_URL")
            .ok()
            .filter(|value| !value.is_empty());

        let base_url = env_base_url
            .map(|value| value.trim_end_matches('/').to_string())
            .or_else(|| {
                self.config
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .providers
                    .as_ref()
                    .and_then(|providers| providers.get(MANAGED_PROVIDER_ID))
                    .and_then(|p| p.base_url.clone())
            });

        let mut env = HashMap::new();
        if let Some(base_url) = base_url {
            env.insert("MIRRICODE_BASE_URL".to_string(), base_url);
        }
        env
    }

    /// The plugin-contributed MCP server configs as the connection layer
    /// sees them (item A7):
    /// [`mirri_plugin_host::PluginManager::enabled_mcp_servers`] — the
    /// manager-exit decoration (cwd default + `MIRRICODE_HOME` /
    /// `MIRRI_PLUGIN_ROOT`) — plus the managed Mirri env merge, the TS
    /// service layer's `withManagedMirriPluginEnv` applied to every stdio
    /// server.
    fn enabled_plugin_mcp_servers_decorated(&self) -> HashMap<String, Value> {
        let managed_env = self.managed_mirri_code_env_for_plugins();
        let mut out = HashMap::new();
        if let Some(plugin_manager) = &self.plugin_manager {
            for (runtime_name, raw) in plugin_manager.enabled_mcp_servers() {
                out.insert(
                    runtime_name,
                    apply_managed_mirri_plugin_env(raw, &managed_env),
                );
            }
        }
        out
    }

    /// Reflect a plugin-contributed MCP server's current user state in the
    /// live connection set: an enabled server connects under its runtime
    /// name (replacing any stale entry), a disabled one unregisters. The
    /// config comes from the plugin's effective state (manifest overlaid
    /// with the per-server user state), so a tool-filter change reconnects
    /// with the new filter in effect. Without an MCP manager this is a
    /// no-op — the state write still persists. Targets the default
    /// workspace's manager ([`Self::mcp_manager`]); per-workspace reloads
    /// use [`Self::reconcile_plugin_mcp_connections`], which drives the
    /// same kernel on the runtime's own manager.
    async fn sync_plugin_mcp_connection(&self, runtime_name: &str) {
        let Some(mcp_manager) = &self.mcp_manager else {
            return;
        };
        let Some(plugin_manager) = &self.plugin_manager else {
            return;
        };
        let managed_env = self.managed_mirri_code_env_for_plugins();
        Self::sync_plugin_mcp_connection_on(
            mcp_manager,
            plugin_manager,
            &managed_env,
            runtime_name,
        )
        .await;
    }

    /// The `sync_plugin_mcp_connection` kernel, parameterized over the MCP
    /// manager and plugin manager — shared by the per-server write path
    /// (above) and the reload reconcile (item C4), which runs it on every
    /// built workspace's own pool manager. `managed_env` is the item A7
    /// managed Mirri env — merged into stdio configs exactly where the
    /// connection layer consumes them (the TS service-layer application
    /// timing).
    async fn sync_plugin_mcp_connection_on(
        mcp_manager: &Arc<mirri_mcp::McpConnectionManager>,
        plugin_manager: &mirri_plugin_host::PluginManager,
        managed_env: &HashMap<String, String>,
        runtime_name: &str,
    ) {
        match plugin_manager.enabled_mcp_servers().get(runtime_name) {
            Some(raw) => {
                let raw = apply_managed_mirri_plugin_env(raw.clone(), managed_env);
                match serde_json::from_value::<mirri_mcp::McpServerConfig>(raw) {
                    Ok(config) => {
                        if let Err(error) = mcp_manager.connect_one(runtime_name, config).await {
                            tracing::warn!(server = %runtime_name, %error, "plugin MCP server failed to connect");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(server = %runtime_name, %error, "plugin MCP server config invalid");
                    }
                }
            }
            None => {
                if let Err(error) = mcp_manager.remove(runtime_name).await {
                    tracing::warn!(server = %runtime_name, %error, "plugin MCP server failed to disconnect");
                }
            }
        }
    }

    /// Refresh every consumer the workspace runtimes derive from the
    /// plugin manager after a `plugin.reload` (item C4 — the TS
    /// `onDidReload` re-registration): the agent-profile manager re-runs
    /// the combined user+plugin discovery pass, and every built workspace's
    /// runtime re-derives its skill catalog, its hook engine (base +
    /// current plugin hooks), and its plugin MCP connections. Existing
    /// sessions keep their assembly-time system prompt / sessionStart /
    /// hook registrations — the refreshed values apply to new sessions
    /// (the documented difference, see the parity checklist I row).
    async fn refresh_plugin_consumers(&self) {
        let Some(plugin_manager) = &self.plugin_manager else {
            return;
        };
        self.refresh_plugin_profiles();
        let config_snapshot = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for runtime in self.registry.built_runtimes() {
            // The skill replay goes through the serialized reload
            // orchestrator (ADR-010): same discovery sequence, plus the
            // atomic swap, the diff gate and the notification loop every
            // other trigger uses (plugin trigger).
            let ctx = crate::skill_reload::SkillReloadContext {
                config: &config_snapshot,
                home_dir: self.home_dir.as_deref(),
                plugin_manager: Some(plugin_manager),
                skill_dirs: &self.skill_dirs,
            };
            self.skill_reload
                .reload_workspace(&runtime, &ctx, crate::skill_reload::SkillReloadTrigger::Plugin);
            runtime.refresh_plugin_hooks(plugin_manager.enabled_hooks());
            self.reconcile_plugin_mcp_connections(&runtime).await;
        }
    }

    /// Re-run the combined profile discovery (user roots + plugin agent
    /// roots, item H) and swap the manager's registrations wholesale —
    /// the watcher's reload pattern (clear + register + suppressed
    /// projection), applied on `plugin.reload` so enabled plugins' agent
    /// roots land while plugin profiles removed by the reload drop out.
    /// A discovery-level failure keeps the previous registrations.
    fn refresh_plugin_profiles(&self) {
        let Some(profile_manager) = &self.profile_manager else {
            return;
        };
        let roots = crate::bootstrap::profile_discovery_roots(
            self.home_dir.as_deref(),
            self.plugin_manager.as_deref(),
        );
        match mirri_core::profile::discover_profiles(&roots, &mirri_os::LocalSyncFs) {
            Ok(discovery) => {
                let mut loaded = 0;
                profile_manager.clear();
                for discovered in &discovery.profiles {
                    if let Err(error) = profile_manager.register_discovered(discovered) {
                        tracing::warn!(
                            %error,
                            name = %discovered.profile.name,
                            "plugin-reload profile registration rejected; keeping the built-in"
                        );
                        continue;
                    }
                    loaded += 1;
                }
                profile_manager.set_suppressed_candidates(discovery.suppressed);
                for skipped in &discovery.skipped {
                    tracing::warn!(
                        path = %skipped.path.display(),
                        reason = %skipped.reason,
                        "agent profile file skipped during plugin reload"
                    );
                }
                tracing::info!(
                    profile_count = loaded,
                    "agent profiles refreshed after plugin reload"
                );
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "plugin reload profile discovery failed; keeping current profiles"
                );
            }
        }
    }

    /// Reconcile one built workspace's plugin MCP connections (item C4):
    /// the desired set is the plugin manager's currently enabled servers;
    /// every connected plugin-runtime name (`plugin-<id>:<name>`) missing
    /// from it disconnects, every desired name connects with its fresh
    /// config (replacing any stale entry, tool filters included).
    ///
    /// The manager addressed follows the codebase's plugin-MCP convention:
    /// the startup connect and the per-server write path both drive
    /// [`Self::mcp_manager`] (the default workspace's pool manager —
    /// `EngineParts.mcp_manager` IS the default runtime's manager, so the
    /// two are the same `Arc` in production); the reconcile addresses that
    /// manager for the default runtime and the runtime's own pool manager
    /// for every other built workspace.
    async fn reconcile_plugin_mcp_connections(&self, runtime: &WorkspaceRuntime) {
        let Some(plugin_manager) = &self.plugin_manager else {
            return;
        };
        let mcp_manager = if self.registry.default_workspace_id().as_deref()
            == Some(runtime.workspace_id.as_str())
        {
            self.mcp_manager.clone()
        } else {
            runtime.mcp_manager.clone()
        };
        let Some(mcp_manager) = mcp_manager else {
            return;
        };
        let desired: std::collections::HashSet<String> = plugin_manager
            .enabled_mcp_servers()
            .keys()
            .cloned()
            .collect();
        for (connected_name, _state) in mcp_manager.list_connections().await {
            if connected_name.starts_with("plugin-")
                && !desired.contains(&connected_name)
                && let Err(error) = mcp_manager.remove(&connected_name).await
            {
                tracing::warn!(
                    server = %connected_name,
                    %error,
                    "plugin MCP server failed to disconnect during reload"
                );
            }
        }
        let managed_env = self.managed_mirri_code_env_for_plugins();
        for runtime_name in desired {
            Self::sync_plugin_mcp_connection_on(
                &mcp_manager,
                plugin_manager,
                &managed_env,
                &runtime_name,
            )
            .await;
        }
    }

    /// Lazily spawn `connect_all` for MCP configs discovered at startup.
    /// Called on the first `call_method` invocation, which is guaranteed
    /// to run inside the Tokio runtime. Uses an atomic flag to ensure the
    /// spawn happens exactly once. After this, a `StatusListener` keeps
    /// tool registrations in sync as servers come and go.
    ///
    /// Under the three-pool model the discovered layers route to their
    /// pools (spec §4.2): the project layers plus plugin-contributed
    /// servers connect on the **workspace manager** (the historical
    /// behavior), while the user layer connects on the **user pool** —
    /// claimed once per pool, so engines sharing a pool never re-connect
    /// the user layer.
    fn maybe_spawn_mcp_connect(&self) {
        use std::sync::atomic::Ordering;

        if self.mcp_connect_spawned.swap(true, Ordering::SeqCst) {
            return;
        }

        let layered = self
            .pending_mcp_configs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default();

        // --- workspace pool: project layers + plugin-contributed servers ---
        // Plugin-contributed servers join the startup connect set under
        // their runtime names (a plugin server can never collide with a
        // user-config name, A1), already decorated: the manager exit adds
        // the cwd default and `MIRRICODE_HOME`/`MIRRI_PLUGIN_ROOT`, and
        // the managed Mirri env merge follows (item A7 — the TS service
        // layer application timing). A config too broken to parse is
        // skipped with a warning rather than blocking the rest.
        let mut workspace_configs = layered.project_merged();
        let mut plugin_names = Vec::new();
        // The merged set is produced through the server (not the raw
        // manager) exactly for that managed-env merge — the chronicle of
        // where decoration happens vs. where the connection layer reads.
        for (runtime_name, raw) in self.enabled_plugin_mcp_servers_decorated() {
            match serde_json::from_value::<mirri_mcp::McpServerConfig>(raw) {
                Ok(config) => {
                    plugin_names.push(runtime_name.clone());
                    workspace_configs.insert(runtime_name, config);
                }
                Err(error) => {
                    tracing::warn!(server = %runtime_name, %error, "plugin MCP server config invalid, skipping");
                }
            }
        }
        // The workspace set's names shadow same-name user entries (spec
        // §4.2 view semantics): a user server whose name a project/plugin
        // entry shares stays connected in the pool, but its tools never
        // reach agents and `mcp.list` shows only the workspace entry.
        // Snapshotted from the config view before the connect set is
        // moved into the spawned task, so the check is race-free.
        let shadowed: std::collections::HashSet<String> =
            workspace_configs.keys().cloned().collect();
        // The pool listeners are attached unconditionally (also for an
        // empty startup set) — a later config-write reconcile may connect
        // into this pool, and its tools reach the live agents through
        // exactly this listener.
        self.ensure_mcp_pool_listeners(&shadowed);
        if !workspace_configs.is_empty()
            && let Some(manager) = &self.mcp_manager
        {
            let mgr = Arc::clone(manager);
            let plugin_manager = self.plugin_manager.clone();
            let startup = Arc::clone(&self.mcp_startup);
            self.mcp_startup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_task();
            tokio::spawn(async move {
                let results = mgr.connect_all(workspace_configs).await;
                for (name, result) in &results {
                    if let Err(error) = result {
                        tracing::warn!(server = %name, %error, "MCP server failed to connect")
                    }
                }
                // A per-server disable may have landed between the
                // snapshot above and registration — `remove` on a
                // not-yet-registered entry no-ops, so the stale snapshot
                // would have connected a server the user state says is
                // disabled and nothing later undoes it. Reconcile
                // against the fresh state (remove is race-safe: it
                // bumps `attempt_id` so an in-flight connect result
                // cannot resurrect the entry).
                if let Some(plugin_manager) = plugin_manager {
                    let fresh: std::collections::HashSet<String> = plugin_manager
                        .enabled_mcp_servers()
                        .keys()
                        .cloned()
                        .collect();
                    for runtime_name in plugin_names {
                        if !fresh.contains(&runtime_name) {
                            tracing::info!(server = %runtime_name, "plugin MCP server disabled during startup connect, removing");
                            if let Err(error) = mgr.remove(&runtime_name).await {
                                tracing::warn!(server = %runtime_name, %error, "plugin MCP server failed to disconnect");
                            }
                        }
                    }
                }
                startup
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .finish_task();
            });
        }

        // --- user pool: the user config layer, connected once per pool ---
        // The A7 cross-pool view (`user_merged`): the user layer's entries
        // with the project layers' filter-only overrides applied — the same
        // shape the reconcile desired set uses, so a workspace-scope filter
        // override on a user-defined name is in effect from the connect and
        // a later reconcile is a no-op rather than a divergence.
        let user_configs = layered.user_merged();
        if user_configs.is_empty() {
            return;
        }
        let Some(pool) = &self.user_mcp_pool else {
            return;
        };
        let manager = pool.manager();

        // A refused claim means another engine sharing this pool already
        // connected the user layer — that engine's startup window covers
        // it, so this engine does not count the task (its own metrics
        // reflect only its workspace connect set, if any).
        if !pool.claim_connect() {
            return; // another engine sharing this pool already connected it
        }
        let startup = Arc::clone(&self.mcp_startup);
        self.mcp_startup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_task();
        tokio::spawn(async move {
            let results = manager.connect_all(user_configs).await;
            for (name, result) in &results {
                if let Err(error) = result {
                    tracing::warn!(server = %name, %error, "user-layer MCP server failed to connect")
                }
            }
            startup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish_task();
        });
    }

    /// The per-workspace lazy project-layer connect (W7, design 08 §2):
    /// every **built non-default** runtime's project-layer servers connect
    /// exactly once, on an engine call — the same lazy pattern as
    /// [`Self::maybe_spawn_mcp_connect`], fanned out per workspace. The
    /// default runtime's connect (the plugin merge and the user-pool claim
    /// included) stays the legacy path's job; this pass only ever connects
    /// `project_merged()` sets, so the user pool keeps a single connection
    /// per server across N workspaces.
    ///
    /// Each workspace's pool gets its own status listener (fanning to that
    /// workspace's own sessions — per-workspace isolation) and the shared
    /// user-pool shadow view refreshes to include the newly loaded
    /// workspaces' project names. An untrusted workspace's layered config
    /// is user-only by construction (the build-time gate), so its connect
    /// set is empty — nothing from an untrusted tree ever connects.
    fn maybe_spawn_workspace_pool_connects(&self) {
        use std::sync::atomic::Ordering;

        let default_id = self.registry.default_workspace_id();
        for runtime in self.registry.built_runtimes() {
            if Some(runtime.workspace_id.as_str()) == default_id.as_deref() {
                continue; // the default runtime's connect is the legacy path's job
            }
            if runtime.mcp_connect_spawned.swap(true, Ordering::SeqCst) {
                continue; // already spawned for this runtime
            }
            let Some(manager) = runtime.mcp_manager.clone() else {
                continue; // no pool wired (no MCP discovery for this root)
            };
            // This workspace's own listener: its pool's transitions reach
            // its own sessions only.
            let mgr_for_listener = Arc::clone(&manager);
            let session_managers = vec![Arc::clone(&runtime.manager)];
            let auth_dir = self.mcp_oauth_token_dir();
            let pool_events = self.mcp_pool_events.clone();
            manager.add_status_listener(Box::new(move |server_name, state| {
                handle_pool_status_transition(
                    &mgr_for_listener,
                    &session_managers,
                    &auth_dir,
                    &pool_events,
                    "workspace",
                    &server_name,
                    state,
                );
            }));
            // The user-pool shadow view now includes this workspace's
            // project-layer names (a same-name user entry stops registering
            // its tools, spec §4.2).
            self.ensure_mcp_pool_listeners(&self.workspace_shadow_view());

            let layered = runtime.pending_mcp_configs.clone().unwrap_or_default();
            let workspace_configs = layered.project_merged();
            if workspace_configs.is_empty() {
                continue; // nothing to connect (possibly an untrusted runtime)
            }
            let startup = Arc::clone(&self.mcp_startup);
            self.mcp_startup
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_task();
            let workspace_id = runtime.workspace_id.clone();
            tokio::spawn(async move {
                let results = manager.connect_all(workspace_configs).await;
                for (name, result) in &results {
                    if let Err(error) = result {
                        tracing::warn!(
                            workspace = %workspace_id,
                            server = %name,
                            %error,
                            "workspace-layer MCP server failed to connect"
                        )
                    }
                }
                startup
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .finish_task();
            });
        }
    }

    /// C1 懒加载：首次目录数据使用时从本地磁盘缓存恢复（N1：一次性本地
    /// 读，量级 ~100-300ms；N2：失败静默降级——缺/坏缓存保持 `None`，
    /// 与刷新路径的既有降级语义一致）。幂等：目录已非 `None` 或
    /// `persist_cache` 关闭（R6：不读）时直接返回；预热任务与首次使用
    /// 先到先得——都经过同一把锁。
    pub(super) fn ensure_models_dev_catalog_loaded(&self) {
        Self::load_models_dev_catalog_from_disk(
            &self.models_dev_catalog,
            &self.config,
            self.models_dev_cache_path(),
        );
    }

    /// Lazily spawn the models.dev catalog background preheat on the first
    /// `call_method` invocation — the [`Self::maybe_spawn_mcp_connect`]
    /// pattern: `for_workspace_root` / `from_parts` may run before the
    /// Tokio runtime exists, while the first RPC call is guaranteed to run
    /// inside one. Zero network (the preheat only reads the local cache
    /// file, never fetches) and zero blocking at the call site (the read
    /// happens inside the spawned task). Idempotent: the load itself
    /// first-arrival-wins on the catalog mutex, so a list call that wins
    /// the race reduces the preheat to a no-op and vice versa.
    fn maybe_spawn_models_catalog_preheat(&self) {
        use std::sync::atomic::Ordering;

        if self.models_preheat_spawned.swap(true, Ordering::SeqCst) {
            return;
        }
        let catalog = Arc::clone(&self.models_dev_catalog);
        let config = Arc::clone(&self.config);
        let cache_path = self.models_dev_cache_path();
        tokio::spawn(async move {
            Self::load_models_dev_catalog_from_disk(&catalog, &config, cache_path);
        });
    }

    /// Run the one-time legacy flat-layout session migration before the
    /// first dispatched RPC — the [`Self::maybe_spawn_mcp_connect`] seam
    /// (construction may precede the Tokio runtime; the first
    /// `call_method` never does).
    ///
    /// Unlike the MCP connect, this is **awaited inline** rather than
    /// spawned: a legacy flat session must already live in its workspace
    /// bucket by the time the first `session.resume` / `session.list`
    /// dispatch reads the sessions root, so the migration settles before
    /// the RPC observes the directory. The work is bounded local-disk I/O
    /// (usually zero flat sessions — the common case is a single iterdir).
    /// Best-effort: a failure logs a warning and must not abort the call
    /// (or the engine); the migration is idempotent and retries on the next
    /// start. See [`SessionManager::migrate_legacy_flat_sessions`].
    async fn migrate_legacy_sessions_once(&self) {
        use std::sync::atomic::Ordering;

        if self.legacy_sessions_migrated.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.session_manager().migrate_legacy_flat_sessions().await {
            Ok(migrated) if migrated > 0 => tracing::info!(
                migrated,
                "legacy flat sessions migrated into their workspace buckets"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                "legacy flat-session migration failed; the dual-layout reads keep \
                 the sessions visible and the migration retries on the next start"
            ),
        }
    }

    /// Shared lazy-load body — the call-time
    /// [`Self::ensure_models_dev_catalog_loaded`] and the preheat task
    /// both funnel through here: the first lock arrival wins, the second
    /// no-ops (the early-return guards).
    fn load_models_dev_catalog_from_disk(
        catalog: &std::sync::Mutex<Option<std::sync::Arc<Value>>>,
        config: &Arc<RwLock<ServerConfig>>,
        cache_path: std::path::PathBuf,
    ) {
        let mut guard = catalog.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() || !config.read().map_or(true, |c| c.persist_cache_enabled()) {
            return;
        }
        // The on-disk cache first (the last successful fetch — the freshest
        // local directory); a missing/unreadable/unparseable cache falls back
        // to the bundled snapshot. In-memory only: the snapshot is never
        // written to the cache file — the disk cache stays fetch-only, so a
        // later refresh can still tell "cached" from "bundled" by the file.
        let Ok(text) = std::fs::read_to_string(cache_path) else {
            *guard = crate::embedded_server::models_dev_import::bundled_models_dev_snapshot()
                .map(std::sync::Arc::new);
            return;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => {
                *guard = Some(Arc::new(v));
            }
            _ => {
                // 坏缓存：保持 None（刷新路径的既有降级语义一致）→ 回落内置
                // 快照，仅区别于显式持久化关闭（R6）。
                *guard = crate::embedded_server::models_dev_import::bundled_models_dev_snapshot()
                    .map(std::sync::Arc::new);
            }
        }
    }

    /// Attach the engine's pool status listeners, once, and refresh the
    /// workspace-layer shadow view (every call — T1 review F-T1-3).
    ///
    /// One listener goes on the workspace pool and one on the user pool —
    /// both drive the same follow-ups through
    /// [`handle_pool_status_transition`]: the live agents' tool tables
    /// stay in sync as servers come and go (the S2 race fix and S1
    /// teardown), and every transition is published to the live sessions'
    /// event buses as the existing `mcp.server.status` session event (the
    /// overlay pool's precedent — no new event vocabulary, spec §4.3's
    /// notification leg reuses the established mechanism).
    ///
    /// The user pool's listener filters the names the workspace config
    /// layers shadow (spec §4.2): the workspace entry owns those names in
    /// the agents' tool tables. The shadow set lives in shared storage
    /// ([`Self::mcp_user_pool_shadow`]) and is refreshed by every call —
    /// the first (startup) and any later (reconcile) — so a hot-added
    /// same-name workspace server takes the name over from that moment.
    fn ensure_mcp_pool_listeners(&self, shadowed: &std::collections::HashSet<String>) {
        use std::sync::atomic::Ordering;

        // Always refresh the shadow view first — the listener reads it per
        // event, so a later refresh applies to the already-attached listener.
        {
            let mut view = self
                .mcp_user_pool_shadow
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *view = shadowed.clone();
        }

        if self
            .mcp_pool_listeners_attached
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        if let Some(manager) = &self.mcp_manager {
            let mgr_for_listener = Arc::clone(manager);
            // The default workspace's pool fans to the default workspace's
            // sessions only — per-workspace isolation (design 08 §2): a
            // project-layer server of workspace A never reaches workspace
            // B's agents. (`self.manager` IS the default runtime's manager.)
            let session_managers = vec![Arc::clone(&self.manager)];
            let auth_dir = self.mcp_oauth_token_dir();
            let pool_events = self.mcp_pool_events.clone();
            manager.add_status_listener(Box::new(move |server_name, state| {
                // The workspace pool holds both project-layer entries and
                // plugin-contributed runtime names (the reserved `plugin-`
                // prefix) — the payload's `pool` field tells them apart.
                let pool = if server_name.starts_with("plugin-") {
                    "plugin"
                } else {
                    "workspace"
                };
                handle_pool_status_transition(
                    &mgr_for_listener,
                    &session_managers,
                    &auth_dir,
                    &pool_events,
                    pool,
                    &server_name,
                    state,
                );
            }));
        }

        if let Some(pool) = &self.user_mcp_pool {
            let manager = pool.manager();
            let mgr_for_listener = Arc::clone(&manager);
            // The user pool is shared across every workspace of the engine
            // (design 08 §2 reuse): its events fan to every BUILT runtime's
            // sessions, resolved through the registry at event time so a
            // workspace built after this attach is covered too.
            let registry = Arc::clone(&self.registry);
            let shadow_for_listener = Arc::clone(&self.mcp_user_pool_shadow);
            let auth_dir = self.mcp_oauth_token_dir();
            let pool_events = self.mcp_pool_events.clone();
            manager.add_status_listener(Box::new(move |server_name, state| {
                if shadow_for_listener
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(server_name.as_str())
                {
                    return; // the workspace entry owns this name — not the user layer
                }
                let session_managers: Vec<Arc<SessionManager>> = registry
                    .built_runtimes()
                    .into_iter()
                    .map(|runtime| Arc::clone(&runtime.manager))
                    .collect();
                handle_pool_status_transition(
                    &mgr_for_listener,
                    &session_managers,
                    &auth_dir,
                    &pool_events,
                    "user",
                    &server_name,
                    state,
                );
            }));
        }
    }

    /// The OAuth token store directory for MCP servers: `<mirri home>/mcp-tokens`
    /// (the `mirri_mcp::oauth::TokenStore` layout). `None` when no home can
    /// be resolved — OAuth servers then behave as if no tokens were cached
    /// (the plain connect observes the server's 401 and lands in
    /// `needs-auth`, where the authenticate tool reports the missing store).
    fn mcp_oauth_token_dir(&self) -> Option<std::path::PathBuf> {
        match &self.home_dir {
            Some(home) => Some(home.join("mcp-tokens")),
            None => mirri_core::config::mirri_home_dir()
                .ok()
                .map(|home| home.join("mcp-tokens")),
        }
    }

    /// Create a server on a fresh workspace rooted at the current working
    /// directory. Used by the NAPI bridge's default constructor; see
    /// [`for_workspace_root`](Self::for_workspace_root) for what is wired.
    pub fn for_cwd() -> Self {
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::for_workspace_root(
            WorkspaceId::new("embedded"),
            workspace_root,
            None,
            Vec::new(),
        )
    }

    /// Create a fully-wired server on a fresh workspace rooted at the given
    /// directory, with an optional explicitly injected Mirri home.
    ///
    /// `home_dir` (`None` = resolve through the ambient environment, exactly
    /// like the pre-injection behavior) roots every home consumer:
    ///
    /// - `config.toml`, agent profiles, plugins, and MCP config are discovered
    ///   under the injected home instead of `mirri_home_dir()`;
    /// - session persistence moves from the workspace-local
    ///   `<workspace_root>/.mirri/v3_sessions` to `<home>/v3_sessions` —
    ///   the rust engine's own storage root (ADR-006-R2, physically
    ///   separate from the TS engine's `<home>/sessions`) — so an engine
    ///   built for an isolated home never reads or writes another home's
    ///   sessions;
    /// - [`Self::home_dir`] keeps the value for the RPC-time consumers
    ///   (`session.export`).
    ///
    /// `skill_dirs` carries the SDK's explicit skill directories (v1/v2
    /// `skillDirs`): when non-empty they replace the default user/project
    /// skill discovery — see [`crate::bootstrap::assemble`]. Empty keeps
    /// the ambient discovery.
    ///
    /// Used by the ACP binary (rooted at the negotiated workspace, no
    /// injected home) and by [`for_cwd`](Self::for_cwd) / the NAPI
    /// `createEngine(homeDir, skillDirs)` path (which passes the harness
    /// home and skill dirs through).
    ///
    /// Everything a turn needs is auto-discovered and attached, mirroring the
    /// `mirri serve` startup path:
    ///
    /// - `config.toml`, MCP servers (three-layer `mcp.json`), plugins, and
    ///   agent profiles from the Mirri home directory (project-level MCP
    ///   layers resolve against `workspace_root`),
    /// - the config-aware [`ConfigAwareLlmFactory`](crate::ConfigAwareLlmFactory):
    ///   model ids resolve through the configured `[models]` aliases (with
    ///   provider credentials injectable through `ANTHROPIC_API_KEY` /
    ///   `OPENAI_API_KEY` when a provider record carries none), so
    ///   `message.send` runs turns on whatever the user configured,
    /// - a profile manager, which together with the LLM factory registers the
    ///   `Agent` and `AgentSwarm` tools,
    /// - a local SSRF-guarded `FetchURL` provider (`WebSearch` stays
    ///   unregistered until managed-search auth is wired).
    ///
    /// Without provider credentials in the environment the factory fails at
    /// turn time with a human-readable reason; every other subsystem degrades
    /// exactly as it did before this wiring.
    pub fn for_workspace_root(
        workspace_id: WorkspaceId,
        workspace_root: PathBuf,
        home_dir: Option<PathBuf>,
        skill_dirs: Vec<PathBuf>,
    ) -> Self {
        // Thin delegator: the shared composition root (`crate::bootstrap`)
        // assembles the workspace scope, session manager, config, profiles,
        // MCP, plugins and providers under the UntrustedDefault policy —
        // byte-identical to the historical wiring described above.
        let mut server = Self::from_parts(assemble(
            workspace_id,
            workspace_root,
            home_dir,
            None,
            WorkspacePolicy::UntrustedDefault,
            skill_dirs,
        ));
        // The embedded-host pin (design §2.2, W5): an NAPI/ACP host's root is
        // *the* workspace it may list — the home's `workspaces.json` may
        // register more (another host's, the TS engine's), and those sessions
        // are not this host's to see. Serve (which builds through
        // `from_parts` directly) keeps the unrestricted default.
        server.listing_visibility = ListingVisibility::PinnedToDefault;
        server
    }

    /// The explicitly injected Mirri home directory, when the server was
    /// built with one (see [`Self::for_workspace_root`]). RPC handlers that
    /// resolve home-relative paths at call time (`session.export`) prefer
    /// it over the env-based resolution.
    pub fn home_dir(&self) -> Option<&Path> {
        self.home_dir.as_deref()
    }

    /// The experimental-flag registry the `flag.*` RPC methods read.
    /// Hosts (and tests) register definitions through this handle; the
    /// registry starts empty (the honest EMB default).
    pub fn flag_registry(&self) -> &Arc<mirri_core::FlagRegistry> {
        &self.flag_registry
    }

    /// The underlying session manager — lets consumers (tests, tooling)
    /// reach session-scoped APIs that have no RPC equivalent.
    pub fn session_manager(&self) -> Arc<SessionManager> {
        Arc::clone(&self.manager)
    }

    /// Mark a session as active in its workspace — the routing seam's first
    /// level (W3). W1 is single-workspace, so the session is noted against
    /// this engine's default workspace runtime (the only one that can own
    /// sessions this batch); called by `session.create` and the
    /// resume/restore paths.
    pub(super) fn note_active_session(&self, session_id: &str) {
        if let Some(runtime) = self.registry.default_runtime() {
            self.registry.note_active(session_id, &runtime.workspace_id);
        }
    }

    /// Whether a turn of the session is in flight — the `busy` listing
    /// filter's input (the same in-flight-turn registry check
    /// `build_session_view`'s `busy` field performs, hoisted so the filter
    /// can run before the views are built).
    pub(super) fn session_is_busy(&self, session_id: &str) -> bool {
        let prefix = format!("{session_id}:");
        let in_flight = self
            .in_flight_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|key| key.starts_with(&prefix));
        // OR in the activity shadow's aggregate busy (background tasks light
        // it through the fold); if the artifacts can't be resolved, fall
        // back to the in-flight registry check alone.
        let shadow_busy = self
            .resolve_session_artifacts(session_id)
            .map(|a| a.activity_shadow.snapshot_value().busy)
            .unwrap_or(false);
        in_flight || shadow_busy
    }

    /// Mark a session as active in an explicit workspace — the fork /
    /// create-child path (the new session joins the SOURCE session's
    /// workspace, which need not be the default).
    pub(super) fn note_active_session_in(&self, workspace_id: &str, session_id: &str) {
        self.registry.note_active(session_id, workspace_id);
    }

    /// Unmark a session from the active table (the routing seam's first
    /// level) — called by `session.close` and `session.archive`.
    pub(super) fn forget_active_session(&self, session_id: &str) {
        self.registry.forget_active(session_id);
    }

    /// The current server configuration snapshot.
    pub fn config(&self) -> ServerConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The path of the config.toml the current configuration was loaded
    /// from. `None` when the config fell back to defaults (missing HOME or
    /// missing/unparseable config.toml).
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config_path.clone()
    }

    /// Register all connected MCP tools into the given agent's tool manager.
    /// Called during `session.create` and `session.resume` so the agent
    /// has MCP tools available immediately. Idempotent: existing tools with
    /// the same name are replaced.
    ///
    /// The effective view (spec §4.2, with the D6 overlay extension):
    /// overlay tools plus workspace-pool tools plus user-pool tools, minus
    /// every entry a same-name entry of a higher-priority source shadows —
    /// **overlay > workspace > user**. Each tool's bridge routes to the
    /// pool that owns its server.
    pub(super) async fn register_mcp_tools(
        &self,
        agent: &Arc<mirri_core::AgentScope>,
        session_id: &str,
    ) {
        // (server name, tool definition, owning pool) — collected per pool,
        // the higher-priority sources first so their names win the shadow
        // filter: the session's overlay, then the workspace pool, then the
        // user pool.
        let mut tools: Vec<(
            String,
            mirri_mcp::McpToolDefinition,
            Arc<mirri_mcp::McpConnectionManager>,
        )> = Vec::new();
        // needs-auth servers per pool (same priority order): each gets the
        // synthetic authenticate tool instead of real ones (plans/07 T8).
        let mut needs_auth: Vec<(String, Arc<mirri_mcp::McpConnectionManager>)> = Vec::new();
        let mut shadowed = std::collections::HashSet::new();
        // The session overlay comes first — its *configured* names shadow
        // both pools (it is the highest-priority source), before any of
        // its connections even settle. Bound before the block: the map's
        // lock guard must drop before the awaits inside (the future must
        // stay Send).
        let overlay = self
            .session_overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned();
        if let Some(overlay) = overlay {
            shadowed.extend(overlay.names.iter().cloned());
            for (server, def) in overlay.manager.list_all_tools().await {
                shadowed.insert(server.clone());
                tools.push((server, def, Arc::clone(&overlay.manager)));
            }
            for (server, state) in overlay.manager.list_connections().await {
                if matches!(state, mirri_mcp::McpConnectionState::NeedsAuth) {
                    needs_auth.push((server, Arc::clone(&overlay.manager)));
                }
            }
        }
        // The workspace pool of THE SESSION'S OWN WORKSPACE (W7): resolve
        // the owning runtime through the routing seam (the create/resume
        // paths note the session active before this runs), falling back to
        // the default runtime's pool — the historical single-workspace
        // behavior, byte-identical for every direct-constructor server.
        // (The fallback is only reachable when no workspace resolves at
        // all — an unresolvable session of a live create/resume path
        // cannot get here, and direct-constructor servers have no other
        // workspace to fall back from.)
        let workspace_manager = self
            .workspace_pool_for_session(session_id)
            .or_else(|| self.mcp_manager.clone());
        if let Some(manager) = &workspace_manager {
            for (server, def) in manager.list_all_tools().await {
                if shadowed.contains(&server) {
                    continue; // shadowed by the overlay entry
                }
                shadowed.insert(server.clone());
                tools.push((server, def, Arc::clone(manager)));
            }
            for (server, state) in manager.list_connections().await {
                if matches!(state, mirri_mcp::McpConnectionState::NeedsAuth)
                    && !shadowed.contains(&server)
                {
                    needs_auth.push((server, Arc::clone(manager)));
                }
            }
        }
        if let Some(pool) = &self.user_mcp_pool {
            let manager = pool.manager();
            for (server, def) in manager.list_all_tools().await {
                if shadowed.contains(&server) {
                    continue; // shadowed by the overlay or workspace entry
                }
                tools.push((server, def, Arc::clone(&manager)));
            }
            for (server, state) in manager.list_connections().await {
                if matches!(state, mirri_mcp::McpConnectionState::NeedsAuth)
                    && !shadowed.contains(&server)
                {
                    needs_auth.push((server, Arc::clone(&manager)));
                }
            }
        }

        // The authenticate bridges are built before the tool-manager guard
        // is taken (their config lookups await); both registrations then
        // share one critical section.
        let oauth_dir = self.mcp_oauth_token_dir();
        let mut auth_bridges = Vec::new();
        for (server_name, manager) in &needs_auth {
            if let Some(bridge) =
                build_mcp_auth_tool(manager, server_name, oauth_dir.as_deref()).await
            {
                auth_bridges.push((server_name.clone(), bridge));
            }
        }

        if tools.is_empty() && auth_bridges.is_empty() {
            return;
        }

        let tool_manager = agent.tool_manager();
        let mut tm = acquire_tool_table_write(&tool_manager).await;

        for (server_name, def, manager) in tools {
            let qualified = mirri_mcp::qualify_tool_name(&server_name, &def.name);
            let description = if def.description.is_empty() {
                format!("MCP tool from '{server_name}': {}", def.name)
            } else {
                def.description.clone()
            };

            let bridge = mcp_bridge::McpBridgeTool::new(
                &qualified,
                &def.name,
                &server_name,
                &description,
                def.input_schema.clone(),
                manager,
            );
            tm.register_mcp_tool(server_name, Arc::new(bridge));
        }
        for (server_name, bridge) in auth_bridges {
            tm.register_mcp_tool(server_name, Arc::new(bridge));
        }
    }

    /// Install (or replace) the session's MCP overlay: a fresh private
    /// connection manager — with its configured server **names** — is
    /// registered in the overlay table immediately, so the session's own
    /// [`Self::register_mcp_tools`] run shadows same-name workspace/user
    /// entries right away (before any connection settles), while the
    /// connects are spawned in the background: `session/new` never waits
    /// for them (the IDE cold-start rule; a slow MCP server must not
    /// stall the response).
    ///
    /// Per-server connect failures warn and drop just that server; the
    /// rest still connect. Malformed entries (missing name, config that
    /// does not parse) are dropped the same way.
    ///
    /// Re-registering (a `session/load` re-sending `mcpServers`, the v1
    /// contract: the overlay is never persisted) disconnects the replaced
    /// manager — its child processes go with it.
    fn prepare_session_overlay(&self, session_id: &str, servers: Vec<Value>) {
        // Parse the raw entries, warn-dropping malformed ones and keeping
        // the last write per name (last-write-wins, mcp.ts parity).
        let mut parsed: Vec<(String, mirri_mcp::McpServerConfig)> = Vec::new();
        for raw in servers {
            let Some(name) = raw.get("name").and_then(Value::as_str) else {
                tracing::warn!("session overlay MCP entry without a name, dropped: {raw}");
                continue;
            };
            let config_value = raw.get("config").cloned().unwrap_or(Value::Null);
            match serde_json::from_value::<mirri_mcp::McpServerConfig>(config_value) {
                Ok(config) => match parsed.iter_mut().find(|(existing, _)| existing == name) {
                    Some((_, existing_config)) => *existing_config = config,
                    None => parsed.push((name.to_string(), config)),
                },
                Err(error) => {
                    tracing::warn!(server = %name, %error, "session overlay MCP config invalid, dropped");
                }
            }
        }

        let manager = Arc::new(mirri_mcp::McpConnectionManager::new());
        // Bridge the overlay's connection status transitions onto the
        // session's event bus (the `mcp.server.status` wire event): every
        // notify_listeners call (pending/connected/failed transitions)
        // becomes one event carrying the TS `McpServerStatusPayload`
        // shape. The transport strings ride a name-keyed map captured from
        // the parsed entries (the listener signature carries only the state).
        {
            let transports: HashMap<String, String> = parsed
                .iter()
                .map(|(name, config)| {
                    let transport = match &config.transport {
                        mirri_mcp::McpTransportConfig::Stdio { .. } => "stdio",
                        mirri_mcp::McpTransportConfig::Http { .. } => "http",
                        mirri_mcp::McpTransportConfig::Sse { .. } => "sse",
                    };
                    (name.clone(), transport.to_string())
                })
                .collect();
            let registry = Arc::clone(&self.registry);
            let sid = session_id.to_string();
            manager.add_status_listener(Box::new(
                move |name: String, state: mirri_mcp::McpConnectionState| {
                    // The artifacts resolve through the routing seam (W3) so
                    // a non-default workspace's session hears its own
                    // overlay's transitions.
                    let Ok(artifacts) = session_routing::resolve_artifacts_via(&registry, &sid)
                    else {
                        return;
                    };
                    let (status, tool_count, error) = match &state {
                        mirri_mcp::McpConnectionState::Pending => ("pending", 0, None),
                        mirri_mcp::McpConnectionState::Connected { tools, .. } => {
                            ("connected", tools.len() as u32, None)
                        }
                        mirri_mcp::McpConnectionState::Failed { error } => {
                            ("failed", 0, Some(error.clone()))
                        }
                        mirri_mcp::McpConnectionState::Disabled => ("disabled", 0, None),
                        mirri_mcp::McpConnectionState::NeedsAuth => ("needs-auth", 0, None),
                    };
                    artifacts
                        .event_bus
                        .publish(mirri_core::SessionEvent::McpServerStatus {
                            name: name.clone(),
                            transport: transports.get(&name).cloned().unwrap_or_default(),
                            status: status.to_string(),
                            tool_count,
                            error,
                            // The overlay is the session-scoped fourth
                            // source (the D6 ruling) — its events name
                            // their pool as `session` and carry no
                            // change kind (no persisted change is ever
                            // involved; the overlay is never on disk).
                            pool: "session".to_string(),
                            change_kind: None,
                        });
                },
            ));
        }
        let overlay = SessionOverlay {
            manager: Arc::clone(&manager),
            names: parsed.iter().map(|(name, _)| name.clone()).collect(),
        };
        if let Some(replaced) = self
            .session_overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), overlay)
        {
            // The overlay is being re-registered (session/load): the
            // replaced manager's connections are torn down in the
            // background so the swap never blocks the request.
            tokio::spawn(async move {
                replaced.manager.disconnect_all().await;
            });
        }
        if parsed.is_empty() {
            return;
        }

        let registry = Arc::clone(&self.registry);
        let overlays = Arc::clone(&self.session_overlays);
        let sid = session_id.to_string();
        let connect_manager = Arc::clone(&manager);
        tokio::spawn(async move {
            // Connect every server independently; a failure warns and
            // drops that server without failing the rest (the rest of the
            // session/new / session/load request already succeeded).
            let connects = parsed.into_iter().map(|(name, config)| {
                let manager = Arc::clone(&connect_manager);
                async move {
                    if let Err(error) = manager.connect_one(&name, config).await {
                        tracing::warn!(server = %name, %error, "session overlay MCP server failed to connect, dropped");
                    }
                }
            });
            futures::future::join_all(connects).await;
            // A re-registration or unregistration may have replaced this
            // overlay while the connects ran — only the current entry may
            // touch the session's tools.
            let still_current = overlays
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&sid)
                .is_some_and(|current| Arc::ptr_eq(&current.manager, &connect_manager));
            if still_current {
                register_overlay_tools_to_session(&connect_manager, &registry, &sid).await;
            }
        });
    }

    /// Tear the session's MCP overlay down: the table entry leaves and its
    /// connection manager disconnects every server (the child processes
    /// go with it). Called from `session.close` and `session.archive` —
    /// the overlay dies with the session.
    async fn clear_session_overlay(&self, session_id: &str) {
        // Bound before the block: the map's lock guard must drop before
        // the disconnect await (the future must stay Send).
        let overlay = self
            .session_overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(overlay) = overlay {
            overlay.manager.disconnect_all().await;
        }
    }

    /// The live overlay connection rows for a session — `(server name,
    /// state)` pairs of the session's overlay manager; empty when the
    /// session has no overlay (never registered, or torn down at
    /// close/archive). An inspection surface for hosts and tests: the
    /// `mcp.list` / `mcp.status` views deliberately exclude overlays (they
    /// are session-scoped, not pool entries).
    pub async fn session_overlay_connections(
        &self,
        session_id: &str,
    ) -> Vec<(String, mirri_mcp::McpConnectionState)> {
        // Bound before the match: the map's lock guard must drop before
        // the await inside (the future must stay Send).
        let overlay = self
            .session_overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned();
        match overlay {
            Some(overlay) => overlay.manager.list_connections().await,
            None => Vec::new(),
        }
    }

    // --- MCP RPC dispatch helpers ---------------------------------------

    /// `mcp.list` / `mcp.global.list` — return all registered MCP servers
    /// in the TS `McpServerInfo[]` wire shape (bare array), merged across
    /// the workspace and user pools (the effective view, spec §4.2): a
    /// same-name workspace entry shadows the user entry. Each entry also
    /// carries its pool (`user` / `workspace` / `plugin`) and config
    /// `source` layer.
    async fn dispatch_mcp_list(&self, _global: bool) -> Result<Value, EngineError> {
        Ok(self.mcp_pool_view(true).await)
    }

    /// `mcp.startupMetrics` — the measured wall-clock span of this
    /// engine's startup connect set (the TS
    /// `McpConnectionManager.initialLoadDurationMs` counterpart): from
    /// the first spawned connect task until the last settles. While a
    /// task is still in flight the elapsed-so-far is reported; an engine
    /// with no startup connect set reports 0.
    async fn dispatch_mcp_startup_metrics(&self) -> Result<Value, EngineError> {
        let timing = self.mcp_startup.lock().unwrap_or_else(|e| e.into_inner());
        to_result(McpStartupMetricsResult {
            duration_ms: timing.duration_ms(),
        })
    }

    /// `mcp.status` — the no-workspace-context global view (spec §4.4):
    /// every user-pool entry plus this engine's workspace-pool entries,
    /// each with `pool` / `source` provenance. Unlike `mcp.list` there is
    /// no shadowing — the app-level config page sees every entry in every
    /// pool, so a name defined in both the user layer and a project layer
    /// appears twice (one entry per pool).
    async fn dispatch_mcp_status(&self) -> Result<Value, EngineError> {
        Ok(self.mcp_pool_view(false).await)
    }

    /// The merged pool view for the running-server reads: workspace-pool
    /// entries first, then user-pool entries. `shadow_user` filters
    /// same-name user entries out (`mcp.list`, the effective view of spec
    /// §4.2); `false` keeps every entry (`mcp.status`, the app-level
    /// global view of spec §4.4).
    async fn mcp_pool_view(&self, shadow_user: bool) -> Value {
        let mut servers = Vec::new();
        let mut workspace_names = std::collections::HashSet::new();

        // Workspace pool first — its names shadow same-name user entries.
        if let Some(manager) = &self.mcp_manager {
            for (name, state) in manager.list_connections().await {
                workspace_names.insert(name.clone());
                // Plugin-contributed servers run under `plugin-<id>:<name>`
                // (the TS engine's convention); everything else in this
                // pool comes from the project config layers.
                let is_plugin = name.starts_with("plugin-");
                servers.push(
                    mcp_list_entry(
                        manager,
                        name,
                        state,
                        if is_plugin { "plugin" } else { "workspace" },
                        if is_plugin { "plugin" } else { "project" },
                    )
                    .await,
                );
            }
        }

        if let Some(pool) = &self.user_mcp_pool {
            let manager = pool.manager();
            for (name, state) in manager.list_connections().await {
                if shadow_user && workspace_names.contains(&name) {
                    continue; // shadowed by the workspace entry
                }
                servers.push(mcp_list_entry(&manager, name, state, "user", "user").await);
            }
        }

        Value::Array(servers)
    }

    /// `mcp.global.tools` / `listGlobalMcpTools` — return all tools from
    /// connected MCP servers in the TS `GlobalMcpToolInfo[]` wire shape
    /// (bare array, original tool names — the agent tool bridge applies the
    /// `mcp__server__tool` qualification separately), merged across the
    /// workspace and user pools (same-name workspace entries shadow user
    /// entries, the effective view). The enumeration goes through
    /// [`mcp_tool_summaries`] and drops tools the connection's stored
    /// filter excludes — the same `list_all_tools` semantics, one shared
    /// enumeration.
    async fn dispatch_mcp_tools(&self) -> Result<Value, EngineError> {
        let mut tools: Vec<Value> = Vec::new();
        let mut workspace_names = std::collections::HashSet::new();

        let workspace = self.mcp_manager.as_ref();
        let user = self.user_mcp_pool.as_ref().map(|pool| pool.manager());
        for (manager, is_workspace) in [(workspace, true), (user.as_ref(), false)] {
            let Some(manager) = manager else { continue };
            for (server, _state) in manager.list_connections().await {
                if !is_workspace && workspace_names.contains(&server) {
                    continue; // shadowed by the workspace entry
                }
                let Some(sums) = mcp_tool_summaries(manager, &server).await else {
                    continue; // not connected in this pool
                };
                if is_workspace {
                    workspace_names.insert(server.clone());
                }
                let filter = manager.get_server_config(&server).await;
                for (tool, description) in sums {
                    if filter.as_ref().is_some_and(|c| c.is_tool_allowed(&tool)) {
                        tools.push(json!({
                            "name": tool,
                            // the retired wire keeps the connection
                            // layer's plain-string description (no
                            // optional-key mapping here)
                            "description": description.unwrap_or_default(),
                            "mcpServerId": server,
                        }));
                    }
                }
            }
        }
        Ok(Value::Array(tools))
    }

    /// `mcp.global.connect` / `mcp.global.create` — connect to a single MCP
    /// server from a config object passed in the params.
    async fn dispatch_mcp_connect(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.mcp_manager else {
            return to_result(McpConnectResult {
                server: None,
                tools: Vec::new(),
            });
        };

        let params = normalize_params(params);
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("name is required for mcp.connect"))?
            .to_string();

        // Build McpServerConfig from the params object. The shape mirrors
        // what `mcp.json` uses: { command, args, env } for stdio or
        // { url } for SSE.
        let config_value = params
            .get("config")
            .or_else(|| params.get("serverConfig"))
            .cloned()
            .unwrap_or(params.clone());

        let config: mirri_mcp::McpServerConfig =
            serde_json::from_value(config_value).map_err(|e| invalid_params(e.to_string()))?;

        let result = manager.connect_one(&name, config).await;
        let tools = manager.list_all_tools().await;
        let tool_list: Vec<Value> = tools
            .iter()
            .filter(|(srv, _)| srv == &name)
            .map(|(server, def)| {
                let qualified = mirri_mcp::qualify_tool_name(server, &def.name);
                json!({
                    "name": qualified,
                    "description": def.description,
                    "inputSchema": def.input_schema,
                })
            })
            .collect();

        match result {
            Ok(()) => to_result(McpConnectResult {
                server: Some(McpServerState {
                    name,
                    status: "connected".to_string(),
                    error: None,
                }),
                tools: tool_list,
            }),
            Err(e) => to_result(McpConnectResult {
                server: Some(McpServerState {
                    name,
                    status: "failed".to_string(),
                    error: Some(e.to_string()),
                }),
                tools: Vec::new(),
            }),
        }
    }

    /// `mcp.global.delete` — retired in the v2 contract (v1-era global MCP
    /// surface; v2 has no global MCP RPC). Answers with an explicit
    /// `not_implemented` error instead of the former silent acknowledgement;
    /// the dispatch arm stays until the retirement batch removes it.
    async fn dispatch_mcp_disconnect(&self, _params: Value) -> Result<Value, EngineError> {
        Err(not_implemented(
            "mcp.global.delete is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
        ))
    }

    /// The pool that owns the given server name (the workspace pool first —
    /// same-name workspace entries shadow user entries, so they also own
    /// the name), when either pool holds it.
    async fn owning_pool_manager(
        &self,
        name: &str,
    ) -> Option<Arc<mirri_mcp::McpConnectionManager>> {
        if let Some(manager) = &self.mcp_manager
            && manager.get_server_config(name).await.is_some()
        {
            return Some(Arc::clone(manager));
        }
        if let Some(pool) = &self.user_mcp_pool {
            let manager = pool.manager();
            if manager.get_server_config(name).await.is_some() {
                return Some(manager);
            }
        }
        None
    }

    /// `mcp.reconnect` — reconnect a specific MCP server, routed to the
    /// pool that owns it (workspace first, then the user pool —
    /// same-name workspace entries shadow user entries and own the name).
    ///
    /// Real when either pool is configured; an explicit `not_implemented`
    /// error replaces the former silent acknowledgement when the embedded
    /// engine has no MCP servers to reconnect.
    async fn dispatch_mcp_reconnect(&self, params: Value) -> Result<Value, EngineError> {
        // The no-MCP check precedes name parsing (the pinned wire surface:
        // an engine without MCP answers not_implemented whatever the params).
        if self.mcp_manager.is_none() && self.user_mcp_pool.is_none() {
            return Err(not_implemented(
                "mcp.reconnect is unavailable: no MCP servers are configured in this embedded engine",
            ));
        }

        let params = normalize_params(params);
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("name is required for mcp.reconnect"))?;

        // An unknown name falls through to the workspace manager (the
        // historical surface: its reconnect error reaches the caller).
        let manager = self
            .owning_pool_manager(name)
            .await
            .or_else(|| self.mcp_manager.as_ref().map(Arc::clone));
        let Some(manager) = manager else {
            return Err(not_implemented(
                "mcp.reconnect is unavailable: no MCP servers are configured in this embedded engine",
            ));
        };

        manager
            .reconnect(name)
            .await
            .map_err(|e| EngineError::Internal(format!("MCP reconnect error: {e}")))?;
        ack()
    }

    /// `mcp.global.reload` — retired in the v2 contract (v1-era global MCP
    /// surface; v2 has no global MCP RPC). Answers with an explicit
    /// `not_implemented` error instead of the former silent acknowledgement
    /// (and the former real reconcile path, which kept the retired method
    /// alive off-contract); the dispatch arm stays until the retirement
    /// batch removes it.
    async fn dispatch_mcp_reload(&self, _params: Value) -> Result<Value, EngineError> {
        Err(not_implemented(
            "mcp.global.reload is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
        ))
    }

    /// `mcp.global.testConnect` — test connectivity to an MCP server,
    /// probing whichever pool owns the name (workspace first, then the
    /// user pool).
    async fn dispatch_mcp_test_connect(&self, params: Value) -> Result<Value, EngineError> {
        let params = normalize_params(params);
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("name is required for mcp.testConnect"))?
            .to_string();

        let manager = self.owning_pool_manager(&name).await;
        let Some(manager) = &manager else {
            return to_result(McpTestConnectResult {
                status: "error".to_string(),
                tool_count: 0,
                error: Some(format!("server '{name}' is not connected")),
            });
        };

        let connections = manager.list_connections().await;
        let connected = connections.iter().any(|(n, s)| {
            n == &name && matches!(s, mirri_mcp::McpConnectionState::Connected { .. })
        });

        if connected {
            let tools = manager.list_all_tools().await;
            let count = tools.iter().filter(|(n, _)| *n == name).count();
            to_result(McpTestConnectResult {
                status: "ok".to_string(),
                tool_count: count,
                error: None,
            })
        } else {
            to_result(McpTestConnectResult {
                status: "error".to_string(),
                tool_count: 0,
                error: Some(format!("server '{name}' is not connected")),
            })
        }
    }

    /// `mcp.toggleState` / `mcp.global.toggleState` — return the disabled
    /// servers and disabled tools across all configured MCP servers.
    ///
    /// `disabledServers` lists the names of servers whose connection state
    /// is `Disabled`; `disabledTools` lists the per-server tool denylist
    /// entries, formatted as `"serverName/toolName"`. The global MCP manager
    /// is the same manager for now, so both routes share this dispatch.
    async fn dispatch_mcp_toggle_state(&self) -> Result<Value, EngineError> {
        let Some(manager) = &self.mcp_manager else {
            return to_result(McpToggleStateResult {
                disabled_servers: Vec::new(),
                disabled_tools: Vec::new(),
            });
        };

        let mut disabled_servers: Vec<String> = Vec::new();
        let mut disabled_tools: Vec<String> = Vec::new();

        for (name, state) in manager.list_connections().await {
            if matches!(state, mirri_mcp::McpConnectionState::Disabled) {
                disabled_servers.push(name.clone());
            }
            if let Some(config) = manager.get_server_config(&name).await
                && let Some(tools) = &config.disabled_tools
            {
                for tool in tools {
                    disabled_tools.push(format!("{name}/{tool}"));
                }
            }
        }

        to_result(McpToggleStateResult {
            disabled_servers,
            disabled_tools,
        })
    }

    /// `mcp.enable` — retired in the v2 contract (v1-era session-level toggle;
    /// v2 deleted it). Answers with an explicit `not_implemented` error
    /// instead of the former silent acknowledgement (and the former real
    /// enable path, which kept the retired method alive off-contract); the
    /// dispatch arm stays until the retirement batch removes it.
    async fn dispatch_mcp_enable(&self, _params: Value) -> Result<Value, EngineError> {
        Err(not_implemented(
            "mcp.enable is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
        ))
    }

    /// `mcp.disable` — retired in the v2 contract (v1-era session-level
    /// toggle; v2 deleted it). Answers with an explicit `not_implemented`
    /// error instead of the former silent acknowledgement (and the former
    /// real disable path, which kept the retired method alive off-contract);
    /// the dispatch arm stays until the retirement batch removes it.
    async fn dispatch_mcp_disable(&self, _params: Value) -> Result<Value, EngineError> {
        Err(not_implemented(
            "mcp.disable is retired in the RPC contract; the embedded engine returns an explicit not_implemented error",
        ))
    }

    /// `mcp.enableTool` — retired in the v2 contract (session-level route; the
    /// dispatch arm returns an explicit error). Still serves the
    /// out-of-scope `mcp.global.enableTool` route below; the no-manager
    /// fallback answers with an explicit `not_implemented` error instead of
    /// the former silent acknowledgement.
    async fn dispatch_mcp_enable_tool(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.mcp_manager else {
            return Err(not_implemented(
                "mcp.enableTool is unavailable: no MCP servers are configured in this embedded engine",
            ));
        };

        let params = normalize_params(params);
        let server = params
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("server is required for mcp.enableTool"))?;
        let tool = params
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("tool is required for mcp.enableTool"))?;

        manager
            .enable_tool(server, tool)
            .await
            .map_err(|e| EngineError::Internal(format!("MCP enable tool error: {e}")))?;
        ack()
    }

    /// `mcp.disableTool` — retired in the v2 contract (session-level route; the
    /// dispatch arm returns an explicit error). Still serves the
    /// out-of-scope `mcp.global.disableTool` route below; the no-manager
    /// fallback answers with an explicit `not_implemented` error instead of
    /// the former silent acknowledgement.
    async fn dispatch_mcp_disable_tool(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.mcp_manager else {
            return Err(not_implemented(
                "mcp.disableTool is unavailable: no MCP servers are configured in this embedded engine",
            ));
        };

        let params = normalize_params(params);
        let server = params
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("server is required for mcp.disableTool"))?;
        let tool = params
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("tool is required for mcp.disableTool"))?;

        manager
            .disable_tool(server, tool)
            .await
            .map_err(|e| EngineError::Internal(format!("MCP disable tool error: {e}")))?;
        ack()
    }

    // --- Plugin RPC dispatch helpers -------------------------------------
    //
    // `plugin_error_to_engine` lives at module level with the other RPC
    // error-mapping helpers (see above); the wire rows (N1/N2/N3) serialize
    // directly from `mirri_plugin_host::PluginSummary` /
    // `PluginInfoView`.

    /// `plugin.list` / `listPlugins` — list installed plugins with their
    /// status, in the TS `PluginSummary[]` wire shape (bare array). Accepts
    /// an optional `status` filter (`"active"`/`"inactive"`).
    async fn dispatch_plugin_list(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Ok(Value::Array(Vec::new()));
        };

        let params = normalize_params(params);
        // Rust superset (D-N1): the TS contract takes no input, but the
        // historical `status` filter (`"active"`/`"inactive"`) is kept as
        // an optional parameter — klient never sends it, so the TS surface
        // is untouched.
        let status_filter = params.get("status").and_then(|v| v.as_str());
        let plugins: Vec<Value> = manager
            .summaries()
            .into_iter()
            .filter(|summary| match status_filter {
                Some("active") => summary.enabled,
                Some("inactive") => !summary.enabled,
                _ => true,
            })
            .filter_map(|summary| serde_json::to_value(summary).ok())
            .collect();
        tracing::trace!(count = plugins.len(), "plugin.list");
        Ok(Value::Array(plugins))
    }

    /// `plugin.install` / `installPlugin` — install a plugin from a source
    /// string (a `local:` path, a bare absolute path, a `github:` spec, a
    /// `github.com/...` URL, or an HTTP(S) `.zip` URL — A1) and return the
    /// full klient `pluginSummarySchema` row of the installed plugin (N2,
    /// the TS `installPlugin` success body).
    async fn dispatch_plugin_install(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let source = params
            .get("source")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("source is required for plugin.install"))?;

        let id = manager
            .install(source)
            .await
            .map_err(plugin_error_to_engine)?;
        tracing::trace!(plugin = %id, "plugin installed");
        let summary = manager
            .summary(&id)
            .ok_or_else(|| EngineError::Internal(format!("installed plugin not found: {id}")))?;
        serde_json::to_value(summary).map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `plugin.setEnabled` / `setPluginEnabled` — enable or disable an
    /// installed plugin by id.
    async fn dispatch_plugin_set_enabled(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("id is required"))?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| invalid_params("enabled is required"))?;

        if enabled {
            manager.enable(id).map_err(plugin_error_to_engine)?;
        } else {
            manager.disable(id).map_err(plugin_error_to_engine)?;
        }
        tracing::trace!(plugin = %id, enabled, "plugin enabled state set");
        ack()
    }

    /// `plugin.setMcpServerEnabled` / `setPluginMcpServerEnabled` — set one
    /// plugin-contributed MCP server's enablement (the plugin user state,
    /// A1): the write persists through the plugin store and the live
    /// connection set follows (enabled → connect under the runtime name,
    /// disabled → unregister).
    async fn dispatch_plugin_set_mcp_server_enabled(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("id is required"))?;
        let server = params
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("server is required"))?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| invalid_params("enabled is required"))?;

        let runtime_name = manager
            .set_mcp_server_enabled(id, server, enabled)
            .map_err(plugin_error_to_engine)?;
        tracing::trace!(plugin = %id, server = %server, enabled, "plugin MCP server enabled state set");
        self.sync_plugin_mcp_connection(&runtime_name).await;
        ack()
    }

    /// `plugin.remove` / `removePlugin` — remove an installed plugin by id.
    async fn dispatch_plugin_remove(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("id is required"))?;

        manager.remove(id).map_err(plugin_error_to_engine)?;
        tracing::trace!(plugin = %id, "plugin removed");
        ack()
    }

    /// `plugin.reload` / `reloadPlugins` — reload a single plugin (when
    /// `pluginId` is given) or all installed plugins, then return the TS
    /// `ReloadSummary` shape (`{added, removed, errors}`). The full reload
    /// mirrors the TS `PluginManager.reload` (item C3): `added`/`removed`
    /// are the installed.json record id diff against the previously loaded
    /// set, error-state plugins (missing/broken manifest) stay in the list
    /// with `state: 'error'` and land in neither bucket, and `errors`
    /// collects per-record materialization failures (currently unreachable —
    /// every rust materialize step is tolerant, matching the TS try/catch
    /// scope). The `pluginId` form is the rust superset: a successful
    /// single reload answers an empty summary.
    async fn dispatch_plugin_reload(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let summary = match params.get("pluginId").and_then(|v| v.as_str()) {
            Some(id) => {
                manager.reload(id).map_err(plugin_error_to_engine)?;
                mirri_plugin_host::ReloadSummary::default()
            }
            None => manager.reload_all().map_err(plugin_error_to_engine)?,
        };

        // Item C4 — refresh every consumer of the plugin manager (skill
        // catalogs, hook engines, agent profiles, and the live MCP
        // connections) so reloaded plugins take effect for new sessions.
        // Existing sessions keep their assembly-time values (see the
        // parity checklist I row).
        self.refresh_plugin_consumers().await;
        tracing::trace!(
            added = summary.added.len(),
            removed = summary.removed.len(),
            errors = summary.errors.len(),
            "plugin.reload"
        );
        to_result(ReloadSummaryResult {
            added: summary.added,
            removed: summary.removed,
            errors: summary
                .errors
                .into_iter()
                .map(|entry| serde_json::json!({ "id": entry.id, "message": entry.message }))
                .collect(),
        })
    }

    /// `plugin.info` / `getPluginInfo` — return detailed info for one plugin
    /// by id, in the klient `pluginInfoSchema` shape (N3/L): the summary
    /// super-set plus `root`, `installedAt`, `updatedAt`, `manifestKind`,
    /// `manifestPath`, the wire `manifest`, per-server `mcpServers`
    /// (`PluginMcpServerInfo`), `shadowedManifestPath`, and `diagnostics`.
    async fn dispatch_plugin_info(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("id is required"))?;

        let info = manager
            .info(id)
            .ok_or_else(|| not_found(format!("plugin not found: {id}")))?;
        serde_json::to_value(info).map_err(|e| EngineError::Internal(e.to_string()))
    }

    /// `plugin.commands` / `listPluginCommands` — list all slash commands
    /// contributed by enabled plugins, in the TS `PluginCommandDef[]` wire
    /// shape (bare array): markdown-file commands (frontmatter
    /// `name`/`description`, `body` the trimmed md body, `path` the file's
    /// absolute path) and the Rust-only inline specs mapped onto the same
    /// shape with `path: ""` (B9/N4, the TS `enabledCommands` model).
    async fn dispatch_plugin_commands(&self) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Ok(Value::Array(Vec::new()));
        };

        let commands: Vec<Value> = manager
            .enabled_commands()
            .into_iter()
            .map(|def| serde_json::to_value(def).unwrap_or(Value::Null))
            .collect();
        tracing::trace!(count = commands.len(), "plugin.commands");
        Ok(Value::Array(commands))
    }

    /// `plugin.activateCommand` / `activatePluginCommand` — run a slash
    /// command with the given argument string. Both command sources work:
    /// markdown-file commands expand with the TS `$ARGUMENTS` semantics
    /// (placeholder replacement, else a trailing `ARGUMENTS:` block), and
    /// inline specs render their minijinja template (B9/F).
    async fn dispatch_plugin_activate_command(&self, params: Value) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };

        let params = normalize_params(params);
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("name is required for plugin.activateCommand"))?;
        let args = params.get("args").and_then(|v| v.as_str()).unwrap_or("");
        // `pluginId` is accepted for API compatibility but ignored: the
        // manager resolves commands by name across enabled plugins.
        let _plugin_id = params.get("pluginId").and_then(|v| v.as_str());

        let output = manager
            .activate_command(name, args)
            .map_err(plugin_error_to_engine)?;
        tracing::trace!(command = %name, "plugin.activateCommand");
        to_result(CommandOutputResult { output })
    }

    /// `plugin.checkUpdates` / `checkUpdates` — check every installed
    /// github-sourced plugin for updates, in the klient
    /// `pluginUpdateStatusSchema[]` wire shape (bare array, sorted by id).
    /// A per-plugin resolution failure drops just that row (the TS
    /// semantics); without a plugin manager there is nothing to check and
    /// the answer is an empty array.
    async fn dispatch_plugin_check_updates(&self) -> Result<Value, EngineError> {
        let Some(manager) = &self.plugin_manager else {
            return Ok(Value::Array(Vec::new()));
        };

        let statuses: Vec<responses::PluginUpdateStatusEntry> = manager
            .check_updates()
            .await
            .into_iter()
            .map(responses::PluginUpdateStatusEntry::from)
            .collect();
        tracing::trace!(count = statuses.len(), "plugin.checkUpdates");
        to_result(statuses)
    }

    /// `agent.listCommands` — the commands available to the addressed
    /// agent, in the klient `AgentCommandInfo[]` wire shape (`{ name,
    /// description?, source }`). Commands come from the plugin manager's
    /// command registry; `source` is the contributing plugin's id (the
    /// real provenance, not a placeholder). Without a plugin manager the
    /// engine has no commands and answers an empty array.
    async fn dispatch_agent_list_commands(&self, params: Value) -> Result<Value, EngineError> {
        let ref_params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        // Resolving the agent keeps the agent-scope addressing contract
        // (unknown session/agent answers not found) even though the
        // command catalog is engine-global today.
        self.resolve_agent(&ref_params).await?;
        let commands: Vec<responses::AgentCommandInfoResult> = match &self.plugin_manager {
            Some(manager) => manager
                .command_registry()
                .list()
                .into_iter()
                .map(|registered| responses::AgentCommandInfoResult {
                    name: registered.spec.name,
                    description: registered.spec.description,
                    source: registered.plugin,
                })
                .collect(),
            None => Vec::new(),
        };
        tracing::trace!(count = commands.len(), "agent.listCommands");
        to_result(commands)
    }

    /// `agent.activity` — the addressed agent's activity snapshot in the
    /// klient `agentActivityStateSchema` wire shape. The state is folded
    /// live by the engine's [`ActivityTracker`] (turn start/step/phase/tool
    /// window/approval wait/retry/lastTurn) and read here; a mid-turn read
    /// is a real in-flight observation (the tracker has its own mutex, so
    /// the read never waits for the running turn).
    async fn dispatch_agent_activity(&self, params: Value) -> Result<Value, EngineError> {
        let ref_params: AgentRefParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let agent = self.resolve_agent(&ref_params).await?;
        let agent_id = agent.id().as_str().to_string();
        let model_id = agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone();
        let state: AgentActivityState = self
            .activity_tracker_for(&ref_params.session_id, &agent_id, &model_id)
            .snapshot();
        tracing::trace!(session_id = %ref_params.session_id, agent_id, "agent.activity");
        to_result(responses::AgentActivityResult::from_state(&state))
    }

    /// `agent.runCommand` — run a command by name with an optional argument
    /// string. The klient contract models the method as `noResult`
    /// (JSON `null`); an unknown name errors through
    /// [`plugin_error_to_engine`]'s `CommandNotFound` branch (-32001),
    /// the same mapping `plugin.activateCommand` uses.
    async fn dispatch_agent_run_command(&self, params: Value) -> Result<Value, EngineError> {
        #[derive(Debug, Deserialize)]
        struct RunCommandParams {
            #[serde(alias = "sessionId")]
            session_id: String,
            #[serde(default, alias = "agentId")]
            agent_id: Option<String>,
            name: String,
            #[serde(default)]
            args: Option<String>,
        }
        let params: RunCommandParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| invalid_params(e.to_string()))?;
        let ref_params = AgentRefParams {
            session_id: params.session_id,
            agent_id: params.agent_id,
        };
        self.resolve_agent(&ref_params).await?;
        let Some(manager) = &self.plugin_manager else {
            return Err(EngineError::Internal(
                "plugin support not configured".to_string(),
            ));
        };
        let args = params.args.as_deref().unwrap_or("");
        manager
            .command_registry()
            .execute(&params.name, args)
            .map_err(plugin_error_to_engine)?;
        tracing::trace!(command = %params.name, "agent.runCommand");
        Ok(Value::Null)
    }

    // --- shared helpers ----------------------------------------------------

    /// Resolve the agent addressed by [`AgentRefParams`], returning a
    /// `not found` error when the session or agent is missing. The session
    /// resolves through the W3 routing seam (agent ids resolve within a
    /// session, so the session resolution upstream carries the agent).
    async fn resolve_agent(&self, params: &AgentRefParams) -> Result<Arc<AgentScope>, EngineError> {
        let session = self.resolve_session(&params.session_id).await?;
        resolve_session_agent(&session, params.agent_id.as_deref())
            .await
            .ok_or_else(|| match &params.agent_id {
                Some(agent_id) => not_found(format!("agent not found: {agent_id}")),
                None => not_found(format!("session '{}' has no agents yet", params.session_id)),
            })
    }

    /// Record a `WireOp::ConfigUpdate` model-alias change on the session's
    /// wire log so a resume replays it back into the agent state. Shared by
    /// the `agent.setModel` and `session.updateMeta` model paths.
    async fn record_session_config_op(
        &self,
        session_id: &str,
        model_alias: String,
    ) -> Result<(), EngineError> {
        let session = self.resolve_session(session_id).await?;
        session
            .record_op(WireOp::ConfigUpdate {
                model_alias: Some(model_alias),
                profile_name: None,
                thinking_effort: None,
                system_prompt: None,
                disallowed_tools: None,
            })
            .await;
        Ok(())
    }

    /// Push a model switch's context window into the agent's existing
    /// turn flow (the flow was built with the previous model's window).
    pub(crate) fn update_turn_flow_model_window(
        &self,
        session_id: &str,
        agent_id: &str,
        model_id: &str,
    ) {
        let key = format!("{session_id}:{agent_id}");
        // Brief map lock only (the same invariant `get_or_create_turn_flow`
        // upholds): clone the flow's shared compaction Arc out of the side
        // map and drop the guard, then update the window through it. The
        // turn-long TurnFlow mutex is never taken here — a running turn
        // holds it for the whole turn, and the window update only touches
        // the compaction mutex in a short critical section.
        let compaction = self
            .compaction_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(compaction) = compaction {
            let window = self.max_context_window(model_id);
            compaction
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_max_context_tokens(window);
        }
    }

    /// Push a model switch's ALIAS into the agent's existing turn flow's
    /// usage-model key — the `by_model` attribution key the flow's
    /// step-usage recording closure reads (spec 11 A13): the usage
    /// recorded after the switch point is attributed to the new model.
    /// The turn-long `TurnFlow` mutex is never taken here (a running turn
    /// holds it for the whole turn); the handle update only touches the
    /// shared key mutex in a short critical section.
    pub(crate) fn update_turn_flow_usage_model_key(
        &self,
        session_id: &str,
        agent_id: &str,
        model_id: &str,
    ) {
        let key = format!("{session_id}:{agent_id}");
        let usage_model_key = self
            .usage_model_key_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(usage_model_key) = usage_model_key {
            *usage_model_key.lock().unwrap_or_else(|e| e.into_inner()) = Some(model_id.to_string());
        }
    }

    /// The max context window for `model_id` from the model catalog
    /// (0 = unknown — ratio triggers stay off until an overflow teaches
    /// the engine the real limit).
    pub(crate) fn max_context_window(&self, model_id: &str) -> u64 {
        max_context_window_of(&self.config, model_id)
    }

    /// The turn-flow config for `model_id` (loop-control limits + the
    /// model's context window). The reads are strictly sequential —
    /// `max_context_window`'s catalog read settles (and drops its guard)
    /// before the loop-control snapshot read opens, so no config-reload
    /// writer queued between them can deadlock the reader-writer way.
    pub(crate) fn turn_flow_config_for(&self, model_id: &str) -> TurnFlowConfig {
        let window = self.max_context_window(model_id);
        let server_config = self.config.read().unwrap_or_else(|e| e.into_inner());
        turn_flow_config_from(&server_config, window)
    }

    /// Get-or-create the addressed agent's turn flow (brief map lock only;
    /// the inner mutex serializes the agent's turns). Keyed by
    /// `"{session_id}:{agent_id}"` so sessions with the same default agent id
    /// (`agent-main`) don't share a flow — cross-session steer leakage and
    /// head-of-line blocking would result.
    ///
    /// The agent's [`ActivityTracker`] (the `agent.activity` fold) is
    /// registered in the parallel `activity_trackers` map at creation time,
    /// so the `agent.activity` handler can read the fold without locking the
    /// `TurnFlow` (a running turn holds it for the whole turn).
    fn get_or_create_turn_flow(
        &self,
        session_id: &str,
        agent_id: &str,
        model_id: &str,
    ) -> Arc<std::sync::Mutex<TurnFlow>> {
        let key = format!("{session_id}:{agent_id}");
        let mut flows = self.turn_flows.lock().unwrap_or_else(|e| e.into_inner());
        if !flows.contains_key(&key) {
            let mut turn_flow = TurnFlow::new(self.turn_flow_config_for(model_id));
            // Wire the host's model catalog (the same optional-service shape
            // as the title injector) so the capability-gap injector's
            // profile name-pointing resolves real model capabilities: the
            // catalog is built from the shared server config Arc, the same
            // handle `max_context_window_of` reads. Standalone flows (task /
            // cron / subagent hosts) keep the conservative `None`.
            turn_flow.set_model_catalog(Some(Arc::new(
                crate::model_catalog::ConfigModelCatalog::new(Arc::clone(&self.config)),
            )));
            // Wire the server-level profile manager (the same handle
            // `with_profile_manager` receives from bootstrap / NAPI) so the
            // capability-gap injector's profile name-pointing sees the
            // registered profiles on production turns — without this the
            // US5.2 injector stays dormant on the main path.
            if let Some(pm) = &self.profile_manager {
                turn_flow.set_profile_manager(Arc::clone(pm));
            }
            // Wire the session's event bus into the flow so the turn-internal
            // auto-compaction lifecycle publishes on it (the embedded layer
            // cannot observe a compaction happening inside a turn). The
            // artifacts resolve through the routing seam (W3) so a turn of a
            // non-default workspace's session wires its own bus. The same
            // artifacts carry the session's turn-id allocator — the flow
            // draws its `turn.ended` ids from it, so every flow of the
            // session (this one, cron/task turns, subagents) shares ONE
            // numeric namespace that survives flow disposal and resume.
            if let Ok(artifacts) = self.resolve_session_artifacts(session_id) {
                turn_flow.set_event_bus(Arc::clone(&artifacts.event_bus));
                turn_flow.set_turn_id_source(artifacts.turn_id_counter());
            }
            // Pin the flow's agent identity (the same string the
            // `"{session_id}:{agent_id}"` side-map keys use —
            // `AgentId::as_str()`, `"main"` for the main agent) so the
            // turn-internal auto-compaction lifecycle and approval asks
            // attribute to the addressed agent — the same id the dispatch
            // layer's compaction events carry. Standalone flows (task /
            // cron / subagent hosts) set their own identity (or none,
            // defaulting to "main" at the emit sites).
            turn_flow.set_agent_id(Some(agent_id.to_string()));
            let tracker = turn_flow.activity_tracker();
            self.activity_trackers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), tracker);
            // The shared steer buffer goes into the side map at flow creation
            // (the same handle the turn's hooks flush), so mid-turn steers
            // never have to lock the flow itself — the run loop holds that
            // mutex for the whole turn.
            let steer_buffer = turn_flow.steer_buffer_handle();
            self.steer_buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), steer_buffer);
            // The shared compaction orchestrator goes into the side map at
            // flow creation too (the same handle the activity fold shares),
            // so `setModel` / `updateMeta` window pushes never have to lock
            // the flow itself — the run loop holds that mutex for the whole
            // turn.
            let compaction_handle = turn_flow.compaction_handle();
            self.compaction_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), compaction_handle);
            // The shared usage-model-key handle goes into the side map at
            // flow creation too (the same handle the turn's step-usage
            // recording closure reads), so `setModel` / `updateMeta` usage
            // re-attribution pushes never have to lock the flow itself.
            let usage_model_key = turn_flow.usage_model_key_handle();
            self.usage_model_key_handles
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), usage_model_key);
            flows.insert(key.clone(), Arc::new(std::sync::Mutex::new(turn_flow)));
        }
        Arc::clone(&flows[&key])
    }

    /// Wire the structured agent-status publisher (CR4) onto the
    /// addressed agent's turn flow — the `event.agent.status.updated`
    /// emitter the flow's activity-tracker phase transitions drive.
    ///
    /// Called from the message dispatches (both the `message.send` path
    /// and the steer-launched idle path), where the `AgentScope` and the
    /// live session's artifacts are both in hand — the publisher reads
    /// the agent's live handles (config / usage / context memory), the
    /// flow's compaction window, and the session-scoped plan / swarm-mode
    /// services, so the wiring is an idempotent overwrite on every
    /// dispatch: a mid-turn model switch is picked up by the next frame
    /// without a re-wire.
    ///
    /// A session without resolvable artifacts (not live) skips the
    /// wiring — there is no bus to publish on. The flow lock is taken
    /// briefly, the same dispatch-time precedent `set_usage_sink` uses.
    fn attach_status_publisher(
        &self,
        session_id: &str,
        agent: &AgentScope,
        flow_handle: &Arc<std::sync::Mutex<TurnFlow>>,
    ) {
        let Ok(artifacts) = self.resolve_session_artifacts(session_id) else {
            return;
        };
        wire_agent_status_publisher(&artifacts.event_bus, agent, flow_handle);
    }

    /// The addressed agent's shared steer buffer, creating the agent's turn
    /// flow (and with it the buffer) when none exists yet. Parking a steer
    /// through this handle works while a turn of the agent is running —
    /// the buffer is the same one the turn's `before_step` /
    /// `should_continue_after_stop` hooks flush.
    fn steer_buffer_for(
        &self,
        session_id: &str,
        agent_id: &str,
        model_id: &str,
    ) -> Arc<std::sync::Mutex<SteerBuffer>> {
        // Ensure the flow (and with it the buffer entry) exists.
        let _ = self.get_or_create_turn_flow(session_id, agent_id, model_id);
        let key = format!("{session_id}:{agent_id}");
        self.steer_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            .expect("the steer buffer entry is created together with the flow")
    }

    /// The wire view of a live session scope owned by this server: the
    /// static metadata plus the runtime fields (busy / model / usage /
    /// messageCount / lastSeq — see [`build_session_view`]).
    async fn session_view(&self, session: &SessionScope) -> SessionView {
        build_session_view(self, session).await
    }

    /// The addressed agent's activity tracker, creating the agent's turn
    /// flow (and with it the tracker) when none exists yet. The returned
    /// fold is safe to snapshot while a turn of the same agent is running.
    ///
    /// When the flow already exists but its tracker entry is gone — the
    /// archive → restore path: `dispose_activity_trackers` removes the
    /// entry at archive time while the turn flow survives for the restore
    /// — the flow is given a **fresh** (ready, empty) fold and the map
    /// entry is re-registered, idempotently bringing the two back in sync.
    /// This lock of the flow can wait out an in-flight turn of that agent;
    /// the path only opens after an archive/restore, where a concurrent
    /// turn is pathological.
    fn activity_tracker_for(
        &self,
        session_id: &str,
        agent_id: &str,
        model_id: &str,
    ) -> Arc<ActivityTracker> {
        let key = format!("{session_id}:{agent_id}");
        let flow = self.get_or_create_turn_flow(session_id, agent_id, model_id);
        let mut trackers = self
            .activity_trackers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Arc::clone(trackers.entry(key).or_insert_with(|| {
            flow.lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset_activity_tracker()
        }))
    }

    /// Dispose the activity trackers of a closed (or archived) session's
    /// agents — the view's lifecycle flips to `disposed` and the entries
    /// leave the map. A later restore of the same session id re-registers
    /// a **fresh** (ready, empty) fold on the surviving turn flow (see
    /// [`Self::activity_tracker_for`]); the view owns no authoritative
    /// state and can be discarded and rebuilt.
    fn dispose_activity_trackers(&self, session_id: &str) {
        let prefix = format!("{session_id}:");
        let mut trackers = self
            .activity_trackers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let keys: Vec<String> = trackers
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys {
            if let Some(tracker) = trackers.remove(&key) {
                tracker.dispose();
            }
        }
    }

    /// Drop the turn flows and steer buffers of a closed session's agents.
    ///
    /// A surviving flow would hold the **closed session's** event bus: the
    /// resume installs fresh plumbing, so a same-process close → resume
    /// would otherwise reuse the dead flow (keyed identically) and every
    /// turn of the resumed session would publish its `turn.step.*` and
    /// turn-internal compaction events onto a bus nobody subscribes to.
    /// Removing the entries makes the resumed session's dispatch
    /// get-or-create a fresh flow, wired onto the fresh bus. An in-flight
    /// turn of the closed session keeps its `Arc` to the inner flow and
    /// finishes on the old bus — it holds no dangling references.
    fn dispose_turn_flows(&self, session_id: &str) {
        let prefix = format!("{session_id}:");
        self.turn_flows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !key.starts_with(&prefix));
        self.steer_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !key.starts_with(&prefix));
        self.compaction_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !key.starts_with(&prefix));
        self.usage_model_key_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| !key.starts_with(&prefix));
    }
}

/// The TS `withManagedMirriPluginEnv` per-server merge (pluginService.ts,
/// item A7): fold the managed env into one plugin stdio server config —
/// `{ ...server.env, ...managedEnv }`, managed values win. http/sse
/// servers and empty managed envs pass through untouched (the TS
/// `transport === 'stdio'` gate and the `Object.keys(...).length === 0`
/// early return). The plugin-host manager already applied its own
/// decoration (`MIRRICODE_HOME` / `MIRRI_PLUGIN_ROOT` + the cwd default,
/// item A7) before this merge runs.
fn apply_managed_mirri_plugin_env(
    mut raw: serde_json::Value,
    managed_env: &HashMap<String, String>,
) -> serde_json::Value {
    if managed_env.is_empty()
        || raw
            .get("transport")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|transport| transport != "stdio")
    {
        return raw;
    }
    let Some(obj) = raw.as_object_mut() else {
        return raw;
    };
    let env = obj
        .entry("env")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(env_obj) = env.as_object_mut() {
        for (key, value) in managed_env {
            env_obj.insert(key.clone(), Value::String(value.clone()));
        }
    }
    raw
}

/// Build one `mcp.list` wire entry from a pool's connection state (the TS
/// `McpServerInfo` shape plus the three-pool `pool` / `source` provenance
/// fields — spec §4.4).
async fn mcp_list_entry(
    manager: &mirri_mcp::McpConnectionManager,
    name: String,
    state: mirri_mcp::McpConnectionState,
    pool: &str,
    source: &str,
) -> Value {
    let transport = match manager.get_server_config(&name).await {
        Some(config) => match config.transport {
            mirri_mcp::McpTransportConfig::Stdio { .. } => "stdio",
            mirri_mcp::McpTransportConfig::Http { .. } => "http",
            mirri_mcp::McpTransportConfig::Sse { .. } => "sse",
        },
        None => "stdio",
    };
    let (status, tool_count, error) = match state {
        mirri_mcp::McpConnectionState::Pending => ("pending", 0, None),
        mirri_mcp::McpConnectionState::Connected { tools, .. } => ("connected", tools.len(), None),
        mirri_mcp::McpConnectionState::Failed { error } => ("failed", 0, Some(error)),
        mirri_mcp::McpConnectionState::Disabled => ("disabled", 0, None),
        mirri_mcp::McpConnectionState::NeedsAuth => ("needs-auth", 0, None),
    };
    let mut entry = json!({
        "name": name,
        "transport": transport,
        "status": status,
        "toolCount": tool_count,
        "pool": pool,
        "source": source,
    });
    if let Some(error) = error {
        entry["error"] = json!(error);
    }
    entry
}

/// The connected server `name`'s advertised tool summaries `(name,
/// description)` from one pool — the enumeration behind both tool-listing
/// wires. `None` when the server is not connected in that pool (absent,
/// pending, failed, disabled, or needs-auth). The enumeration itself is
/// unfiltered: tool-level filtering is the caller's policy — `mcp.tools`
/// annotates each tool with the merged config's verdict, the retired
/// `mcp.global.tools` drops the ones the connection's stored filter
/// excludes. An omitted description (the connection layer stores an empty
/// string) maps to `None` so the wire keeps the field optional.
async fn mcp_tool_summaries(
    manager: &std::sync::Arc<mirri_mcp::McpConnectionManager>,
    name: &str,
) -> Option<Vec<(String, Option<String>)>> {
    let state = manager
        .list_connections()
        .await
        .into_iter()
        .find(|(conn_name, _)| conn_name == name)
        .map(|(_, state)| state)?;
    match state {
        mirri_mcp::McpConnectionState::Connected { tools, .. } => Some(
            tools
                .into_iter()
                .map(|tool| {
                    let description = if tool.description.is_empty() {
                        None
                    } else {
                        Some(tool.description)
                    };
                    (tool.name, description)
                })
                .collect(),
        ),
        _ => None,
    }
}

/// Register every tool of a session's overlay manager into that one
/// session's live agents — the session-scoped counterpart of
/// [`register_server_tools_to_sessions`]. Called by the background task
/// spawned in [`EmbeddedServer::prepare_session_overlay`] once the
/// overlay's connects settle: the tools appear slightly after
/// `session/new` responded (the connects are deliberately not awaited by
/// the request), replacing any same-qualified tool that a lower-priority
/// pool registered earlier. The session resolves through the routing seam
/// (W3) so a non-default workspace's session receives its overlay tools.
async fn register_overlay_tools_to_session(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    registry: &Arc<crate::workspace_registry::WorkspaceRegistry>,
    session_id: &str,
) {
    let all_tools = manager.list_all_tools().await;
    if all_tools.is_empty() {
        return;
    }
    if let Ok(session) = session_routing::resolve_session_via(registry, session_id).await {
        for agent_id in session.list_agents().await {
            if let Some(agent) = session.get_agent(&agent_id).await {
                let tool_manager = agent.tool_manager();
                let mut tm = acquire_tool_table_write(&tool_manager).await;
                for (server_name, def) in &all_tools {
                    let qualified = mirri_mcp::qualify_tool_name(server_name, &def.name);
                    let description = if def.description.is_empty() {
                        format!("MCP tool from '{server_name}': {}", def.name)
                    } else {
                        def.description.clone()
                    };
                    let bridge = mcp_bridge::McpBridgeTool::new(
                        &qualified,
                        &def.name,
                        server_name,
                        &description,
                        def.input_schema.clone(),
                        Arc::clone(manager),
                    );
                    tm.register_mcp_tool(server_name.clone(), Arc::new(bridge));
                }
            }
        }
        tracing::info!(
            session_id,
            tool_count = all_tools.len(),
            "registered overlay MCP tools into the session"
        );
    }
}

/// Follow up on one pool status transition (the shared body of the
/// workspace and user pool listeners): spawned detached, it keeps the
/// live agents' tool tables in sync (Connected registers, teardown states
/// unregister — the S2 race fix and S1 teardown) and publishes the
/// existing `mcp.server.status` session event to every live session (the
/// overlay pool's notification precedent, spec §4.3's notify leg on the
/// established event vocabulary) plus the app-level broadcast (M7, F-9 —
/// no session needed). The event carries `changeKind: "status"` (a plain
/// connection transition; the reconcile kernel's own emissions carry the
/// persisted-change kinds `added`/`removed`/`reconfigured`/`field-change`).
///
/// The session fan-out is a slice (W7): a workspace pool passes exactly
/// its own workspace's manager, while the user pool — shared across every
/// workspace of the engine — passes every built runtime's manager so the
/// user layer's tools and events reach all of them.
fn handle_pool_status_transition(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    session_managers: &[Arc<SessionManager>],
    oauth_token_dir: &Option<std::path::PathBuf>,
    pool_events: &tokio::sync::broadcast::Sender<Arc<Value>>,
    pool: &'static str,
    server_name: &str,
    state: mirri_mcp::McpConnectionState,
) {
    let manager = Arc::clone(manager);
    let session_managers: Vec<Arc<SessionManager>> = session_managers.to_vec();
    let oauth_token_dir = oauth_token_dir.clone();
    let pool_events = pool_events.clone();
    let name = server_name.to_string();
    tokio::spawn(async move {
        match &state {
            mirri_mcp::McpConnectionState::Connected { .. } => {
                // Drop the server's current registrations first so a
                // needs-auth → connected transition cannot leave the
                // synthetic authenticate tool in the table alongside (or
                // instead of) the real tools.
                remove_server_tools_from_sessions(&session_managers, &name).await;
                register_server_tools_to_sessions(&manager, &session_managers, &name).await;
            }
            mirri_mcp::McpConnectionState::NeedsAuth => {
                // The OAuth trigger surface (T8): drop the server's real
                // tools (there are none usable without a token) and
                // register the synthetic authenticate tool the LLM can call
                // to drive the authorization flow.
                remove_server_tools_from_sessions(&session_managers, &name).await;
                register_auth_tool_to_sessions(
                    &manager,
                    &session_managers,
                    &name,
                    oauth_token_dir.as_deref(),
                )
                .await;
            }
            mirri_mcp::McpConnectionState::Failed { .. }
            | mirri_mcp::McpConnectionState::Disabled => {
                remove_server_tools_from_sessions(&session_managers, &name).await;
            }
            _ => {}
        }
        publish_pool_status_event(
            &manager,
            &session_managers,
            &pool_events,
            &name,
            pool,
            Some("status"),
            &state,
        )
        .await;
    });
}

/// Translate one pool transition into the `McpServerStatus` session event
/// (the wire shape `event_projection` already projects) and publish it to
/// every live session's event bus, plus the app-level broadcast (M7, F-9)
/// that `subscribe_mcp_pool_events` consumers observe without any session.
/// A server whose registration is already gone (`remove`'s trailing
/// Pending) has no config to name its transport — skipped.
///
/// `change_kind` names what happened: `"status"` for a plain connection
/// transition (the pool listeners), `"added"` / `"removed"` /
/// `"reconfigured"` / `"field-change"` for the reconcile kernel's pool
/// operations (every persisted change emits, ADR-008).
async fn publish_pool_status_event(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    session_managers: &[Arc<SessionManager>],
    pool_events: &tokio::sync::broadcast::Sender<Arc<Value>>,
    server_name: &str,
    pool: &str,
    change_kind: Option<&str>,
    state: &mirri_mcp::McpConnectionState,
) {
    let transport = match manager.get_server_config(server_name).await {
        Some(config) => match config.transport {
            mirri_mcp::McpTransportConfig::Stdio { .. } => "stdio",
            mirri_mcp::McpTransportConfig::Http { .. } => "http",
            mirri_mcp::McpTransportConfig::Sse { .. } => "sse",
        },
        None => return,
    };
    let (status, tool_count, error) = match state {
        mirri_mcp::McpConnectionState::Pending => ("pending", 0, None),
        mirri_mcp::McpConnectionState::Connected { tools, .. } => {
            ("connected", tools.len() as u32, None)
        }
        mirri_mcp::McpConnectionState::Failed { error } => ("failed", 0, Some(error.clone())),
        mirri_mcp::McpConnectionState::Disabled => ("disabled", 0, None),
        mirri_mcp::McpConnectionState::NeedsAuth => ("needs-auth", 0, None),
    };
    for session_manager in session_managers {
        for session_id in session_manager.workspace().list_sessions().await {
            if let Some(artifacts) = session_manager.session_artifacts(&session_id) {
                artifacts
                    .event_bus
                    .publish(mirri_core::SessionEvent::McpServerStatus {
                        name: server_name.to_string(),
                        transport: transport.to_string(),
                        status: status.to_string(),
                        tool_count,
                        error: error.clone(),
                        pool: pool.to_string(),
                        change_kind: change_kind.map(str::to_string),
                    });
            }
        }
    }
    // The app-level leg (M7): the same server payload without the session
    // envelope — the serve wire's `mcp.subscribe` forwarder wraps it as an
    // `event.mcp.server.status` notification. A send failure means "no
    // subscribers" and is ignored.
    let mut server = json!({
        "name": server_name,
        "transport": transport,
        "status": status,
        "toolCount": tool_count,
        "pool": pool,
    });
    if let Some(error) = &error {
        server["error"] = json!(error);
    }
    if let Some(change_kind) = change_kind {
        server["changeKind"] = json!(change_kind);
    }
    let _ = pool_events.send(Arc::new(json!({ "server": server })));
}

/// Acquire an agent tool table's write lock without ever blocking the
/// calling worker. A turn holds the table's read guard for its whole
/// duration (the dispatch closure in `message_handlers` keeps
/// `agent_tools.read()` until the turn future settles), so a blocking
/// `write()` parks the worker until that turn ends — enough concurrent
/// registrations pin every worker of the runtime and freeze the whole
/// server (HTTP, WebSocket, timers, the stall watchdog). The retry yields
/// with a short backoff instead: the write applies as soon as the turn's
/// guard drops.
async fn acquire_tool_table_write(
    tool_manager: &std::sync::Arc<parking_lot::RwLock<mirri_core::ToolManager>>,
) -> parking_lot::RwLockWriteGuard<'_, mirri_core::ToolManager> {
    loop {
        if let Some(guard) = tool_manager.try_write() {
            return guard;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Register all tools from a specific MCP server into every live agent
/// across all active sessions. Called by the status listener when a
/// server transitions to `Connected` — handles the race where
/// `session.create` fires before background connections complete.
async fn register_server_tools_to_sessions(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    session_managers: &[Arc<SessionManager>],
    server_name: &str,
) {
    // Gather tools for this specific server.
    let all_tools = manager.list_all_tools().await;
    let server_tools: Vec<_> = all_tools
        .into_iter()
        .filter(|(s, _)| s == server_name)
        .collect();
    if server_tools.is_empty() {
        return;
    }

    // Iterate every addressed workspace's live sessions and their agents
    // (the W7 slice fan-out: a workspace pool addresses its own workspace,
    // the user pool every built one).
    for session_manager in session_managers {
        let session_ids = session_manager.workspace().list_sessions().await;
        for sid in &session_ids {
            if let Some(session) = session_manager.workspace().get_session(sid).await {
                let agent_ids = session.list_agents().await;
                for aid in &agent_ids {
                    if let Some(agent) = session.get_agent(aid).await {
                        let tool_manager = agent.tool_manager();
                        let mut tm = acquire_tool_table_write(&tool_manager).await;
                        for (_, def) in &server_tools {
                            let qualified = mirri_mcp::qualify_tool_name(server_name, &def.name);
                            let description = if def.description.is_empty() {
                                format!("MCP tool from '{server_name}': {}", def.name)
                            } else {
                                def.description.clone()
                            };
                            let bridge = mcp_bridge::McpBridgeTool::new(
                                &qualified,
                                &def.name,
                                server_name,
                                &description,
                                def.input_schema.clone(),
                                Arc::clone(manager),
                            );
                            tm.register_mcp_tool(server_name.to_string(), Arc::new(bridge));
                        }
                    }
                }
            }
        }
    }
    tracing::info!(server = %server_name, tool_count = server_tools.len(), "registered MCP tools into live agents");
}

/// Register the synthetic `mcp__<server>__authenticate` tool into every
/// live agent (the `needs-auth` state's surface, T8): the tool a remote
/// OAuth server exposes so the LLM can drive the authorization flow. The
/// config's transport URL and OAuth settings come from the connection's
/// stored config; a config without a usable remote URL (or without an
/// OAuth token store) leaves the state visible in `mcp.list` but no tool
/// to drive.
async fn register_auth_tool_to_sessions(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    session_managers: &[Arc<SessionManager>],
    server_name: &str,
    oauth_token_dir: Option<&std::path::Path>,
) {
    let Some(bridge) = build_mcp_auth_tool(manager, server_name, oauth_token_dir).await else {
        return;
    };
    let qualified = mirri_mcp::qualify_tool_name(server_name, "authenticate");
    tracing::info!(
        server = %server_name,
        tool = %qualified,
        "registering the MCP OAuth authenticate tool into live agents"
    );

    let tool: std::sync::Arc<dyn mirri_core::Tool> = std::sync::Arc::new(bridge);
    for session_manager in session_managers {
        let session_ids = session_manager.workspace().list_sessions().await;
        for sid in &session_ids {
            if let Some(session) = session_manager.workspace().get_session(sid).await {
                let agent_ids = session.list_agents().await;
                for aid in &agent_ids {
                    if let Some(agent) = session.get_agent(aid).await {
                        let tool_manager = agent.tool_manager();
                        let mut tm = acquire_tool_table_write(&tool_manager).await;
                        tm.register_mcp_tool(server_name.to_string(), Arc::clone(&tool));
                    }
                }
            }
        }
    }
}

/// Build the synthetic `mcp__<server>__authenticate` tool for a
/// `needs-auth` server from its stored config: the remote transport URL
/// and the OAuth settings (client id / scopes) come from the config, the
/// token store from the engine's `<mirri home>/mcp-tokens` directory.
/// `None` when the config is unusable (no remote URL, no OAuth block) or
/// no token store is wired — the state stays visible in `mcp.list`, just
/// without a tool to drive.
async fn build_mcp_auth_tool(
    manager: &Arc<mirri_mcp::McpConnectionManager>,
    server_name: &str,
    oauth_token_dir: Option<&std::path::Path>,
) -> Option<mcp_bridge::McpAuthBridgeTool> {
    let config = manager.get_server_config(server_name).await?;
    // The remote endpoint the flow discovers against (A4: HTTP/SSE only).
    let server_url = match &config.transport {
        mirri_mcp::McpTransportConfig::Http { url, .. } => url.clone(),
        mirri_mcp::McpTransportConfig::Sse { url, .. } => url.clone(),
        mirri_mcp::McpTransportConfig::Stdio { .. } => return None,
    };
    let (client_id, scopes) = match &config.auth {
        Some(mirri_mcp::McpAuthConfig::OAuth { client_id, scopes }) => {
            (client_id.clone(), scopes.clone().unwrap_or_default())
        }
        _ => (None, Vec::new()),
    };
    let token_dir = oauth_token_dir.map(std::path::Path::to_path_buf)?;
    let qualified = mirri_mcp::qualify_tool_name(server_name, "authenticate");
    Some(mcp_bridge::McpAuthBridgeTool::new(
        &qualified,
        server_name,
        &server_url,
        client_id,
        scopes,
        token_dir,
        Arc::clone(manager),
    ))
}

/// Remove all tools from a specific MCP server from every live agent.
/// Called by the status listener when a server disconnects, fails, or is
/// disabled — ensures stale `McpBridgeTool` instances don't linger for
/// the LLM to call.
async fn remove_server_tools_from_sessions(
    session_managers: &[Arc<SessionManager>],
    server_name: &str,
) {
    let mut removed_total = 0usize;
    for session_manager in session_managers {
        let session_ids = session_manager.workspace().list_sessions().await;
        for sid in &session_ids {
            if let Some(session) = session_manager.workspace().get_session(sid).await {
                let agent_ids = session.list_agents().await;
                for aid in &agent_ids {
                    if let Some(agent) = session.get_agent(aid).await {
                        let tool_manager = agent.tool_manager();
                        let mut tm = acquire_tool_table_write(&tool_manager).await;
                        removed_total += tm.remove_mcp_tools_by_server(server_name);
                    }
                }
            }
        }
    }
    if removed_total > 0 {
        tracing::info!(server = %server_name, removed = removed_total, "removed MCP tools from live agents");
    } else {
        tracing::debug!(server = %server_name, "removed MCP tools from live agents (no-op)");
    }
}

impl Default for EmbeddedServer {
    fn default() -> Self {
        Self::for_cwd()
    }
}

#[async_trait]
impl EngineAdapter for EmbeddedServer {
    /// Dispatch one RPC method. Every route accepts **both** the TS client's
    /// camelCase wire name and the dotted protocol name (Phase 4.4: the TS
    /// client sends `createSession`, `prompt`, … while the Rust engine
    /// dispatches on `session.create`, `message.send`, … — the two naming
    /// conventions are aliased in the route table so both work).
    async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        // Lazily spawn background MCP connections on the first call. This
        // runs inside the Tokio runtime (guaranteed by EngineBridge), unlike
        // for_cwd() which may execute before the runtime exists.
        self.maybe_spawn_mcp_connect();
        // The per-workspace fan-out of the same lazy pattern (W7): every
        // built non-default runtime's project layers connect exactly once.
        self.maybe_spawn_workspace_pool_connects();
        // Plan Task 5 (ADR-010 decision 1): mount the per-workspace skill
        // directory watchers — the same lazy pattern (construction may
        // precede the Tokio runtime; the first RPC call never does), and
        // workspaces built lazily AFTER this point are picked up on the
        // next call because the pass is idempotent over built runtimes.
        // The mount itself is awaited (spawn_blocking inside): the first
        // RPC pays the watch registration once per workspace, after
        // which the handle's existence is a mounted watcher.
        self.spawn_skill_watchers();
        // P5: the models.dev catalog background preheat rides the same lazy
        // spawn pattern — construction may precede the runtime, the first
        // RPC call never does.
        self.maybe_spawn_models_catalog_preheat();
        // Batch 1 (US C1.1): the per-server index refresh driver rides the
        // same lazy spawn pattern — it ticks the re-runnable index converge
        // so a foreign entry's mutation on the shared home surfaces in this
        // engine's session.list within the freshness budget (flag-gated).
        self.maybe_spawn_index_refresh_driver();
        // Task 8: the retention compression task rides the same lazy
        // spawn seam — only when `[retention] enabled = true` (the
        // assembly-time gate; 默认即无操作).
        self.maybe_spawn_retention_task();
        // Phase 2: the one-time legacy flat-layout migration settles before
        // the first dispatch reads the sessions root (an inline await, not
        // a spawn — a racing session.resume must find the moved session).
        self.migrate_legacy_sessions_once().await;
        // ADR-012 c/d/f: the one-time startup orphan census settles before
        // any session of this instance registers a task (the Task 3
        // ordering ledger — the census's candidates are all Running+Process
        // archives with no epoch filter, so it must not run behind a live
        // task of this instance), then the resident lifetime sweeper
        // spawns behind it.
        self.run_startup_orphan_governance().await;
        // Phase 3: the one-time session-index converge settles before the
        // first dispatch reads the sessions root too (an inline await for
        // the same reason — a racing session.list must see the reconciled
        // rows).
        self.reconcile_session_index_once().await;

        // Registry-driven dispatch: the static route table is the whole
        // method surface (186 routes, every domain migrated).
        match routes::find_route(method) {
            Some(route) => (route.handler)(self, params).await,
            None => Err(EngineError::MethodNotFound(method.to_string())),
        }
    }

    async fn subscribe_events(&self) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        // Server-scoped lifecycle events ride the same channel as the
        // session relays (contract 3 — one event stream per subscriber):
        // a per-subscriber forwarder translates the server-event broadcast
        // into notifications on this subscriber's channel. The forwarder is
        // spawned here — `subscribe_events` is async, so a Tokio runtime is
        // guaranteed to exist. A lagging subscriber is lagged (the
        // broadcast's bounded-ring semantics), never blocking the emitter.
        let mut server_rx = self.server_events.subscribe();
        let server_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match server_rx.recv().await {
                    Ok(event) => {
                        let notification = event_projection::project_server_event(&event);
                        let framed = JsonRpcNotification {
                            method: notification.method,
                            params: notification.params,
                        };
                        if server_tx.send(framed).await.is_err() {
                            break; // the subscriber's channel closed
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!("server event broadcast lagged in embedded subscriber");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        // The app-level skill-catalog change stream (ADR-010 Task 7) rides
        // the same channel: a per-subscriber forwarder relays the
        // orchestrator's camelCase wire broadcast onto this subscriber's
        // channel as `skill.catalogChanged` notifications — the same
        // payload the serve-wire `skill.subscribe` forwarder emits, so the
        // in-process consumers (the NAPI event channel the TUI's SDK
        // client rides) see the identical surface. A lagging subscriber is
        // lagged, never blocking the emitter.
        let mut skill_catalog_rx = self.skill_reload.subscribe_camelcase();
        let skill_catalog_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                match skill_catalog_rx.recv().await {
                    Ok(params) => {
                        let framed = JsonRpcNotification {
                            method: "skill.catalogChanged".to_string(),
                            params,
                        };
                        if skill_catalog_tx.send(framed).await.is_err() {
                            break; // the subscriber's channel closed
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!("skill catalog broadcast lagged in embedded subscriber");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        Ok(rx)
    }

    /// The app-level MCP pool surface (M7, F-9): the pool status broadcast
    /// behind `mcp.subscribe` on the serve wire. Every pool transition and
    /// every reconcile pool operation is mirrored here (the same payloads
    /// the session-level fan-out publishes, minus the session envelope),
    /// independent of any live session.
    fn subscribe_mcp_pool_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<std::sync::Arc<Value>>, EngineError> {
        Ok(self.mcp_pool_events.subscribe())
    }

    /// The app-level skill-catalog change surface (ADR-010 Task 6, the
    /// `subscribe_mcp_pool_events` precedent shape): the orchestrator's
    /// per-workspace change broadcast, re-keyed to camelCase at the
    /// boundary — consumers observe `skill.catalogChanged` notifications
    /// with `{workspaceId, added, removed, updated, trigger}` params, no
    /// session envelope (the same no-session shape `event.workspace.*`
    /// uses). Independent of any live session.
    fn subscribe_skill_catalog_events(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<Value>, EngineError> {
        Ok(self
            .skill_reload
            .subscribe_camelcase())
    }

    async fn on_reverse_rpc(&self, handler: ReverseRpcHandler) -> Result<(), EngineError> {
        // Wrap the transport-layer handler in a ReverseRpcCallback adapter
        // so the engine core can consume it through the trait.
        let callback: Arc<dyn ReverseRpcCallback> = Arc::new(ReverseRpcCallbackImpl {
            handler: Arc::clone(&handler),
        });
        {
            let mut rpc = self.reverse_rpc.lock().unwrap_or_else(|e| e.into_inner());
            *rpc = Some(handler);
        }
        {
            let mut cb = self
                .reverse_rpc_callback
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *cb = Some(callback);
        }
        Ok(())
    }

    /// The sessions with an engine turn in flight right now — the union
    /// source the transport read faces (`session.status` / `session.snapshot`
    /// / `session.get`) consult so driver-injected turns (parked-notification
    /// reactivations, cron) are not reported idle: the shared registry every
    /// dispatch origin registers into.
    async fn in_flight_sessions(&self) -> Vec<String> {
        // The registry keys are "{session_id}:{agent_id}:{turn_id}" and a
        // session id ("session-<uuid>") never contains a colon, so the
        // first segment is the session.
        let turns = self
            .in_flight_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sessions: Vec<String> = turns
            .keys()
            .map(|key| key.split(':').next().unwrap_or(key).to_string())
            .collect();
        sessions.sort();
        sessions.dedup();
        sessions
    }

    async fn dispose(&self) -> Result<(), EngineError> {
        for (session_id, handle) in
            std::mem::take(&mut *self.relays.lock().unwrap_or_else(|e| e.into_inner()))
        {
            handle.abort();
            tracing::debug!(session_id, "embedded event relay aborted on dispose");
        }
        // The follow drivers are the relay map's kin (one task per live
        // session): dispose must not leak them either. Aborting a finished
        // handle is a no-op, so the exit-on-close / exit-on-truncation
        // paths need no special casing here.
        for (session_id, handle) in std::mem::take(
            &mut *self
                .follow_drivers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        ) {
            handle.abort();
            tracing::debug!(session_id, "session follow driver aborted on dispose");
        }
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(())
    }
}

/// ADR-009 (slot form): mirror the dispatch-resolved reverse-RPC callback
/// into the SESSION slot the agent's tools resolve. The serve transport
/// installs its per-connection callback into the session artifacts at
/// subscribe — possibly later than the agent's create-time — and this mirror
/// guarantees the slot holds the value this turn dispatches on, so both the
/// `AskUserQuestion` tool's call-time resolution and mid-turn subagent
/// spawns observe the current connection. The agent CONFIG needs no mirror
/// anymore: it carries the slot handle, not a callback snapshot.
pub(crate) fn mirror_reverse_rpc_callback_into_agent_config(
    agent: &AgentScope,
    callback: &Option<Arc<dyn ReverseRpcCallback>>,
) {
    let session_slot = agent.session_reverse_rpc_slot();
    let mut guard = session_slot.write().unwrap_or_else(|e| e.into_inner());
    // Preserve an already-installed serve-transport callback over `None`
    // (a driver turn with no resolved callback must not clear a live
    // connection's callback).
    if callback.is_some() || guard.is_none() {
        *guard = callback.clone();
    }
}

// --- submodule wiring ------------------------------------------------
/// Public (not `mod`): the serve (`mirri-server`) wire reuses the same
/// grant sink implementation at its `approval.decide` override.
pub mod additional_dirs;
mod compaction_handlers;
mod config_handlers;
mod cron_driver;
mod env_handlers;
mod event_relay;
mod flag_handlers;
mod follow_scheduler;
mod fs_handlers;
mod goal_driver;
mod index_refresh;
mod interaction_handlers;
mod mcp_bridge;
mod mcp_config_handlers;
mod message_handlers;
mod model_handlers;
mod models_dev_import;
mod orphan_governance;
mod permission_handlers;
mod print_mode;
mod profile_handlers;
mod prompt_parts;
mod provider_handlers;
mod responses;
mod retention_delete;
mod retention_handlers;
mod retention_task;
mod routes;
mod search;
mod session_handlers;
mod session_index;
mod session_routing;
mod side_title;
mod task_driver;
mod workspace_handlers;

use event_relay::live_chunk_callback;

/// M1 wiring tests for [`turn_flow_config_from`]: the `[loop_control]`
/// config section and the model's catalog window now drive the production
/// turn-flow config (previously every construction site used
/// [`TurnFlowConfig::default`], so the parsed config never took effect and
/// the compaction window stayed unknown).
#[cfg(test)]
mod turn_flow_config_wiring_tests {
    use super::*;
    use mirri_core::config::LoopControlConfig;

    #[test]
    fn should_build_config_from_loop_control_when_section_present() {
        // Given a server config with [loop_control] set and a known window
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig {
                max_steps_per_turn: Some(120),
                max_retries_per_step: Some(3),
                reserved_context_size: Some(8_000),
                compaction_trigger_ratio: Some(0.7),
                retention_tail_ratio: Some(0.2),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 200_000);

        // Then every wired field flows through
        assert_eq!(config.max_steps, Some(120));
        assert_eq!(config.max_retry_attempts, 3);
        assert_eq!(config.compaction_strategy.max_context_tokens, 200_000);
        assert_eq!(config.compaction_strategy.reserved_context_size, 8_000);
        assert_eq!(config.compaction_strategy.trigger_ratio, 0.7);
        assert_eq!(config.compaction_strategy.retention_tail_ratio, 0.2);
        assert_eq!(config.compaction_strategy.retention_tail_tokens, None);
    }

    #[test]
    fn should_apply_absolute_retention_budget_when_tokens_configured() {
        // Given a server config whose [loop_control] sets the absolute
        // retention budget (US6.2: the ratio spelling stays unset)
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig {
                retention_tail_tokens: Some(5_000),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 100_000);

        // Then the absolute budget rides the strategy and the ratio keeps
        // its default (the absolute value wins in
        // `FullCompaction::retention_budget_tokens`)
        assert_eq!(
            config.compaction_strategy.retention_tail_tokens,
            Some(5_000)
        );
        assert_eq!(
            config.compaction_strategy.retention_tail_ratio,
            mirri_core::compaction::RETENTION_TAIL_RATIO
        );
    }

    #[test]
    fn should_keep_the_unlimited_default_when_loop_control_omits_max_steps() {
        // Given a server config whose [loop_control] sets only the ratio
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig {
                compaction_trigger_ratio: Some(0.7),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 100_000);

        // Then the default stays unlimited (TS parity: an unset
        // maxStepsPerTurn means no limit); an explicit limit — including
        // 0 as a no-op spelling of unlimited — still takes effect
        assert_eq!(config.max_steps, None);
        assert_eq!(config.compaction_strategy.trigger_ratio, 0.7);
    }

    #[test]
    fn should_fall_back_to_defaults_when_loop_control_absent() {
        // Given a server config without [loop_control] but a known window
        let server_config = ServerConfig::default();

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 100_000);

        // Then defaults hold except the window
        let default = TurnFlowConfig::default();
        assert_eq!(config.max_steps, default.max_steps);
        assert_eq!(config.max_retry_attempts, default.max_retry_attempts);
        assert_eq!(config.compaction_strategy.max_context_tokens, 100_000);
        assert_eq!(
            config.compaction_strategy.reserved_context_size,
            default.compaction_strategy.reserved_context_size
        );
    }

    #[test]
    fn should_keep_unknown_window_when_catalog_has_no_entry() {
        // Given a server config whose loop_control is present
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig::default()),
            ..ServerConfig::default()
        };

        // When building with window 0 (model unknown to the catalog)
        let config = turn_flow_config_from(&server_config, 0);

        // Then the window stays 0 — ratio triggers stay off until an
        // overflow teaches the engine (Task 1's conservative learning).
        assert_eq!(config.compaction_strategy.max_context_tokens, 0);
    }

    #[test]
    fn should_use_configured_overflow_retry_limit_when_loop_control_sets_it() {
        // Given a server config whose [loop_control] sets the overflow
        // compaction retry limit to 1
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig {
                max_overflow_compaction_attempts: Some(1),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 100_000);

        // Then the strategy honors the configured limit (the default of 3
        // only survives an absent key, handled by the partial-section
        // guard)
        assert_eq!(
            config.compaction_strategy.max_overflow_compaction_attempts,
            1
        );
    }

    #[test]
    fn should_apply_configured_overflow_limit_when_strategy_comes_from_loop_control() {
        // Task 3: pin the whole config-key line end to end — a user-written
        // `[loop_control] max_overflow_compaction_attempts` must survive the
        // TOML parse (`LoopControlConfig`), the strategy build
        // (`turn_flow_config_from`, with the model's catalog window as the
        // signature's other input), and then actually change the overflow
        // ladder's behavior. The sibling test above stops at the strategy
        // field; this one drives `FullCompaction::handle_overflow_error`
        // and proves the ladder gives up after ONE retry instead of the
        // default three (the hard-coded-strategy unit tests in mirri-core's
        // compaction_tests.rs pin only the constant, not the config line).
        use mirri_core::compaction::FullCompaction;
        use mirri_core::context_memory::ContextMemory;
        use mirri_core::error::EngineError;

        // Given a config whose [loop_control] caps overflow compaction at 1
        // attempt (the TOML spelling users write; `from_toml` is the parse
        // path every load — boot, hot reload, `config.set` — funnels
        // through)
        let with_key =
            ServerConfig::from_toml("[loop_control]\nmax_overflow_compaction_attempts = 1")
                .expect("the loop_control section must parse");

        // And a partial section without the key — the control arm: the
        // early give-up below must be attributable to the config key alone,
        // not to the ladder behaving that way generally.
        let without_key = ServerConfig::from_toml("[loop_control]\nmax_steps_per_turn = 30")
            .expect("the partial loop_control section must parse");

        // When the per-agent turn-flow config is built (the same
        // construction the embedded server applies per model: loop-control
        // limits + the model's catalog window)
        let flow_with_key = turn_flow_config_from(&with_key, 100_000);
        let flow_without_key = turn_flow_config_from(&without_key, 100_000);
        // the model window rides the strategy (the signature's other input)
        assert_eq!(
            flow_with_key.compaction_strategy.max_context_tokens,
            100_000
        );

        // Then the configured cap reaches the ladder's strategy surface
        // (read back through the production accessor, not the input
        // struct, so a strategy-build bug cannot self-confirm)...
        let mut fc = FullCompaction::new(flow_with_key.compaction_strategy);
        assert_eq!(fc.strategy_max_overflow_attempts(), 1);

        let mem = ContextMemory::new();
        let cancel = CancelToken::new();

        // ...the first overflow is absorbed (counter 1 ≤ limit 1: one
        // retry happens — the caller may still `begin` a compaction)...
        fc.handle_overflow_error(&mem, &cancel, 85_000).unwrap();
        assert_eq!(fc.consecutive_overflow_compactions(), 1);

        // ...and the ladder gives up on the SECOND overflow instead of the
        // fourth — the config key, not the default constant, decided it.
        assert!(matches!(
            fc.handle_overflow_error(&mem, &cancel, 85_000),
            Err(EngineError::ContextOverflow { attempts: 2 })
        ));
        assert_eq!(fc.consecutive_overflow_compactions(), 2);

        // And the control arm: the same construction without the key keeps
        // the default of 3, so the same two overflows are both absorbed.
        let mut fc_default = FullCompaction::new(flow_without_key.compaction_strategy);
        assert_eq!(fc_default.strategy_max_overflow_attempts(), 3);
        fc_default
            .handle_overflow_error(&mem, &cancel, 85_000)
            .unwrap();
        fc_default
            .handle_overflow_error(&mem, &cancel, 85_000)
            .unwrap();
        assert_eq!(fc_default.consecutive_overflow_compactions(), 2);
    }

    #[test]
    fn should_use_configured_capacity_limits_when_loop_control_sets_them() {
        // Given a server config whose [loop_control] sets the resume-time
        // capacity keys (history 4 / deferred 2 — the caps the turn flow
        // applies via `set_limits` from the strategy every turn, and the
        // resume / close-time-snapshot fold simulations model through
        // `ReplayLimits`)
        let server_config = ServerConfig {
            loop_control: Some(LoopControlConfig {
                max_history_messages: Some(4),
                max_deferred_messages: Some(2),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };

        // When building the turn-flow config
        let config = turn_flow_config_from(&server_config, 0);

        // Then both caps ride the compaction strategy (the surface the
        // turn flow's `set_limits` and the resume-path `ReplayLimits`
        // read from)
        assert_eq!(config.compaction_strategy.max_history_messages, 4);
        assert_eq!(config.compaction_strategy.max_deferred_messages, 2);

        // And a partial [loop_control] section without the keys keeps
        // the 10 000 / 1 000 production defaults (an absent key must
        // never silently tighten or lift the caps)
        let partial = ServerConfig {
            loop_control: Some(LoopControlConfig {
                compaction_trigger_ratio: Some(0.7),
                ..LoopControlConfig::default()
            }),
            ..ServerConfig::default()
        };
        let partial_config = turn_flow_config_from(&partial, 0);
        assert_eq!(
            partial_config.compaction_strategy.max_history_messages,
            10_000
        );
        assert_eq!(
            partial_config.compaction_strategy.max_deferred_messages,
            1_000
        );
    }
}

#[cfg(test)]
mod session_routing_tests;
#[cfg(test)]
mod tests;
