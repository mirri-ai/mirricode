# Custom Agent Profiles — Requirements Document

## 1. Background

Mirri Code CLI's agent system consists of 4 built-in profiles (`agent`, `coder`, `explore`, `plan`), embedded as YAML files at build time in `packages/agent-core/src/profile/default/`. A shared `system.md` template provides the common system prompt; each profile injects role-specific instructions via `promptVars.roleAdditional`.

Current limitations:

1. **No user-defined agents** — All profiles are compile-time constants. Users cannot add, modify, or remove agent definitions.
2. **No per-agent model** — Subagents unconditionally inherit the parent agent's model alias (`subagent-host.ts:configureChild`). The profile schema has no model field.
3. **No LLM model selection** — The `Agent` and `AgentSwarm` tool input schemas have no `model` parameter. The LLM cannot choose a model based on task difficulty when spawning subagents.
4. **No enable/disable control** — Built-in non-essential agents (`coder`, `explore`, `plan`) cannot be disabled, even when unused. They always appear in the `AgentTool` description.

## 2. Root Requirement

Provide a complete custom agent management system:

1. **File-level customization**: Users place YAML agent definition files in well-known directories; the system discovers and loads them automatically.
2. **Per-agent default model**: Each profile can declare a `defaultModel` (a model alias from `config.toml`). Subagents resolve their model via 3-tier precedence: LLM-specified → profile default → parent inheritance.
3. **LLM model selection for subagents**: `Agent` / `AgentSwarm` tools accept an optional `model` parameter; the tool description lists available models with context size and capabilities so the LLM can choose by task difficulty.
4. **Enable/disable control**: Built-in non-essential agents and custom agents can be disabled via `config.toml` or UI. The built-in `agent` profile is essential and always enabled.
5. **UI management**: Web/Desktop settings page can view built-in definitions, create/edit/delete custom agents, and toggle enable/disable.
6. **TUI command**: `/agents` command lists and manages agent profiles.

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

### US-2: Per-agent default model alias

**As a developer**, I want each agent profile to specify a default model alias.

**Acceptance criteria:**
- The `defaultModel` field in a profile YAML specifies a model alias name.
- The alias must exist in `config.toml`'s `models` map. If it doesn't, the system falls back to the parent's model and logs a warning.
- `defaultModel` is inherited through `extends` chains (child overrides parent).
- When a subagent is spawned, its model resolves as: `options.model` (LLM-specified) → `profile.defaultModel` → `parent.config.modelAlias`.

### US-3: LLM chooses model when spawning subagents

**As the main agent (LLM)**, I want to choose a model when spawning subagents based on task difficulty.

**Acceptance criteria:**
- The `Agent` tool input schema accepts an optional `model` parameter (a model alias name).
- The `AgentSwarm` tool input schema accepts the same optional `model` parameter (applied to all subagents in the swarm).
- The `Agent` tool description includes a table of available models with alias, display name, context size, and capabilities.
- The description provides guidance: large-context models for complex multi-file tasks, smaller/faster models for simple lookups.
- If `model` is omitted, the subagent uses `profile.defaultModel` or inherits the parent's model.
- If the specified alias doesn't exist, the subagent falls back to the parent's model and the tool result includes a warning.

### US-4: Enable/disable agents

**As a user**, I want to enable or disable non-essential agents from config or UI.

**Acceptance criteria:**
- `disabled_agents` in `config.toml` lists agent profile names to disable.
- The built-in `agent` profile is essential and cannot be disabled (attempts are ignored).
- Built-in `coder`, `explore`, `plan` can be disabled.
- Custom agents can be disabled.
- Disabled agents do not appear in `getMergedProfiles()` or the `AgentTool` description.
- Disabled agents still appear in `listEntries()` with `enabled: false`.
- The UI shows a toggle for each agent; essential agents have a disabled toggle.
- Config changes trigger a profile reload without restarting the session.

### US-5: View built-in agent definitions in UI

**As a user**, I want to view built-in agent definitions in the settings UI.

**Acceptance criteria:**
- The settings UI lists all agent profiles (built-in + custom) with name, source badge, description, and status.
- Clicking a built-in profile shows its definition (tools, defaultModel, whenToUse, system prompt template) in read-only mode.

### US-6: Create/edit/delete custom agents from UI

**As a user**, I want to create, edit, and delete custom agents from the settings UI.

**Acceptance criteria:**
- An "Add Agent" form allows specifying: name, description, extends, defaultModel, tools, roleAdditional, whenToUse.
- Creating an agent persists a YAML file to `~/.mirri-code/agents/<name>.yaml`.
- Editing a custom agent updates the YAML file and reloads the profile registry.
- Deleting a custom agent removes the YAML file.
- Built-in profiles cannot be created (name collision), edited, or deleted.
- Attempting to create a profile with an existing name is rejected.
- Changes propagate to all connected clients via `event.agent.profiles_changed`.

### US-7: Manage agents from TUI

**As a user**, I want to manage agent profiles from the terminal via `/agents`.

**Acceptance criteria:**
- The `/agents` slash command opens a selector listing all profiles with source and enable/disable status.
- Selecting a profile shows its details.
- Enable/disable can be toggled from the selector.

## 4. Non-Functional Requirements

### Performance
- Profile discovery + reload completes in < 500ms for up to 50 custom profiles.
- Profile reload does not block the session event loop (async, non-blocking).
- The `AgentTool` description (including model table) does not exceed reasonable token bounds (< 2KB for the model table section).

### Compatibility
- Existing built-in profiles (`agent`, `coder`, `explore`, `plan`) remain unchanged in behavior when no custom profiles or config overrides are present.
- The `DEFAULT_AGENT_PROFILES` constant remains available for backward compatibility; `ProfileRegistry` wraps it.
- Existing sessions that don't use custom agents have zero behavioral change.

### Security
- YAML files are validated against `RawAgentProfileSchema`; invalid files are skipped, not crashed on.
- `extends` cycle detection prevents infinite loops (already exists in `resolveAgentProfiles`).
- File paths are normalized; path traversal (`../../etc/passwd`) in profile names is rejected.
- Profile names must match `[a-z0-9-]+` (kebab-case) to prevent filesystem issues.
- No arbitrary code execution from YAML content (YAML is parsed in safe mode via `js-yaml`).

### Extensibility
- The scanner/registry pattern mirrors `skill/scanner.ts` for consistency.
- New profile sources (e.g., plugin-provided) can be added by extending `AgentProfileSource`.
- The REST API follows existing `defineRoute` + envelope patterns.

## 5. Out of Scope

- Hot-swapping models mid-turn for the main agent (the LLM cannot switch its own model; only subagent models).
- Agent profile versioning or migration.
- Importing/exporting agent profiles as packages.
- Per-project agent profile overrides via UI (only via file system).
- Model auto-selection by the system (the LLM makes the choice; the system only provides information).
