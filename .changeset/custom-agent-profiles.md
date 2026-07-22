---
"@mirri-ai/mirri-code": minor
---

Add custom agent profiles: define custom agents in YAML files under `~/.mirri-code/agents/` or `.mirri-code/agents/`, with per-agent default model, enable/disable controls, and a `model` parameter on the Agent and AgentSwarm tools so the main agent can choose a model per sub-task. Configure discovery dirs with `extra_agent_dirs` and disable agents with `disabled_agents` in config.toml. Built-in agents can be partially overridden (e.g. override only `defaultModel` without replacing the full profile). Custom agents are automatically visible to the LLM and dispatchable. Disabled agents are hidden from the LLM and cannot be spawned. The Web UI includes a model selector dropdown and a tag-based tool editor with autocomplete for built-in tools and MCP tools.
