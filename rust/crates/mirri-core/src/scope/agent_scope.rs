//! AgentScope — the innermost scope, holding all engine-layer components.
//!
//! Each `AgentScope` owns the shared engine state for one agent loop:
//! [`ContextMemory`], [`ToolManager`], [`PermissionManager`], and handles to
//! the session-scoped [`TodoList`], [`GoalManager`], [`PlanManager`], and
//! [`CollaborationManager`] services. All are wrapped in `Arc` so they can be
//! shared across tasks (e.g. the loop engine, tool scheduler, and hooks)
//! without copying.

use std::fmt;
use std::sync::{Arc, Mutex, RwLock};

use mirri_collaboration::CollaborationManager;

use crate::context_memory::ContextMemory;
use crate::cron::CronManager;
use crate::goal::GoalManager;
use crate::hook_engine::HookEngine;
use crate::permission::{PermissionManager, PermissionServices};
use crate::plan::PlanManager;
use crate::skill::SkillCatalog;
use crate::swarm::SwarmModeService;
use crate::task::TaskManager;
use crate::todo::TodoList;
use crate::tool_registry::ToolManager;
use crate::tool_select::ToolSelectService;
use crate::tools::{BuiltinToolServices, WorkspacePaths, register_builtin_tools};
use crate::usage::AgentUsageService;

use super::config::AgentScopeConfig;
use super::ids::AgentId;
use crate::permission::PermissionMode;
use crate::profile::ThinkingEffort;

/// The innermost scope — holds all engine-layer components for one agent.
///
/// All shared components are behind `Arc` so they can be cloned and passed to
/// async tasks (loop engine, tool scheduler, hooks) without lifetime juggling.
/// `ContextMemory` uses `std::sync::Mutex` because it is never held across an
/// `.await` point. `ToolManager` uses `std::sync::RwLock` because it needs to
/// support runtime mutation (MCP tool registration/removal) while also being
/// readable from the loop engine.
///
/// The [`TodoList`], [`TaskManager`], [`CronManager`], [`GoalManager`],
/// [`PlanManager`], and [`CollaborationManager`] services are session-scoped:
/// a session creates one instance of each and hands the same `Arc` to every
/// agent it spawns, so subagents share one todo list, background-task set,
/// cron schedule, goal, plan-mode state, and collaboration-manager state.
/// The built-in tools are bound to these same services via
/// [`register_builtin_tools`].
pub struct AgentScope {
    /// Unique identifier for this agent within its session.
    id: AgentId,
    /// Canonical message history store (Task 16).
    context_memory: Arc<Mutex<ContextMemory>>,
    /// Tool registry and scheduler (Task 17).
    ///
    /// Wrapped in `RwLock` to allow dynamic tool registration (MCP
    /// connect/disconnect) at runtime. Read access is used by the loop engine
    /// (`loop_tools`, `find_tool`); write access is used by
    /// `register_mcp_tool`/`remove_mcp_tools_by_server`/`set_active_tools`.
    ///
    /// The lock is [`parking_lot::RwLock`] (B4 T3, SDR-B4-01): unlike
    /// `std::sync::RwLock`, a parking_lot read guard is `Send` when the
    /// guarded data is, so a future can hold the guard across an `.await`
    /// — the precondition for driving a subagent turn as a plain async
    /// tree instead of `spawn_blocking` + `block_on`. Construction-time
    /// writes stay synchronous and parking_lot's API is identical minus
    /// poisoning (guards never fail to acquire).
    ///
    /// The 21 built-in tools (Bash, Read, Write, Edit, Glob, Grep, TodoList,
    /// TaskList, TaskStop, TaskOutput, CronCreate, CronDelete, CronList,
    /// CreateGoal, GetGoal, SetGoalBudget, UpdateGoal, EnterPlanMode,
    /// ExitPlanMode, select_tools, Skill) are registered here at construction;
    /// they only reach the LLM loop once enabled via `set_active_tools`.
    tool_manager: Arc<parking_lot::RwLock<ToolManager>>,
    /// Permission gate and policy chain (Task 18).
    permission_manager: Arc<PermissionManager>,
    /// Session-scoped todo list (shared with every agent in the session).
    todo_list: Arc<TodoList>,
    /// Session-scoped background-task manager (shared with every agent in the
    /// session).
    tasks: Arc<TaskManager>,
    /// Session-scoped cron scheduler (shared with every agent in the
    /// session).
    cron: Arc<CronManager>,
    /// Session-scoped goal state (shared with every agent in the session).
    goal: Arc<GoalManager>,
    /// Session-scoped plan-mode state (shared with every agent in the
    /// session).
    plan: Arc<PlanManager>,
    /// Session-scoped multi-agent collaboration manager (shared with every
    /// agent in the session). Backs the `collaboration_*` tools.
    collaboration: Arc<CollaborationManager>,
    /// Session-scoped external-command hook engine (shared with every agent
    /// in the session). Feeds the observation-only `PermissionResult` fire
    /// from `PermissionManager::before_tool_call` (v2 parity: hooks never
    /// gate permission decisions).
    hook_engine: Arc<HookEngine>,
    /// Session-scoped swarm-mode state service (shared with every agent in
    /// the session). Consulted by the swarm permission policies; behind a
    /// `Mutex` because the service's mutations take `&mut self`.
    swarm_mode: Arc<Mutex<SwarmModeService>>,
    /// Per-agent token usage accounting service.
    ///
    /// Accumulates token usage from LLM calls. Pre-populated with
    /// `config.initial_usage` when the agent is created from a resumed
    /// session so that cumulative totals reflect prior consumption.
    usage_service: Arc<Mutex<AgentUsageService>>,
    /// The workspace-scoped skill catalog (shared with every agent in the
    /// workspace). Backs the [`SkillTool`](crate::tools::SkillTool) so it
    /// can resolve and read discovered `SKILL.md` files.
    skill_catalog: Arc<SkillCatalog>,
    /// The per-agent tool-select service backing
    /// [`SelectToolsTool`](crate::tools::SelectToolsTool). The host can
    /// configure progressive disclosure (deferred / selective mode) and
    /// register deferred tools via this handle.
    tool_select: Arc<Mutex<ToolSelectService>>,
    /// Agent-level configuration (interior-mutable so runtime setters like
    /// `set_model` / `set_thinking` can update it without rebuilding the
    /// scope).
    config: Arc<RwLock<AgentScopeConfig>>,
    /// The profile-resolved system prompt *before* the session-scope
    /// workspace suffixes (AGENTS.md content, working-directory tree) were
    /// appended at agent creation. Kept so
    /// [`SessionScope::refresh_agent_system_prompt`] can rebuild the full
    /// prompt from the current workspace state after a compaction without
    /// re-resolving the profile. `None` for directly-constructed scopes that
    /// never went through session creation (refresh is then a no-op).
    ///
    /// [`SessionScope::refresh_agent_system_prompt`]:
    ///     super::session_scope::SessionScope::refresh_agent_system_prompt
    base_system_prompt: RwLock<Option<String>>,
}

impl AgentScope {
    /// Create a new agent scope with fresh engine components.
    ///
    /// The [`ContextMemory`] and [`PermissionManager`] are initialized empty
    /// and the built-in tools are registered into the fresh [`ToolManager`].
    /// The [`TodoList`], [`TaskManager`], [`CronManager`], [`GoalManager`],
    /// [`PlanManager`], [`HookEngine`], [`SwarmModeService`], and
    /// [`CollaborationManager`] services are session-scoped: pass the
    /// session's shared instances (see
    /// [`SessionScope::create_agent`](super::SessionScope::create_agent)).
    /// The permission mode is taken from `config`, and the session services
    /// are wired into the permission policy chain (plan-mode guard, pre-tool
    /// hooks, swarm policies). When `config.enabled_tools` is set, only
    /// those tools are activated for the agent. The `workspace_paths`
    /// confines the agent's file tools (Read/ReadMediaFile/Write/Edit) to
    /// the session's shared workspace sandbox (root plus additional dirs);
    /// `None` disables file-tool path validation. The `wire_recorder` is the
    /// session's wire-log handle — the goal/plan tools persist their
    /// mutations through it (an empty-slot handle records nothing).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: AgentId,
        config: AgentScopeConfig,
        session_id: String,
        todo_list: Arc<TodoList>,
        tasks: Arc<TaskManager>,
        cron: Arc<CronManager>,
        goal: Arc<GoalManager>,
        plan: Arc<PlanManager>,
        collaboration: Arc<CollaborationManager>,
        hook_engine: Arc<HookEngine>,
        swarm_mode: Arc<Mutex<SwarmModeService>>,
        skill_catalog: Arc<SkillCatalog>,
        workspace_paths: Option<Arc<WorkspacePaths>>,
        wire_recorder: crate::scope::SessionWireLog,
    ) -> Self {
        // Build the shared tool manager FIRST (B1 D-50): the Agent tool's
        // background gate consults the live activation state, so it must
        // capture a handle to the same manager the tools register into.
        // The gate holds a Weak handle — a strong Arc would cycle
        // (manager → AgentTool → gate → manager) and leak per agent.
        let tool_manager = Arc::new(parking_lot::RwLock::new(ToolManager::new()));
        let tool_select = Arc::new(Mutex::new(ToolSelectService::new()));
        let background_gate: Arc<dyn Fn() -> bool + Send + Sync> = {
            let weak = Arc::downgrade(&tool_manager);
            Arc::new(move || {
                weak.upgrade()
                    .map(|manager| {
                        ["TaskList", "TaskOutput", "TaskStop"]
                            .iter()
                            .all(|name| manager.read().is_tool_active(name))
                    })
                    .unwrap_or(false)
            })
        };
        register_builtin_tools(
            &mut tool_manager.write(),
            &BuiltinToolServices {
                todo_list: Arc::clone(&todo_list),
                tasks: Arc::clone(&tasks),
                goal: Arc::clone(&goal),
                plan: Arc::clone(&plan),
                cron: Arc::clone(&cron),
                collaboration: Arc::clone(&collaboration),
                skill_catalog: Arc::clone(&skill_catalog),
                tool_select: Arc::clone(&tool_select),
                reverse_rpc_slot: config.reverse_rpc_slot.clone(),
                interaction_kernel: config.interaction_kernel.clone(),
                subagent_host: config.subagent_host.clone(),
                web_search_provider: config.web_search_provider.clone(),
                fetch_url_provider: config.fetch_url_provider.clone(),
                session_id: session_id.clone(),
                agent_id: id.to_string(),
                workspace_paths: workspace_paths.clone(),
                wire_recorder,
                event_bus: config.event_bus.clone(),
                io: config.tool_io.clone(),
                background_gate,
                subagent_timeout: config.subagent_timeout,
                max_swarm_items: config.max_swarm_items,
            },
        );
        match config.enabled_tools.clone() {
            Some(enabled_tools) => {
                tool_manager.write().set_active_tools(enabled_tools);
            }
            None => {
                // No explicit allowlist means all registered builtin/user
                // tools are enabled. Without this, ToolManager defaults to an
                // empty enabled_tools vec and recompute_table() produces an
                // empty tool table — the LLM would see zero tools.
                tool_manager.write().enable_all_registered();
            }
        }
        let permission_services = PermissionServices {
            hook_engine: Some(Arc::clone(&hook_engine)),
            plan: Some(Arc::clone(&plan)),
            swarm_mode: Some(Arc::clone(&swarm_mode)),
            // The boundary policy classifies tool paths against the same
            // session sandbox the file tools use (plan D2).
            workspace_paths,
        };
        // Session agents share the session's approval store (D-07): when
        // the config carries it, the manager is built over that store so a
        // session-scoped approval recorded through one agent reaches every
        // other agent of the session. Without a store (directly-constructed
        // scopes) the manager self-owns one. The user-config rules (D-34)
        // ride the config's permission_rules channel.
        let permission_rules = config.permission_rules.clone();
        let permission_manager = Arc::new(match config.session_approvals.clone() {
            Some(store) => PermissionManager::with_shared_approvals(
                config.permission_mode,
                permission_rules,
                permission_services,
                store,
            ),
            None => PermissionManager::with_services(
                config.permission_mode,
                permission_rules,
                permission_services,
            ),
        });

        // Pre-populate the usage service with initial values when the agent is
        // created from a resumed session, so cumulative totals reflect the
        // usage already consumed before the restart.
        let mut usage_service = AgentUsageService::new();
        if let Some(initial) = &config.initial_usage {
            // SDR-004: seed the cold-start baseline BEFORE the resume record
            // so the accumulated totals never capture themselves as the
            // session's first call.
            if let Some(first_call) = &initial.first_call {
                usage_service.set_first_call(first_call.clone());
            }
            usage_service.record(
                &config.model_id,
                crate::usage::TokenUsage {
                    input_tokens: initial.total_input_tokens,
                    output_tokens: initial.total_output_tokens,
                    cache_read_tokens: initial.total_cache_read_tokens,
                    cache_creation_tokens: initial.total_cache_creation_tokens,
                },
                Some("resume".to_string()),
            );
        }

        Self {
            id,
            context_memory: Arc::new(Mutex::new(ContextMemory::new())),
            tool_manager,
            permission_manager,
            todo_list,
            tasks,
            cron,
            goal,
            plan,
            collaboration,
            hook_engine,
            swarm_mode,
            usage_service: Arc::new(Mutex::new(usage_service)),
            skill_catalog,
            tool_select,
            config: Arc::new(RwLock::new(config)),
            base_system_prompt: RwLock::new(None),
        }
    }

    /// This agent's unique identifier.
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// Shared handle to the context memory.
    ///
    /// Cloning the `Arc` is cheap; the underlying `Mutex<ContextMemory>` is
    /// shared by all holders.
    pub fn context_memory(&self) -> Arc<Mutex<ContextMemory>> {
        Arc::clone(&self.context_memory)
    }

    /// Shared handle to the tool manager.
    ///
    /// Returns `Arc<RwLock<ToolManager>>` so callers can obtain either a read
    /// guard (for `loop_tools`, `find_tool`, etc.) or a write guard (for
    /// `register_mcp_tool`, `remove_mcp_tools_by_server`, `set_active_tools`).
    pub fn tool_manager(&self) -> Arc<parking_lot::RwLock<ToolManager>> {
        Arc::clone(&self.tool_manager)
    }

    /// Shared handle to the permission manager.
    pub fn permission_manager(&self) -> Arc<PermissionManager> {
        Arc::clone(&self.permission_manager)
    }

    /// Shared handle to the session-scoped todo list.
    pub fn todo_list(&self) -> Arc<TodoList> {
        Arc::clone(&self.todo_list)
    }

    /// Shared handle to the session-scoped background-task manager.
    pub fn tasks(&self) -> Arc<TaskManager> {
        Arc::clone(&self.tasks)
    }

    /// Shared handle to the session-scoped cron scheduler.
    pub fn cron(&self) -> Arc<CronManager> {
        Arc::clone(&self.cron)
    }

    /// Shared handle to the session-scoped goal manager.
    pub fn goal(&self) -> Arc<GoalManager> {
        Arc::clone(&self.goal)
    }

    /// Shared handle to the session-scoped plan manager.
    pub fn plan(&self) -> Arc<PlanManager> {
        Arc::clone(&self.plan)
    }

    /// Shared handle to the session-scoped collaboration manager.
    ///
    /// Every agent in the session shares one [`CollaborationManager`], so a
    /// collaboration started by one agent (via the `collaboration_start`
    /// tool) is observable and steerable by every other agent in the
    /// session.
    pub fn collaboration(&self) -> Arc<CollaborationManager> {
        Arc::clone(&self.collaboration)
    }

    /// Shared handle to the session-scoped external-command hook engine.
    pub fn hook_engine(&self) -> Arc<HookEngine> {
        Arc::clone(&self.hook_engine)
    }

    /// Shared handle to the session-scoped swarm-mode state service.
    pub fn swarm_mode(&self) -> Arc<Mutex<SwarmModeService>> {
        Arc::clone(&self.swarm_mode)
    }

    /// Shared handle to the per-agent token usage accounting service.
    ///
    /// The service is behind `Arc<Mutex<…>>` so it can be updated from the
    /// loop engine (via `record`) and read from the host (via `status`)
    /// without lifetime juggling.
    pub fn usage_service(&self) -> Arc<Mutex<AgentUsageService>> {
        Arc::clone(&self.usage_service)
    }

    /// Shared handle to the workspace-scoped skill catalog.
    ///
    /// The catalog indexes discovered `SKILL.md` files and backs the
    /// [`SkillTool`](crate::tools::SkillTool). Skills are workspace-scoped:
    /// every session and agent in the workspace shares the same catalog.
    pub fn skill_catalog(&self) -> Arc<SkillCatalog> {
        Arc::clone(&self.skill_catalog)
    }

    /// Shared handle to the per-agent tool-select service.
    ///
    /// The host uses this to configure progressive tool disclosure (set
    /// `Deferred` or `Selective` mode, register deferred tools) before
    /// starting a turn. When disclosure is not enabled (the default `All`
    /// mode), [`SelectToolsTool`](crate::tools::SelectToolsTool) returns a
    /// clear "not configured" message instead of erroring.
    pub fn tool_select(&self) -> Arc<Mutex<ToolSelectService>> {
        Arc::clone(&self.tool_select)
    }

    /// Agent-level configuration.
    pub fn config(&self) -> Arc<RwLock<AgentScopeConfig>> {
        Arc::clone(&self.config)
    }

    /// The session's reverse-RPC slot this agent resolves questions and
    /// approvals through (the late-subscribe fix): the slot's CURRENT
    /// callback is read at call time, so a transport subscribing after
    /// this agent was created still reaches its tools. `None` only for
    /// agents constructed outside a session (directly-built test scopes).
    pub fn session_reverse_rpc_slot(&self) -> crate::reverse_rpc::ReverseRpcSlot {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .reverse_rpc_slot
            .clone()
            .unwrap_or_else(|| Arc::new(RwLock::new(None)))
    }

    /// Update the agent's model ID at runtime.
    pub fn set_model(&self, model: String) {
        self.config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .model_id = model;
    }

    /// The agent's current model id (the model *alias* as configured — the
    /// identifier usage accounting keys by, TS parity, N4).
    pub fn model_id(&self) -> String {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id
            .clone()
    }

    /// Update the agent's thinking effort at runtime.
    pub fn set_thinking(&self, effort: Option<ThinkingEffort>) {
        self.config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .thinking_effort = effort;
    }

    /// Update the agent's permission mode at runtime.
    pub fn set_permission_mode(&self, mode: PermissionMode) {
        self.permission_manager.set_mode(mode);
        self.config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .permission_mode = mode;
    }

    /// The profile-resolved system prompt captured before the session-scope
    /// workspace suffixes (AGENTS.md, directory tree) were appended.
    ///
    /// `None` when the scope was constructed directly (tests, subagents)
    /// rather than through session agent creation — those agents have
    /// nothing to refresh against.
    pub fn base_system_prompt(&self) -> Option<String> {
        self.base_system_prompt
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record the pre-suffix system prompt (session agent creation only —
    /// see [`base_system_prompt`](Self::base_system_prompt)).
    pub fn set_base_system_prompt(&self, base: Option<String>) {
        *self
            .base_system_prompt
            .write()
            .unwrap_or_else(|e| e.into_inner()) = base;
    }

    /// Replace the assembled system prompt in the agent config.
    ///
    /// The next LLM built from this config (per turn / per compaction
    /// summarizer) picks the new prompt up.
    pub fn set_system_prompt(&self, prompt: Option<String>) {
        self.config
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .system_prompt = prompt;
    }
}

impl fmt::Debug for AgentScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("AgentScope")
            .field("id", &self.id)
            .field("config", &*config)
            .finish_non_exhaustive()
    }
}
