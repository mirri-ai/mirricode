# Custom Agent Profiles — Requirements Document

## 1. Background

Mirri Code CLI's agent system consists of 4 built-in profiles (`agent`, `coder`, `explore`, `plan`), embedded as YAML files at build time in `packages/agent-core/src/profile/default/`. A shared `system.md` template provides the common system prompt; each profile injects role-specific instructions via `promptVars.roleAdditional`.

Initial limitations (all resolved by this feature):

1. **No user-defined agents** — All profiles were compile-time constants. Users could not add, modify, or remove agent definitions.
2. **No per-agent model** — Subagents unconditionally inherited the parent agent's model alias. The profile schema had no model field.
3. **No LLM model selection** — The `Agent` and `AgentSwarm` tool input schemas had no `model` parameter. The LLM could not choose a model based on task difficulty when spawning subagents.
4. **No enable/disable control** — Built-in non-essential agents always appeared in the `AgentTool` description and could not be disabled.
5. **No dynamic subagent discovery** — `AgentTool` description used a static built-in subagent list. Custom agents were invisible to the LLM.
6. **Disabled agents still ran** — The three-layer fallback in `resolveProfile()` bypassed the registry's enabled filtering.

## 2. Root Requirement

Provide a complete custom agent management system:

1. **File-level customization**: Users place YAML agent definition files in well-known directories; the system discovers and loads them automatically.
2. **Partial-merge override**: A custom YAML with the same name as a built-in partially merges — only declared fields override; others inherit from the built-in. This lets users override just `defaultModel` without copying the entire built-in definition.
3. **Per-agent default model**: Each profile can declare a `defaultModel` (a model alias from `config.toml`). Subagents resolve their model via 3-tier precedence: LLM-specified → profile default → parent inheritance.
4. **LLM model selection for subagents**: `Agent` / `AgentSwarm` tools accept an optional `model` parameter.
5. **Dynamic subagent discovery**: `AgentTool` and `AgentSwarmTool` descriptions dynamically list all enabled subagents (built-in + custom). Disabled agents disappear from the LLM's view.
6. **Enable/disable control**: Built-in non-essential agents and custom agents can be disabled via `config.toml` or UI. Disabled agents are hidden from the LLM and cannot be dispatched.
7. **UI management**: Web/Desktop settings page can view built-in definitions, create/edit/delete custom agents, toggle enable/disable, select models from a dropdown, and configure tools via a tag editor with autocomplete.
8. **TUI command**: `/agents` command lists and manages agent profiles.

## 3. User Stories

### US-1: Define a custom agent in YAML

**As a developer**, I want to define a custom agent in a YAML file so that the system auto-loads it.

**Acceptance criteria:**
- Placing a `reviewer.yaml` in `~/.mirri-code/agents/` makes the `reviewer` agent available.
- Placing a `test-runner.yaml` in `<project>/.mirri-code/agents/` makes the `test-runner` agent available for that project.
- `extra_agent_dirs` in `config.toml` adds additional scan directories.
- Custom profiles can use `extends: agent` (or any other profile) to inherit tools, system prompt, and prompt vars.
- Discovery priority: project > user > extra > built-in (first-wins by name for overrides).
- The YAML schema is validated; invalid files are skipped with a warning.

### US-2: Built-in agent model override (partial merge)

**As a user**, I want to give a built-in agent (e.g. `coder`) a different default model without copying the entire built-in YAML definition.

**Acceptance criteria:**
- Placing `name: coder\ndefaultModel: gpt-4o` in `~/.mirri-code/agents/coder.yaml` overrides only `defaultModel`.
- All other fields (tools, whenToUse, promptVars, extends) are preserved from the built-in.
- When the built-in is updated in a new release, the override automatically inherits the changes.
- The `agent` (essential) profile cannot be overridden.
- The `ProfileService.setBuiltinModelOverride(name, model)` method creates/updates/removes these minimal override files.
- The REST endpoint `POST /agents/{name}:set-model` exposes this. Passing `model: null` or omitting it removes the override.

### US-3: LLM sees and can dispatch custom agents

**As a user**, I want the main agent to know my custom agents exist and be able to dispatch tasks to them automatically.

**Acceptance criteria:**
- `AgentTool` description dynamically lists all enabled subagents (built-in + custom).
- `AgentSwarmTool` description dynamically lists available agent types.
- The LLM can dispatch via `subagent_type="my-coder"` and the correct profile is resolved.
- Tool descriptions refresh automatically when profiles change (via provider function + getter pattern).
- `ProfileRegistry.getAvailableSubagents()` is the single source of truth: declared subagents from `agent.subagents` (filtered by enabled) PLUS all other enabled non-essential profiles.

### US-4: Disabled agents are hidden and cannot be dispatched

**As a user**, I want to disable a built-in agent (e.g. `explore`) so the LLM cannot see or dispatch it.

**Acceptance criteria:**
- `disabled_agents = ["explore"]` in `config.toml` removes `explore` from `AgentTool` description.
- LLM attempting `subagent_type="explore"` throws `"explore" was not found or is disabled`.
- The `agent` (essential) profile cannot be disabled.
- An already-running subagent that gets disabled mid-session can still be resumed (same session), but new spawns are blocked.

### US-5: Model resolution precedence

**As a user/LLM**, I want model selection to have a clear precedence and the LLM to be able to choose a model by task difficulty.

**Acceptance criteria:**
- 3-tier model resolution:
  1. **LLM-specified** — `model` parameter in Agent/AgentSwarm tool call
  2. **Profile defaultModel** — declared in profile YAML (or overridden via partial-merge)
  3. **Parent inheritance** — subagent inherits parent agent's model alias
- `defaultModel` is inherited through `extends` chains (child overrides parent with `??` semantics).
- The alias must exist in `config.toml`'s `models` map — `ProviderManager.resolveProviderConfig()` throws `CONFIG_INVALID` if not.

### US-6: Web/Desktop UI — Agent management

**As a user**, I want to view all agents, enable/disable, and create/edit/delete custom agents from the settings UI.

**Acceptance criteria:**
- List shows all agents (built-in + custom) with name, description, source badge, enable/disable toggle.
- Non-essential agents can be enabled/disabled via toggle.
- Custom agents can be edited/deleted.
- Built-in agents show a model selector dropdown (populated from `GET /models`).
- Selecting "(inherited)" clears the model override.
- Creating an agent persists a YAML file to `~/.mirri-code/agents/<name>.yaml`.
- Changes propagate to all connected clients via `event.agent.profiles_changed`.

### US-7: Web/Desktop UI — Model selector dropdown

**As a user**, I want to select an agent's default model from a dropdown of configured models, not type it manually.

**Acceptance criteria:**
- Built-in agents show a `<Select>` populated from `GET /models` (grouped by provider).
- Selecting a model calls `POST /agents/{name}:set-model` which creates/updates the override YAML.
- Selecting "(inherited)" deletes the override file.
- Custom agent create/edit form also uses a `<Select>` for `defaultModel`.

### US-8: Web/Desktop UI — Tool tag editor with autocomplete

**As a user**, I want to configure agent tools via a tag-style editor with autocomplete, not a raw comma-separated text input.

**Acceptance criteria:**
- Selected tools display as removable tag chips.
- Typing filters suggestions by name + description (case-insensitive substring).
- Suggestions include three categories:
  - **Built-in tools**: Read, Write, Bash, Grep, Glob, etc.
  - **MCP server-level**: `mcp__<server>__*` (all tools from that server)
  - **MCP tool-level**: `mcp__<server>__<tool>` (specific tool)
- When `extends` parent changes, tools auto-fill from the parent's tool list.
- If tools were manually edited, a confirm dialog asks before overwriting on extends change.
- When editing an existing profile, tools load from the profile (not from extends).

### US-9: TUI — /agents command

**As a TUI user**, I want to use `/agents` to view and toggle agent profiles.

**Acceptance criteria:**
- Lists all agent profiles with source and enable/disable status.
- Selecting a non-essential profile toggles its enabled state.
- Essential profiles are marked as non-toggleable.

## 4. Non-Functional Requirements

### Performance
- Profile discovery + reload completes in < 500ms for up to 50 custom profiles.
- Profile reload does not block the session event loop (async, non-blocking).
- `AgentTool.description` getter is cheap (a map iteration over enabled entries).

### Compatibility
- Existing built-in profiles remain unchanged when no custom profiles or config overrides are present.
- `DEFAULT_AGENT_PROFILES` constant remains available as a fallback when no registry is configured.
- Existing sessions that don't use custom agents have zero behavioral change.

### Security
- YAML files are validated against `RawAgentProfileSchema`; invalid files are skipped.
- `extends` cycle detection prevents infinite loops.
- Profile names must match `[a-z0-9-]+` (kebab-case) — rejects path traversal.
- No arbitrary code execution from YAML content (parsed in safe mode via `js-yaml`).
- Built-in profiles cannot be created (name collision), updated, or deleted via API.
- Essential `agent` profile cannot be disabled or model-overridden.
- `defaultModel` values are validated at provider resolution time — non-existent aliases throw `CONFIG_INVALID`.

## 5. Out of Scope

- Hot-swapping models mid-turn for the main agent (the LLM cannot switch its own model; only subagent models).
- Agent profile versioning or migration.
- Importing/exporting agent profiles as packages.
- Per-project agent profile overrides via UI (only via file system).
- Model auto-selection by the system (the LLM makes the choice; the system only provides information).
