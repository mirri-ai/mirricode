---
"@mirri-ai/mirri-code": minor
---

Add custom agent profiles: define custom agents in YAML files under `~/.mirri-code/agents/` or `.mirri-code/agents/`, with per-agent default model, enable/disable controls, and a `model` parameter on the Agent and AgentSwarm tools so the main agent can choose a model per sub-task. Configure discovery dirs with `extra_agent_dirs` and disable agents with `disabled_agents` in config.toml.
