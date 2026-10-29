//! Built-in agent tools.
//!
//! These are the core tools registered into every
//! [`ToolManager`](crate::tool_registry::ToolManager) when an agent is
//! created. They cover the canonical agent operations:
//!
//! - OS access: [`BashTool`], [`ReadTool`], [`WriteTool`], [`EditTool`],
//!   [`GlobTool`], [`GrepTool`]
//! - Session state: [`TodoListTool`] (session-scoped todo list),
//!   [`TaskListTool`] / [`TaskStopTool`] / [`TaskOutputTool`] (session-scoped
//!   background-task tracking)
//!
//! Stateful tools receive their backing services at construction time via
//! [`BuiltinToolServices`], so the same session-scoped services are shared
//! between the agent's scope and its tools.

pub mod agent_swarm_tool;
pub mod agent_tool;
pub mod ask_user;
pub mod bash;
pub mod collaboration;
pub mod create_goal;
pub mod cron_create;
pub mod cron_delete;
pub mod cron_list;
pub mod edit;
pub mod enter_plan_mode;
pub mod exit_plan_mode;
pub mod fetch_url;
pub mod file_mutation_guard;
pub mod get_goal;
pub mod glob;
pub mod grep;
pub mod net_class;
pub mod path_security;
pub mod powershell;
pub mod read;
pub mod read_media;
pub mod select_tools;
pub mod set_goal_budget;
pub mod skill;
pub mod task_list;
pub mod task_output;
pub mod task_stop;
pub mod todo_list;
pub mod update_goal;
pub mod web_providers;
pub mod web_search;
pub mod write;

pub use agent_swarm_tool::AgentSwarmTool;
pub use agent_tool::AgentTool;
pub use ask_user::AskUserQuestionTool;
pub use bash::BashTool;
pub use collaboration::{
    COLLABORATION_TOOL_NAMES, CollaborationEscalateTool, CollaborationGetMessagesTool,
    CollaborationGetStateTool, CollaborationInterveneTool, CollaborationSendMessageTool,
    CollaborationStartTool, CollaborationTerminateTool,
};
pub use create_goal::CreateGoalTool;
pub use cron_create::CronCreateTool;
pub use cron_delete::CronDeleteTool;
pub use cron_list::CronListTool;
pub use edit::EditTool;
pub use enter_plan_mode::EnterPlanModeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use fetch_url::FetchUrlTool;
pub use get_goal::GetGoalTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use path_security::{
    PathClassification, WorkspacePathError, WorkspacePaths, validate_workspace_path,
};
pub(crate) use path_security::{classify_tool_path, escape_tool_error};
#[cfg(windows)]
pub use powershell::PowerShellTool;
pub use read::ReadTool;
pub use read_media::ReadMediaFileTool;
pub use select_tools::SelectToolsTool;
pub use set_goal_budget::SetGoalBudgetTool;
pub use skill::SkillTool;
pub use task_list::TaskListTool;
pub use task_output::TaskOutputTool;
pub use task_stop::TaskStopTool;
pub use todo_list::TodoListTool;
pub use update_goal::UpdateGoalTool;
pub use web_providers::{
    FetchContentKind, FetchUrlProvider, FetchUrlResult, WebSearchProvider, WebSearchResult,
};
pub use web_search::WebSearchTool;
pub use write::WriteTool;

use std::sync::{Arc, Mutex};

use crate::cron::CronManager;
use crate::goal::GoalManager;
use crate::plan::PlanManager;
use crate::reverse_rpc::ReverseRpcSlot;
use crate::scope::SessionWireLog;
use crate::session::events::EventBus;
use crate::skill::SkillCatalog;
use crate::task::TaskManager;
use crate::todo::TodoList;
use crate::tool::{ToolErrorResult, ToolExecution};
use crate::tool_registry::ToolManager;
use crate::tool_select::ToolSelectService;

use mirri_collaboration::CollaborationManager;

/// The OS provider set the built-in tools use for their I/O (zero-IO T13a).
///
/// Every built-in tool that touches the host — Read/ReadMediaFile/Write/Edit
/// (filesystem), Bash and the `!` shell-command service (process), Glob, Grep
/// — receives its provider from this struct instead of constructing a local
/// implementation inline. The [`Default`] is the engine default (J3): the
/// local `mirri-os` implementations, so every existing construction path
/// (including [`AgentScopeConfig::default`](crate::scope::AgentScopeConfig))
/// keeps its behavior; hosts and tests override via
/// [`AgentScopeConfig::tool_io`](crate::scope::AgentScopeConfig) or by
/// building a `ToolIo` by hand.
///
/// The provider traits are the object-safe `Dyn*` mirrors of the mirri-os
/// RPITIT traits (`dyn FileSystem` is not possible with `async fn` in
/// trait), so test doubles implement the `Dyn*` traits directly.
#[derive(Clone)]
pub struct ToolIo {
    /// Filesystem provider for the file tools and path validation.
    pub fs: Arc<dyn mirri_os::DynFileSystem>,
    /// Process provider for the Bash tool and the `!` shell-command service.
    pub process: Arc<dyn mirri_os::DynOsProcess>,
    /// Content-search provider for the Grep tool.
    pub search: Arc<dyn mirri_os::DynContentSearch>,
    /// Glob provider for the Glob tool.
    pub glob: Arc<dyn mirri_os::DynGlobMatch>,
}

impl Default for ToolIo {
    fn default() -> Self {
        Self {
            fs: Arc::new(mirri_os::LocalFs::new()),
            process: Arc::new(mirri_os::LocalProcess::new()),
            search: Arc::new(mirri_os::RipgrepSearch::new()),
            glob: Arc::new(mirri_os::LocalGlob::new()),
        }
    }
}

impl std::fmt::Debug for ToolIo {
    /// Manual `Debug`: the provider trait objects have no `Debug` impls.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolIo").finish_non_exhaustive()
    }
}

/// The session-scoped services that stateful built-in tools are bound to.
///
/// [`register_builtin_tools`] clones these `Arc`s into the stateless tool
/// structs, so a tool invocation observes exactly the same state as the
/// owning [`AgentScope`](crate::scope::AgentScope).
#[derive(Clone)]
pub struct BuiltinToolServices {
    /// The session-scoped todo list backing [`TodoListTool`].
    pub todo_list: Arc<TodoList>,
    /// The session-scoped task manager backing [`TaskListTool`],
    /// [`TaskStopTool`], and [`TaskOutputTool`].
    pub tasks: Arc<TaskManager>,
    /// The session-scoped goal state backing the `Goal` tools.
    pub goal: Arc<GoalManager>,
    /// The session-scoped plan-mode state backing the `Plan` tools.
    pub plan: Arc<PlanManager>,
    /// The session-scoped cron scheduler backing the `Cron` tools.
    pub cron: Arc<CronManager>,
    /// The session-scoped collaboration manager backing the `Collaboration` tools.
    pub collaboration: Arc<CollaborationManager>,
    /// The session-scoped skill catalog backing [`SkillTool`].
    pub skill_catalog: Arc<SkillCatalog>,
    /// The per-agent tool-select service backing [`SelectToolsTool`].
    pub tool_select: Arc<Mutex<ToolSelectService>>,
    /// The session's reverse-RPC slot for the `AskUserQuestion` tool.
    ///
    /// The slot is the session's mutable, shared callback holder — the
    /// transport installs the concrete callback when a client subscribes,
    /// which can happen after the agent was created. The tool resolves the
    /// current value at call time, so it is registered whenever a slot
    /// exists (session-bound agents) and an empty slot at call time fails
    /// fast with a plain-text fallback directive. `None` (directly-
    /// constructed headless agents, tests) keeps the tool unregistered.
    pub reverse_rpc_slot: Option<ReverseRpcSlot>,
    /// Optional interaction kernel for the `AskUserQuestion` tool (B4-a).
    ///
    /// When `Some`, the tool's blocking questions register with the
    /// session's question gate (the pending source of truth), so
    /// `question.answer` / `interaction.respond` can answer them. When
    /// `None`, the tool keeps the pure reverse-RPC ask.
    pub interaction_kernel: Option<Arc<crate::session::interaction::InteractionKernel>>,
    /// Optional subagent host for the `Agent` and `AgentSwarm` tools.
    ///
    /// When `Some`, both tools are registered. When `None` (headless/test
    /// mode), they are not registered.
    pub subagent_host: Option<Arc<dyn crate::subagent::SubagentHost>>,
    /// Optional web search provider for the `WebSearch` tool.
    ///
    /// When `Some`, the tool is registered. When `None`, it is not.
    pub web_search_provider: Option<Arc<dyn WebSearchProvider>>,
    /// Optional URL fetch provider for the `FetchURL` tool.
    ///
    /// When `Some`, the tool is registered. When `None`, it is not.
    pub fetch_url_provider: Option<Arc<dyn FetchUrlProvider>>,
    /// The session ID, baked into [`CronCreateTool`] so the model never
    /// supplies it.
    pub session_id: String,
    /// The owning agent's id — the dispatcher attribution background task
    /// registrations carry (a subagent's bash task delivers its
    /// notification back to the subagent).
    pub agent_id: String,
    /// The session-shared workspace sandbox the file and search tools
    /// (Read/ReadMediaFile/Write/Edit/Glob/Grep) are confined to.
    ///
    /// `None` disables path validation (legacy hosts / unit tests); `Some`
    /// makes every file/search tool path resolve inside the workspace root
    /// or one of the sandbox's additional dirs, or fail with a
    /// path-traversal error (issue 15D, ADR-2). The handle is shared with
    /// the owning [`SessionScope`](crate::scope::SessionScope), so
    /// `session.addAdditionalDir` extends the sandbox for every agent in
    /// the session without rebuilding the tools.
    pub workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The session's wire-log recorder, shared by the goal/plan tools so
    /// their mutations are persisted (`goal.*` / `plan_mode.*` ops) to the
    /// session's `wire.jsonl`. The default (empty-slot) handle records
    /// nothing — test scopes without a wire log.
    pub wire_recorder: SessionWireLog,
    /// The session's event bus, shared by the goal/skill tools so their
    /// mutations are announced (`event.goal.updated` /
    /// `event.skill.activated`). `None` (test scopes, hosts without event
    /// plumbing) disables the announcements — the tools still work.
    pub event_bus: Option<Arc<EventBus>>,
    /// The OS providers (filesystem / process / search / glob) the I/O-backed
    /// built-in tools execute through (zero-IO T13a). Defaults to the local
    /// `mirri-os` implementations; tests and hosts inject doubles.
    pub io: ToolIo,
    /// Whether background subagent dispatch is available (B1 D-50):
    /// `true` only while TaskList, TaskOutput, and TaskStop are all
    /// active on the owning agent's tool manager. Evaluated per call —
    /// tool activation can change after construction.
    pub background_gate: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Per-subagent timeout for the `Agent` and `AgentSwarm` tools (B1
    /// D-40/D-45). `None` = unbounded (print mode / `timeoutMs = 0`).
    pub subagent_timeout: Option<std::time::Duration>,
    /// The question-delivery wait ceiling (ADR-008 / D-4): how long the
    /// `AskUserQuestion` delivery loop keeps retrying a broken channel
    /// before failing with the distinguishable "channel unavailable"
    /// error. Default 300s (5 minutes).
    pub question_channel_wait: std::time::Duration,
    /// Per-call item cap for the `AgentSwarm` tool (SDR-012): a single
    /// swarm dispatches at most this many members. `None` keeps the engine
    /// default of 3; `Some(0)` means unlimited. The tool re-resolves the
    /// `MIRRI_AGENT_SWARM_MAX_ITEMS` env override at call time on top of
    /// this value.
    pub max_swarm_items: Option<u32>,
}

impl std::fmt::Debug for BuiltinToolServices {
    /// Manual `Debug`: [`TaskManager`] has no `Debug` impl, so print service
    /// identities instead of internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinToolServices")
            .field("todo_list", &self.todo_list)
            .field("tasks", &Arc::as_ptr(&self.tasks))
            .field("goal", &self.goal)
            .field("plan", &self.plan)
            .field("cron", &self.cron)
            .field("collaboration", &Arc::as_ptr(&self.collaboration))
            .field("skill_catalog", &self.skill_catalog)
            .field("tool_select", &Arc::as_ptr(&self.tool_select))
            .field("reverse_rpc_slot", &self.reverse_rpc_slot.is_some())
            .field("interaction_kernel", &self.interaction_kernel.is_some())
            .field("subagent_host", &self.subagent_host.is_some())
            .field("web_search_provider", &self.web_search_provider.is_some())
            .field("fetch_url_provider", &self.fetch_url_provider.is_some())
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("workspace_paths", &self.workspace_paths)
            .field("wire_recorder", &self.wire_recorder)
            .field("event_bus", &self.event_bus.is_some())
            .field("io", &self.io)
            .field("background_gate", &"Arc<dyn Fn() -> bool>")
            .field("subagent_timeout", &self.subagent_timeout)
            .field("question_channel_wait", &self.question_channel_wait)
            .field("max_swarm_items", &self.max_swarm_items)
            .finish()
    }
}

/// Register every built-in tool into `tool_manager`.
///
/// Tools are registered by name into the builtin registry; they only become
/// visible to the LLM loop once the host enables them via
/// [`ToolManager::set_active_tools`](crate::tool_registry::ToolManager::set_active_tools).
pub fn register_builtin_tools(tool_manager: &mut ToolManager, services: &BuiltinToolServices) {
    tool_manager.register_builtin(Arc::new(BashTool::new(
        Arc::clone(&services.tasks),
        services.agent_id.clone(),
        services.session_id.clone(),
        Arc::clone(&services.io.process),
        // The session's workDir is the default command directory (audit
        // P0-1): the command runs there unless the model supplies `cwd`.
        services
            .workspace_paths
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf()),
    )));
    // The PowerShell tool is Windows-only (WO3-R4): the native shell face for
    // Windows hosts. Non-Windows hosts never see it; the static catalog still
    // carries its row unconditionally (ADR-WO3-5).
    #[cfg(windows)]
    tool_manager.register_builtin(Arc::new(PowerShellTool::new(
        Arc::clone(&services.tasks),
        services.agent_id.clone(),
        services.session_id.clone(),
        Arc::clone(&services.io.process),
        services
            .workspace_paths
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf()),
    )));
    tool_manager.register_builtin(Arc::new(ReadTool::new(
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(ReadMediaFileTool::new(
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(WriteTool::new(
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(EditTool::new(
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(GlobTool::new(
        Arc::clone(&services.io.glob),
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(GrepTool::new(
        Arc::clone(&services.io.search),
        services.workspace_paths.clone(),
        Arc::clone(&services.io.fs),
    )));
    tool_manager.register_builtin(Arc::new(
        TodoListTool::new(Arc::clone(&services.todo_list))
            .with_wire_recorder(services.wire_recorder.clone()),
    ));
    tool_manager.register_builtin(Arc::new(TaskListTool::new(Arc::clone(&services.tasks))));
    tool_manager.register_builtin(Arc::new(TaskStopTool::new(Arc::clone(&services.tasks))));
    tool_manager.register_builtin(Arc::new(TaskOutputTool::new(Arc::clone(&services.tasks))));

    // Cron tools
    tool_manager.register_builtin(Arc::new(CronCreateTool::new(
        Arc::clone(&services.cron),
        services.session_id.clone(),
    )));
    tool_manager.register_builtin(Arc::new(CronDeleteTool::new(Arc::clone(&services.cron))));
    tool_manager.register_builtin(Arc::new(CronListTool::new(Arc::clone(&services.cron))));

    // Goal tools
    tool_manager.register_builtin(Arc::new(
        CreateGoalTool::new(Arc::clone(&services.goal))
            .with_wire_recorder(services.wire_recorder.clone())
            .with_event_bus(services.event_bus.clone()),
    ));
    tool_manager.register_builtin(Arc::new(GetGoalTool::new(Arc::clone(&services.goal))));
    tool_manager.register_builtin(Arc::new(
        SetGoalBudgetTool::new(Arc::clone(&services.goal))
            .with_wire_recorder(services.wire_recorder.clone())
            .with_event_bus(services.event_bus.clone()),
    ));
    tool_manager.register_builtin(Arc::new(
        UpdateGoalTool::new(Arc::clone(&services.goal))
            .with_wire_recorder(services.wire_recorder.clone())
            .with_event_bus(services.event_bus.clone()),
    ));

    // Plan tools
    tool_manager.register_builtin(Arc::new(
        EnterPlanModeTool::new(Arc::clone(&services.plan))
            .with_wire_recorder(services.wire_recorder.clone()),
    ));
    tool_manager.register_builtin(Arc::new(
        ExitPlanModeTool::new(Arc::clone(&services.plan))
            .with_wire_recorder(services.wire_recorder.clone()),
    ));

    // Collaboration tools
    tool_manager.register_builtin(Arc::new(CollaborationStartTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationSendMessageTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationGetMessagesTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationGetStateTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationInterveneTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationEscalateTool::new(Arc::clone(
        &services.collaboration,
    ))));
    tool_manager.register_builtin(Arc::new(CollaborationTerminateTool::new(Arc::clone(
        &services.collaboration,
    ))));
    // The collaboration tools are a mode-gated capability: registered (so
    // `find_tool` resolves them and the engine can execute them) but
    // hidden from the LLM table until the host enters collaboration mode
    // (`collaboration.enter` / `collaboration.exit` toggle the whole set).
    for name in COLLABORATION_TOOL_NAMES {
        tool_manager.set_builtin_tool_enabled(name, false);
    }

    // Tool-select and Skill tools
    tool_manager.register_builtin(Arc::new(SelectToolsTool::new(Arc::clone(
        &services.tool_select,
    ))));
    tool_manager.register_builtin(Arc::new(
        SkillTool::new(Arc::clone(&services.skill_catalog))
            .with_event_bus(services.event_bus.clone()),
    ));

    // AskUserQuestion tool — only when a reverse-RPC slot is available.
    // 绑定 TaskManager 和 session_id 以启用 run_in_background 模式。
    // The SLOT (not a callback snapshot) rides the tool: the transport
    // fills it at subscribe, which may postdate agent creation; the tool
    // resolves the current value per call.
    if let Some(ref slot) = services.reverse_rpc_slot {
        tool_manager.register_builtin(Arc::new(
            AskUserQuestionTool::new(Arc::clone(slot))
                .with_task_manager(Arc::clone(&services.tasks), services.session_id.clone())
                .with_interaction_kernel(services.interaction_kernel.clone())
                .with_question_channel_wait(services.question_channel_wait),
        ));
    }

    // Agent and AgentSwarm tools — only when a subagent host is available.
    // Both carry the per-subagent timeout (D-40/D-45) and the background
    // availability gate (D-50).
    if let Some(ref host) = services.subagent_host {
        tool_manager.register_builtin(Arc::new(AgentTool::new(
            Arc::clone(host),
            Arc::clone(&services.tasks),
            services.session_id.clone(),
            services.subagent_timeout,
            Arc::clone(&services.background_gate),
        )));
        tool_manager.register_builtin(Arc::new(
            AgentSwarmTool::new(
                Arc::clone(host),
                services.subagent_timeout,
                services.max_swarm_items,
            )
            .with_task_tracking(Arc::clone(&services.tasks), services.session_id.clone()),
        ));
    }

    // WebSearch tool — only when a search provider is available.
    if let Some(ref provider) = services.web_search_provider {
        tool_manager.register_builtin(Arc::new(WebSearchTool::new(Arc::clone(provider))));
    }

    // FetchURL tool — only when a fetch provider is available.
    if let Some(ref provider) = services.fetch_url_provider {
        tool_manager.register_builtin(Arc::new(FetchUrlTool::new(Arc::clone(provider))));
    }
}

/// The names of every tool that can be registered by
/// [`register_builtin_tools`].
///
/// Note: `AskUserQuestion` is conditional — it is only registered when a
/// reverse-RPC slot is available (see
/// [`BuiltinToolServices::reverse_rpc_slot`]).
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "Agent",
    "AgentSwarm",
    "AskUserQuestion",
    "Bash",
    "CollaborationEscalate",
    "CollaborationGetMessages",
    "CollaborationGetState",
    "CollaborationIntervene",
    "CollaborationSendMessage",
    "CollaborationStart",
    "CollaborationTerminate",
    "CreateGoal",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "EnterPlanMode",
    "ExitPlanMode",
    "FetchURL",
    "GetGoal",
    "Glob",
    "Grep",
    "PowerShell",
    "Read",
    "ReadMediaFile",
    "select_tools",
    "SetGoalBudget",
    "Skill",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TodoList",
    "UpdateGoal",
    "WebSearch",
    "Write",
];

/// The session-independent builtin tool catalog: one `(name, description)`
/// row per entry of [`BUILTIN_TOOL_NAMES`], in the same order.
///
/// This is the config-time directory behind `tool.catalog` — the profile
/// editor's tool picker reads it to suggest builtin tools, so it must answer
/// with the full builtin set even when no session is live (a fresh daemon,
/// or sessions that live in other workspaces / only on disk). The
/// descriptions are one-line summaries for configuration UIs; the
/// LLM-facing tool table keeps the full per-tool `description()` texts.
pub const BUILTIN_TOOL_CATALOG: &[(&str, &str)] = &[
    (
        "Agent",
        "Launch a subagent to handle a task in its own context.",
    ),
    (
        "AgentSwarm",
        "Launch multiple subagents in parallel from one prompt template.",
    ),
    (
        "AskUserQuestion",
        "Ask the user a structured question with options.",
    ),
    ("Bash", "Execute a shell command and return its output."),
    (
        "CollaborationEscalate",
        "Pause a collaboration and escalate a dispute for arbitration.",
    ),
    (
        "CollaborationGetMessages",
        "Retrieve all messages of a collaboration session in arrival order.",
    ),
    (
        "CollaborationGetState",
        "Get the current state and round counter of a collaboration session.",
    ),
    (
        "CollaborationIntervene",
        "Inject a steering directive into an agent's context.",
    ),
    (
        "CollaborationSendMessage",
        "Send a protocol message between collaboration agents.",
    ),
    (
        "CollaborationStart",
        "Start a multi-agent collaboration session with a mode (roles + rules).",
    ),
    (
        "CollaborationTerminate",
        "Terminate a collaboration session as failed.",
    ),
    (
        "CreateGoal",
        "Create a new session goal describing the work toward an outcome.",
    ),
    (
        "CronCreate",
        "Schedule a cron task that injects a prompt at the given times.",
    ),
    ("CronDelete", "Remove a cron task by id."),
    ("CronList", "List all registered cron tasks."),
    ("Edit", "Replace an exact substring in a file."),
    (
        "EnterPlanMode",
        "Enter plan mode before non-trivial implementation work.",
    ),
    (
        "ExitPlanMode",
        "Submit the plan for user approval and leave plan mode.",
    ),
    ("FetchURL", "Fetch content from a URL."),
    ("GetGoal", "Get the current session goal."),
    ("Glob", "Find files by glob pattern."),
    ("Grep", "Search file contents with a regex pattern."),
    (
        "PowerShell",
        "Execute a PowerShell command (Windows only).",
    ),
    ("Read", "Read a text file with line numbers."),
    (
        "ReadMediaFile",
        "Read an image or media file from the local filesystem.",
    ),
    (
        "select_tools",
        "Load deferred tools by name so they can be called.",
    ),
    (
        "SetGoalBudget",
        "Attach a resource budget to the current goal.",
    ),
    (
        "Skill",
        "Invoke a registered skill from the current skill listing.",
    ),
    ("TaskList", "List background tasks and their status."),
    (
        "TaskOutput",
        "Read the accumulated output of a background task by id.",
    ),
    ("TaskStop", "Stop a background task by id."),
    ("TodoList", "View or update the session todo list."),
    ("UpdateGoal", "Update the status of the current goal."),
    ("WebSearch", "Search the web for current information."),
    ("Write", "Write text content to a file."),
];

/// Build a synthetic (non-executing) tool execution result for a user-facing
/// input problem.
///
/// The pipeline turns this into an error [`ToolResult`](crate::tool::ToolResult)
/// that the model sees, without ever invoking the execute phase.
pub fn tool_execution_error(message: impl Into<String>) -> ToolExecution {
    ToolExecution::Error(ToolErrorResult {
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is the config-time face of the builtin tool set: every
    /// name `register_builtin_tools` can register must carry a catalog row,
    /// so a newly added tool cannot silently vanish from the profile
    /// editor's tool picker.
    #[test]
    fn should_cover_every_builtin_tool_name_with_a_catalog_row() {
        // Given the builtin tool names and the static catalog
        // When comparing their name sets
        let mut catalog_names: Vec<&str> =
            BUILTIN_TOOL_CATALOG.iter().map(|(name, _)| *name).collect();
        catalog_names.sort_unstable();
        let mut tool_names = BUILTIN_TOOL_NAMES.to_vec();
        tool_names.sort_unstable();

        // Then they match exactly, and every row carries a description
        assert_eq!(catalog_names, tool_names);
        for (name, description) in BUILTIN_TOOL_CATALOG {
            assert!(
                !description.is_empty(),
                "catalog row for {name} must carry a description"
            );
        }
    }
}
