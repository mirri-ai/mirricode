//! Configuration structs for each scope level.
//!
//! Each scope carries its own config struct. All configs implement [`Default`]
//! with sensible values for development and testing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::hook_engine::HookDef;
use crate::permission::{PermissionMode, PermissionRule, SessionApprovals};
use crate::profile::ThinkingEffort;

// ---------------------------------------------------------------------------
// AppScopeConfig
// ---------------------------------------------------------------------------

/// Global configuration held by [`AppScope`](super::AppScope).
///
/// These values apply to every workspace, session, and agent unless overridden
/// at a deeper level.
#[derive(Debug, Clone)]
pub struct AppScopeConfig {
    /// Default model ID used when an agent does not specify one. Empty by
    /// design (spec 11 R4): the system bakes in no concrete model — the
    /// session-creation chain resolves `modelId` → config `default_model`
    /// and fails loudly when neither is set.
    pub default_model: String,
    /// Default maximum number of turns before the loop engine stops.
    pub default_max_turns: u32,
    /// Request timeout in seconds for LLM calls.
    pub request_timeout_secs: u64,
    /// Maximum number of tools that may execute concurrently in a single step.
    pub max_concurrent_tools: usize,
}

impl Default for AppScopeConfig {
    fn default() -> Self {
        Self {
            // No built-in model (spec 11 R4): an empty default is the
            // legal "unset" state; resolution failures surface as
            // actionable errors at the consumption points.
            default_model: String::new(),
            default_max_turns: 200,
            request_timeout_secs: 300,
            max_concurrent_tools: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceScopeConfig
// ---------------------------------------------------------------------------

/// Trust level for a workspace — controls whether destructive operations are
/// permitted without explicit confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceTrustLevel {
    /// The workspace is trusted — operations proceed with fewer prompts.
    Trusted,
    /// The workspace is untrusted — destructive operations require approval.
    // Default to untrusted for safety: the host must explicitly opt in to
    // trusted mode.
    #[default]
    Untrusted,
}

/// Configuration held by [`WorkspaceScope`](super::WorkspaceScope).
#[derive(Debug, Clone)]
pub struct WorkspaceScopeConfig {
    /// Whether the workspace is trusted for destructive operations.
    pub trust_level: WorkspaceTrustLevel,
    /// Default permission mode for sessions created in this workspace.
    pub permission_mode: PermissionMode,
    /// External command hooks shared by every session in this workspace.
    ///
    /// Registered into each session's [`HookEngine`](crate::hook_engine::HookEngine)
    /// at session creation, before any session-specific hooks.
    pub hooks: Vec<HookDef>,
    /// Additional directories this workspace may access beyond the root
    /// working directory (`cwd`).
    ///
    /// Declared for permission/trust purposes so the engine knows they are
    /// part of the workspace — e.g. a shared asset directory located outside
    /// `cwd`. File tools operate on absolute paths, so this list is
    /// informational: the engine neither grants nor revokes access based on
    /// it.
    pub extra_directories: Vec<PathBuf>,
    /// Whether agents in this workspace get the **project-level**
    /// AGENTS.md sections injected into their system prompts (the W7 trust
    /// gate, design `08-multi-workspace.md` §5 D6). The user-level sections
    /// (brand + generic) always inject; an untrusted workspace's runtime is
    /// assembled with this `false`, so its own instruction files never
    /// reach the model. Defaults to `true` — the historical behavior every
    /// pre-multi-workspace constructor keeps.
    pub load_project_agents_md: bool,
}

impl Default for WorkspaceScopeConfig {
    fn default() -> Self {
        Self {
            trust_level: WorkspaceTrustLevel::default(),
            permission_mode: PermissionMode::default(),
            hooks: Vec::new(),
            extra_directories: Vec::new(),
            // The historical behavior: every pre-multi-workspace
            // constructor injects the project AGENTS.md sections. The W7
            // trust gate is the only writer of `false`.
            load_project_agents_md: true,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionScopeConfig
// ---------------------------------------------------------------------------

/// Configuration held by [`SessionScope`](super::SessionScope).
#[derive(Debug, Clone)]
pub struct SessionScopeConfig {
    /// Maximum turns before the session's agent loops stop.
    pub max_turns: u32,
    /// Maximum number of concurrent agents allowed in this session.
    pub max_concurrent_agents: usize,
    /// External command hooks specific to this session.
    ///
    /// Registered into the session's [`HookEngine`](crate::hook_engine::HookEngine)
    /// at construction, in addition to the workspace-wide hooks inherited
    /// from [`WorkspaceScopeConfig::hooks`].
    pub hooks: Vec<HookDef>,
    /// Whether agents in this session get the project-level AGENTS.md
    /// sections injected (the W7 trust gate). Copied from the owning
    /// [`WorkspaceScopeConfig::load_project_agents_md`] at session
    /// creation — the workspace owns the trust decision, exactly like the
    /// hook inheritance. Defaults to `true` (directly-constructed test
    /// scopes keep the historical behavior).
    pub load_project_agents_md: bool,
    /// Background-task lifecycle knobs (B1, D-40/D-49/D-63/D-64): capacity,
    /// per-subagent timeout, kill grace, close-time convergence, and the
    /// staleness heartbeat window. Assembled from the typed `[task]` /
    /// `[subagent]` config sections by
    /// [`session_scope_config_from`](crate::session_scope_config_from)
    /// (the embedded server's assembly entry); directly-constructed test
    /// scopes keep the TS defaults.
    pub task_lifecycle: TaskLifecycleSettings,
    /// The user-level permission rules (`[[permission.rules]]` of the mirri
    /// home config.toml, D-34) the session's agents evaluate under. Seeded
    /// at construction by [`session_scope_config_from`] so the rules are in
    /// force from the very first agent the session creates — no post-hoc
    /// push window; directly-constructed test scopes keep none.
    pub user_permission_rules: Vec<crate::permission::PermissionRule>,
    /// The subagent nesting-depth cap (B4 T5, SDR-B4-03): dispatch chains
    /// deeper than this are refused at the spawn gate. The default 5
    /// covers realistic delegation chains; `0` disables the limit (TS
    /// parity). Threaded into every [`SessionSubagentHost`] the session's
    /// agents dispatch through (nested hosts inherit the same cap).
    pub max_nesting_depth: u32,
    /// The session's initial permission posture (SDR-004 b): seeded from
    /// the user config's `default_permission_mode` — a seed only, never a
    /// runtime decision input for live sessions.
    pub permission_mode: PermissionMode,
    /// The enabled plugins' rendered system-prompt contributions (item J,
    /// zero-IO contract 6): pure data assembled at the composition root
    /// (the embedded server seeds it from the plugin manager when building
    /// the session config) and rendered into the default base prompt's
    /// `{{plugin_sections}}` slot at agent creation. Empty for sessions
    /// without plugins.
    pub plugin_system_prompts: Vec<crate::plugin_runtime::PluginSystemPromptSection>,
}

/// The background-task lifecycle knobs a [`SessionScope`](super::SessionScope)
/// applies to its [`TaskManager`](crate::task::TaskManager) at construction
/// (B1). Mirrors the TS `taskService` defaults (`taskService.ts:764`,
/// `printDefaults.ts`).
#[derive(Debug, Clone)]
pub struct TaskLifecycleSettings {
    /// Per-subagent timeout (`Agent` / `AgentSwarm`, foreground and
    /// background). `None` = unbounded (the `[subagent] timeoutMs = 0`
    /// and print-mode semantics).
    pub subagent_timeout: Option<std::time::Duration>,
    /// Per-call item cap for the `AgentSwarm` tool (SDR-012): a single
    /// swarm dispatches at most this many members. `None` keeps the engine
    /// default of 3; `Some(0)` means unlimited. Seeded by
    /// [`session_scope_config_from`](crate::session_scope_config_from) from
    /// the `[subagent]` `maxSwarmItems` config key (env overrides are
    /// resolved by the tool at call time).
    pub max_swarm_items: Option<u32>,
    /// Maximum number of concurrently running background tasks.
    /// `0` = unbounded.
    pub max_running_tasks: usize,
    /// Grace window before an escalated stop lands `stopped` (D-63).
    pub kill_grace: std::time::Duration,
    /// Whether session close leaves background tasks running (D-64:
    /// `keepAliveOnExit`).
    pub keep_alive_on_exit: bool,
    /// Liveness heartbeat window for stale detection. `None` disables
    /// staleness entirely.
    pub heartbeat_interval: Option<std::time::Duration>,
}

impl Default for TaskLifecycleSettings {
    fn default() -> Self {
        Self {
            // TS default: 2h subagent timeout.
            subagent_timeout: Some(std::time::Duration::from_secs(2 * 60 * 60)),
            // SDR-012: the swarm item cap defaults to 3 (engine-side;
            // resolved by the tool when unset).
            max_swarm_items: None,
            max_running_tasks: 0,
            // TS default: 5s kill grace (`taskService.ts:764`).
            kill_grace: std::time::Duration::from_millis(5_000),
            keep_alive_on_exit: false,
            // TS default: 5-minute stale window.
            heartbeat_interval: Some(std::time::Duration::from_secs(300)),
        }
    }
}

impl Default for SessionScopeConfig {
    fn default() -> Self {
        Self {
            max_turns: 200,
            max_concurrent_agents: 10,
            hooks: Vec::new(),
            load_project_agents_md: true,
            task_lifecycle: TaskLifecycleSettings::default(),
            user_permission_rules: Vec::new(),
            max_nesting_depth: crate::subagent::DEFAULT_MAX_NESTING_DEPTH,
            permission_mode: PermissionMode::default(),
            plugin_system_prompts: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentScopeConfig
// ---------------------------------------------------------------------------

/// The LLM provider implementation to use for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ProviderType {
    /// Anthropic (Claude).
    #[default]
    Anthropic,
    /// OpenAI (GPT).
    OpenAi,
    /// OpenAI Responses API (newer streaming format with item IDs).
    OpenAiResponses,
    /// Google Generative AI (Gemini).
    Google,
    /// Kimi (Moonshot) — an OpenAI Chat Completions wire dialect whose
    /// thinking intent travels as a top-level `thinking` object instead
    /// of `reasoning_effort`.
    Kimi,
    /// A custom provider implementation.
    Custom,
}

/// Initial token-usage values for restoring an agent's accumulated usage after
/// a session resume.
///
/// When an agent is recreated from a replayed wire log, its
/// [`AgentUsageService`](crate::usage::AgentUsageService) starts empty. Setting
/// `initial_usage` in the [`AgentScopeConfig`] pre-populates the service so
/// that cumulative totals reflect the usage already consumed before the
/// restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageInitialState {
    /// Total input tokens accumulated before this agent was created.
    pub total_input_tokens: u64,
    /// Total output tokens accumulated before this agent was created.
    pub total_output_tokens: u64,
    /// Total prompt-cache read tokens accumulated before this agent was
    /// created. `#[serde(default)]` keeps older persisted state (written
    /// before the three-bucket carry-through) deserializable.
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// Total prompt-cache write tokens accumulated before this agent was
    /// created.
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    /// The session's first LLM call usage — the cold-start baseline
    /// (SDR-004). `#[serde(default)]` keeps older persisted state
    /// deserializable.
    #[serde(default)]
    pub first_call: Option<crate::usage::TokenUsage>,
}

/// Configuration held by [`AgentScope`](super::AgentScope).
///
/// Each agent may override the model, provider, and operational parameters
/// inherited from the session and workspace. The tool/behavior fields
/// (`max_steps_per_turn`, tool lists, capabilities, thinking effort) are
/// typically populated by resolving an [`AgentProfile`](crate::profile::AgentProfile)
/// via [`ProfileManager::resolve`](crate::profile::ProfileManager::resolve);
/// `None` means "use the engine/host default".
#[derive(Clone)]
pub struct AgentScopeConfig {
    /// Model ID to use for LLM calls. Empty is the legal "unset" state
    /// (spec 11 R4/A7): no concrete model is baked in — the
    /// session-creation and resume chains resolve a model explicitly and
    /// fail loudly when none is configured.
    pub model_id: String,
    /// Which provider implementation to dispatch to.
    pub provider_type: ProviderType,
    /// Optional system prompt prepended to every LLM request. When set it
    /// replaces the whole default base (the TS custom-profile semantics);
    /// leave `None` with [`Self::role_additional`] set to render the
    /// default base template with a profile role instead.
    pub system_prompt: Option<String>,
    /// The profile's role text, rendered into the default base template's
    /// `{{role_additional}}` slot when [`Self::system_prompt`] is `None`
    /// (the TS v2 `systemPromptTemplate` counterpart — a role rides the
    /// shared base, it does not replace it).
    pub role_additional: Option<String>,
    /// Maximum turns for this agent's loop.
    pub max_turns: u32,
    /// Permission mode for this agent's tool calls.
    pub permission_mode: PermissionMode,
    /// Maximum tool steps per turn (`None` = engine default).
    pub max_steps_per_turn: Option<u32>,
    /// Tool names this agent may use (`None` = all built-in tools).
    pub enabled_tools: Option<Vec<String>>,
    /// Tool names this agent may not use.
    pub disabled_tools: Option<Vec<String>>,
    /// Capabilities the agent requires (host-side check).
    pub capabilities_required: Option<Vec<String>>,
    /// Reasoning effort the model should invest per response.
    pub thinking_effort: Option<ThinkingEffort>,
    /// The profile name this agent was spawned under (B4 T8, D-55) — set
    /// by the subagent host at spawn and by the restart restore from the
    /// `subagent.registered` registry, so a resumed session's child agents
    /// keep their identity for re-adoption into a fresh host. `None` for
    /// directly-constructed agents.
    pub profile_name: Option<String>,
    /// Initial accumulated token usage to pre-populate the agent's
    /// [`AgentUsageService`](crate::usage::AgentUsageService). Used by
    /// [`SessionManager::resume_session`](crate::session::SessionManager::resume_session)
    /// to restore usage totals from a replayed wire log.
    pub initial_usage: Option<UsageInitialState>,
    /// The session's reverse-RPC slot for `approval.ask` and `question.ask`.
    ///
    /// When `Some`, the `AskUserQuestion` tool is registered and resolves
    /// the slot's CURRENT callback per call (the transport fills the slot
    /// at subscribe — possibly after this agent was created); an empty slot
    /// at call time fails fast with a plain-text fallback directive. The
    /// permission gate's `Ask` result still issues `approval.ask` through
    /// the same slot. When `None` (headless directly-constructed agents),
    /// the tool is not registered and `Ask` falls back to headless
    /// auto-approval or blocking.
    pub reverse_rpc_slot: Option<crate::reverse_rpc::ReverseRpcSlot>,
    /// Optional interaction kernel (B4-a): binds the session's question
    /// gate to the `AskUserQuestion` tool so the gate is the pending
    /// source of truth and the new question/interaction RPCs can answer.
    pub interaction_kernel: Option<Arc<crate::session::interaction::InteractionKernel>>,
    /// Optional subagent host for the `Agent` and `AgentSwarm` tools.
    ///
    /// When `Some`, both tools are registered and the agent can spawn child
    /// agents. When `None` (headless/test mode), they are not registered.
    pub subagent_host: Option<Arc<dyn crate::subagent::SubagentHost>>,
    /// Optional web search provider for the `WebSearch` tool.
    pub web_search_provider: Option<Arc<dyn crate::tools::WebSearchProvider>>,
    /// Optional URL fetch provider for the `FetchURL` tool.
    pub fetch_url_provider: Option<Arc<dyn crate::tools::FetchUrlProvider>>,
    /// The session's event bus, baked into the agent's goal/skill tools so
    /// their mutations are announced on the wire (`event.goal.updated` /
    /// `event.skill.activated`). `None` (the default) keeps the tools
    /// silent — hosts without event plumbing.
    pub event_bus: Option<Arc<crate::session::events::EventBus>>,
    /// The OS providers (filesystem / process / search / glob) the agent's
    /// I/O-backed built-in tools execute through (zero-IO T13a).
    ///
    /// The default is the engine default (J3): the local `mirri-os`
    /// implementations, so every existing construction path (restore,
    /// subagents, tests) keeps its behavior. Hosts that want to inject their
    /// own providers (or test doubles) set this field before creating the
    /// agent; it flows into [`BuiltinToolServices::io`] and from there into
    /// each tool's constructor.
    ///
    /// [`BuiltinToolServices::io`]: crate::tools::BuiltinToolServices
    pub tool_io: crate::tools::ToolIo,
    /// Per-subagent timeout for the agent's `Agent` / `AgentSwarm` tools
    /// (B1 D-40/D-45). `None` = unbounded. Seeded by
    /// [`SessionScope::create_agent`](super::SessionScope::create_agent)
    /// from the session's [`TaskLifecycleSettings`] — the session is the
    /// source of truth; directly-constructed agent scopes keep unbounded.
    pub subagent_timeout: Option<std::time::Duration>,
    /// Per-call item cap for the agent's `AgentSwarm` tool (SDR-012).
    /// `None` keeps the engine default of 3; `Some(0)` means unlimited.
    /// Seeded by [`SessionScope::create_agent`](super::SessionScope::create_agent)
    /// from the session's [`TaskLifecycleSettings`] — the session is the
    /// source of truth; directly-constructed agent scopes keep the default.
    pub max_swarm_items: Option<u32>,
    /// Session-scoped shared approval store (SDR-003 / D-07). `None` = the
    /// agent's [`PermissionManager`](crate::permission::PermissionManager)
    /// owns a private store (directly-constructed test scopes);
    /// [`SessionScope::create_agent`](super::SessionScope::create_agent)
    /// always injects the session's, so every agent of one session shares
    /// one "approve for session" memory.
    pub session_approvals: Option<Arc<SessionApprovals>>,
    /// User-configured permission rules (`[[permission.rules]]` in the
    /// mirri home config.toml, D-34). The rules channel the
    /// [`AgentScope`](super::AgentScope) builds its permission manager
    /// from — seeded by
    /// [`SessionScope::create_agent`](super::SessionScope::create_agent)
    /// from the session's current user-rule set (the session is the source
    /// of truth; directly-constructed agent scopes keep none).
    pub permission_rules: Vec<PermissionRule>,
}

impl Default for AgentScopeConfig {
    fn default() -> Self {
        Self {
            // No built-in model (spec 11 R4): consumers that need a model
            // must resolve one through the explicit chain and error when
            // nothing is configured.
            model_id: String::new(),
            provider_type: ProviderType::default(),
            system_prompt: None,
            role_additional: None,
            max_turns: 200,
            permission_mode: PermissionMode::default(),
            max_steps_per_turn: None,
            enabled_tools: None,
            disabled_tools: None,
            capabilities_required: None,
            thinking_effort: None,
            profile_name: None,
            initial_usage: None,
            reverse_rpc_slot: None,
            interaction_kernel: None,
            subagent_host: None,
            web_search_provider: None,
            fetch_url_provider: None,
            event_bus: None,
            tool_io: crate::tools::ToolIo::default(),
            subagent_timeout: None,
            max_swarm_items: None,
            session_approvals: None,
            permission_rules: Vec::new(),
        }
    }
}

impl std::fmt::Debug for AgentScopeConfig {
    /// Manual `Debug`: the reverse-RPC slot's callback is a trait object
    /// with no `Debug` impl, so print only whether the slot is present.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentScopeConfig")
            .field("model_id", &self.model_id)
            .field("provider_type", &self.provider_type)
            .field("system_prompt", &self.system_prompt)
            .field("max_turns", &self.max_turns)
            .field("permission_mode", &self.permission_mode)
            .field("max_steps_per_turn", &self.max_steps_per_turn)
            .field("enabled_tools", &self.enabled_tools)
            .field("disabled_tools", &self.disabled_tools)
            .field("capabilities_required", &self.capabilities_required)
            .field("thinking_effort", &self.thinking_effort)
            .field("initial_usage", &self.initial_usage)
            .field("reverse_rpc_slot", &self.reverse_rpc_slot.is_some())
            .field("interaction_kernel", &self.interaction_kernel.is_some())
            .field("subagent_host", &self.subagent_host.is_some())
            .field("web_search_provider", &self.web_search_provider.is_some())
            .field("fetch_url_provider", &self.fetch_url_provider.is_some())
            .field("event_bus", &self.event_bus.is_some())
            .field("tool_io", &self.tool_io)
            .field("subagent_timeout", &self.subagent_timeout)
            .field("max_swarm_items", &self.max_swarm_items)
            .field("session_approvals", &self.session_approvals.is_some())
            .field("permission_rules", &self.permission_rules)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// SessionMetadata
// ---------------------------------------------------------------------------

/// Descriptive metadata for a session.
///
/// Stored behind a `RwLock` in [`SessionScope`](super::SessionScope) so it can
/// be updated (e.g. title renamed, tags added) without recreating the session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    /// Human-readable session title.
    pub title: String,
    /// Whether `title` was set by the user (`session.create` with a title or
    /// `session.rename`) rather than derived from the first prompt (TS
    /// `isCustomTitle` parity: the send path only auto-titles while this is
    /// `false`, so a user rename survives later prompts).
    pub is_custom_title: bool,
    /// When the session was created (UTC).
    pub created_at: DateTime<Utc>,
    /// When the session was last modified (UTC).
    pub updated_at: DateTime<Utc>,
    /// Free-form tags for categorization and filtering.
    pub tags: HashSet<String>,
    /// Whether the session is archived (TS parity: persisted in `state.json`
    /// and excluded from default listings). Live sessions are `false` unless
    /// they were resumed from an archived state without a `session.restore`
    /// (which is what clears the flag).
    pub archived: bool,
    /// The working directory the session was rooted at (TS parity: persisted
    /// as `workDir` in `state.json`). Seeded from the workspace `cwd` at
    /// session creation; `None` only for metadata built outside a workspace
    /// (tests) or loaded from a state file that predates the field.
    pub work_dir: Option<String>,
    /// The most recent user prompt sent in the session (TS parity:
    /// `lastPrompt`, written by the send path — TS `updatePromptMetadata`).
    /// Persisted in `state.json` so a load-mutate-save cycle (e.g. the
    /// archive flag or the turn-settle recency bump) keeps it instead of
    /// erasing it back to `None` (ADR-009).
    pub last_prompt: Option<String>,
    /// The session this one was forked from (TS `forkedFrom`): minted by
    /// the fork pipeline, persisted in `state.json`, and read back so a
    /// resume keeps the lineage.
    pub forked_from: Option<String>,
    /// Root session id of the fork chain — the provider prompt-cache key
    /// lineage (TS parity planned). Minted by the fork pipeline: the source's
    /// root when the source is itself a fork, else the source's own id;
    /// `None` on fresh sessions and legacy states predating the field.
    pub cache_root_id: Option<String>,
    /// Free-form custom metadata (TS `custom`): a JSON object carried
    /// verbatim in `state.json`. The fork pipeline seeds it from the
    /// source's custom dict merged with the caller-supplied fork metadata
    /// (the v2 `forkCustomMetadata` semantics).
    pub custom: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Default for SessionMetadata {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            title: "Untitled Session".to_string(),
            is_custom_title: false,
            created_at: now,
            updated_at: now,
            tags: HashSet::new(),
            archived: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_have_sensible_defaults_when_creating_app_scope_config() {
        let config = AppScopeConfig::default();
        // No built-in model (spec 11 R4): the default is the legal
        // "unset" state; see should_return_empty_model_id_for_default_scope_config.
        assert!(config.default_model.is_empty());
        assert!(config.default_max_turns > 0);
        assert!(config.request_timeout_secs > 0);
        assert!(config.max_concurrent_tools > 0);
    }

    #[test]
    fn should_default_to_untrusted_with_no_extra_directories_when_creating_workspace_scope_config()
    {
        let config = WorkspaceScopeConfig::default();
        assert_eq!(config.trust_level, WorkspaceTrustLevel::Untrusted);
        assert!(config.extra_directories.is_empty());
    }

    #[test]
    fn should_default_to_anthropic_with_no_system_prompt_when_creating_agent_scope_config() {
        let config = AgentScopeConfig::default();
        assert_eq!(config.provider_type, ProviderType::Anthropic);
        assert!(config.system_prompt.is_none());
    }

    /// Given the zero-built-in-model policy (spec 11 R4/A7), when the
    /// default scope configs are constructed, then no concrete model id is
    /// baked in anywhere — an empty model id is the legal "unset" state,
    /// and the request-time resolution chain is what fails loudly.
    #[test]
    fn should_return_empty_model_id_for_default_scope_config() {
        assert!(
            AgentScopeConfig::default().model_id.is_empty(),
            "the agent scope default must not bake in a concrete model"
        );
        assert!(
            AppScopeConfig::default().default_model.is_empty(),
            "the app scope default must not bake in a concrete model"
        );
        assert!(
            crate::config::ServerConfig::default()
                .default_model
                .is_none(),
            "the server config default must not bake in a concrete model"
        );
    }

    #[test]
    fn should_round_trip_kimi_as_lowercase_string_through_serde() {
        // Given the Kimi provider type
        let serialized = serde_json::to_value(ProviderType::Kimi).expect("kimi serializes");

        // When it round-trips through the wire form
        let deserialized: ProviderType =
            serde_json::from_value(serialized.clone()).expect("kimi deserializes");

        // Then the wire form is the lowercase "kimi" and the variant is
        // preserved
        assert_eq!(serialized, serde_json::json!("kimi"));
        assert_eq!(deserialized, ProviderType::Kimi);
    }

    #[test]
    fn should_default_to_untitled_with_empty_tags_when_creating_session_metadata() {
        let metadata = SessionMetadata::default();
        assert_eq!(metadata.title, "Untitled Session");
        assert!(metadata.tags.is_empty());
        assert!(metadata.created_at <= metadata.updated_at);
    }

    /// Given the task-lifecycle defaults, when a session is assembled
    /// without an explicit `[task]`/`[subagent]` config, then the TS
    /// defaults apply: a 2h subagent timeout, unbounded capacity, a 5s kill
    /// grace, close-time convergence on, and a 5-minute heartbeat window.
    #[test]
    fn should_default_to_the_ts_task_lifecycle_values_when_creating_task_lifecycle_settings() {
        use std::time::Duration;
        let settings = super::TaskLifecycleSettings::default();
        assert_eq!(
            settings.subagent_timeout,
            Some(Duration::from_secs(2 * 60 * 60)),
            "TS default: 2h subagent timeout"
        );
        assert_eq!(settings.max_running_tasks, 0, "0 = unbounded");
        assert_eq!(settings.kill_grace, Duration::from_millis(5_000));
        assert!(!settings.keep_alive_on_exit);
        assert_eq!(
            settings.heartbeat_interval,
            Some(Duration::from_secs(300)),
            "TS default: 5-minute stale window"
        );
    }
}
