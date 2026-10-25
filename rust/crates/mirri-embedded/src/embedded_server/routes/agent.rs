//! Agent-domain routes — the 12 `agent.*` arms migrated from
//! `EngineAdapter::call_method`'s match (M3 batch 3), plus the two single
//! `tool.list` / `skill.list` arms that shared the match's agent-adjacent
//! sections. Doc comments moved with their arms; handlers and inline
//! values are unchanged.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Context-memory snapshot (message count, used tokens).
    Route {
        method: "agent.getContext",
        aliases: &["getContext"],
        handler: |server, params| Box::pin(server.dispatch_get_context(params)),
    },
    // Accumulated token usage, broken down by model.
    Route {
        method: "agent.getUsage",
        aliases: &["getUsage"],
        handler: |server, params| Box::pin(server.dispatch_get_usage(params)),
    },
    // The agent's current model id (a plain wire string).
    Route {
        method: "agent.getModel",
        aliases: &["getModel"],
        handler: |server, params| Box::pin(server.dispatch_get_model(params)),
    },
    // Agent-scope runtime config updates: all three apply immediately and
    // answer on the klient result shapes (`setModel` echoes
    // `{model, providerName?}`, the other two are JSON `null` — see
    // dispatch_set_model / dispatch_set_thinking / dispatch_set_permission).
    Route {
        method: "agent.setModel",
        aliases: &["setModel"],
        handler: |server, params| Box::pin(server.dispatch_set_model(params)),
    },
    Route {
        method: "agent.setThinking",
        aliases: &["setThinking"],
        handler: |server, params| Box::pin(server.dispatch_set_thinking(params)),
    },
    // B2 gap implementation: the effective thinking effort (plain string).
    Route {
        method: "agent.getEffectiveThinkingLevel",
        aliases: &["getEffectiveThinkingLevel"],
        handler: |server, params| Box::pin(server.dispatch_get_effective_thinking_level(params)),
    },
    Route {
        method: "agent.setPermission",
        aliases: &["setPermission"],
        handler: |server, params| Box::pin(server.dispatch_set_permission(params)),
    },
    // The agent's permission mode.
    Route {
        method: "agent.getPermission",
        aliases: &["getPermission"],
        handler: |server, params| Box::pin(server.dispatch_get_permission(params)),
    },
    // The agent's resolved configuration.
    Route {
        method: "agent.getConfig",
        aliases: &["getConfig"],
        handler: |server, params| Box::pin(server.dispatch_get_config(params)),
    },
    // Restricts the agent's tool set by name.
    Route {
        method: "agent.setActiveTools",
        aliases: &["setActiveTools"],
        handler: |server, params| Box::pin(server.dispatch_set_active_tools(params)),
    },
    // User-registered tools need an executable tool type the embedded
    // engine lacks; acknowledged.
    Route {
        method: "agent.registerTool",
        aliases: &["registerTool"],
        handler: |server, params| Box::pin(server.dispatch_register_tool(params)),
    },
    // No user-tool removal API; acknowledged.
    Route {
        method: "agent.unregisterTool",
        aliases: &["unregisterTool"],
        handler: |server, params| Box::pin(server.dispatch_unregister_tool(params)),
    },
    // Injects the skill's SKILL.md content as a system reminder.
    Route {
        method: "agent.activateSkill",
        aliases: &["activateSkill"],
        handler: |server, params| Box::pin(server.dispatch_activate_skill(params)),
    },
    // B2 gap implementation: the agent's live activity snapshot (the klient
    // agentActivityView.state wire shape, folded by the engine-side
    // ActivityTracker).
    Route {
        method: "agent.activity",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_agent_activity(params)),
    },
    // B2 gap implementations: the command surface available to the agent
    // (klient agentRpcContract listCommands / runCommand).
    Route {
        method: "agent.listCommands",
        aliases: &["listCommands"],
        handler: |server, params| Box::pin(server.dispatch_agent_list_commands(params)),
    },
    Route {
        method: "agent.runCommand",
        aliases: &["runCommand"],
        handler: |server, params| Box::pin(server.dispatch_agent_run_command(params)),
    },
    // The addressed agent's registered tool definitions.
    Route {
        method: "tool.list",
        aliases: &["getTools"],
        handler: |server, params| Box::pin(server.dispatch_get_tools(params)),
    },
    // T2 user-tool management read face: every registered user tool with
    // its registration payload and current visibility (disabled ones
    // included — `tool.list` serves the active table only).
    Route {
        method: "tool.listUserTools",
        aliases: &["listUserTools"],
        handler: |server, params| Box::pin(server.dispatch_list_user_tools(params)),
    },
    // T2 user-tool management write face: per-name enable/disable of a
    // registered user tool (removal stays on agent.unregisterTool).
    Route {
        method: "tool.setUserToolEnabled",
        aliases: &["setUserToolEnabled"],
        handler: |server, params| Box::pin(server.dispatch_set_user_tool_enabled(params)),
    },
    // Both session-level `listSkills` and workspace-level
    // `listWorkspaceSkills` read the workspace skill catalog.
    Route {
        method: "skill.list",
        aliases: &["listSkills", "listWorkspaceSkills"],
        handler: |server, params| Box::pin(server.dispatch_list_skills(params)),
    },
    // B8 skills management read face: the skill's SKILL.md content for the
    // web management view (workspace-scoped like skill.list; an optional
    // session_id routes to that session's shared catalog).
    Route {
        method: "skill.getContent",
        aliases: &["getSkillContent"],
        handler: |server, params| Box::pin(server.dispatch_get_skill_content(params)),
    },
    // B8 skills management write face: user-level per-skill enablement,
    // persisted as the top-level config key disabled_skills.
    Route {
        method: "skill.setEnabled",
        aliases: &["setSkillEnabled"],
        handler: |server, params| Box::pin(server.dispatch_set_skill_enabled(params)),
    },
    // ADR-010 Task 6: the manual skill-catalog reload face — rescan every
    // built workspace through the serialized orchestrator and answer the
    // per-workspace diff counts plus the watcher status (R4).
    Route {
        method: "skill.reload",
        aliases: &["reloadSkills"],
        handler: |server, params| Box::pin(server.dispatch_skill_reload(params)),
    },
];
