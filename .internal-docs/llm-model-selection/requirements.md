# LLM-Aware Model Selection for Subagent Dispatch — Requirements Document

## 1. Background

The custom agent profiles feature (see `.internal-docs/custom-agents/`) added `defaultModel` to agent profiles and a `model` parameter to the `Agent` and `AgentSwarm` tool schemas. The LLM can now pass a model alias when dispatching subagents.

However, the LLM currently operates blind:

1. **Dead reference**: The `model` parameter description says *"See 'Available Models' in this tool description for the list"* — but no such section is ever injected into the tool description.
2. **No default visibility**: `buildSubagentDescriptions()` exposes agent name, description, whenToUse, and tools — but not `defaultModel`. The LLM cannot know which agents already have a sensible model configured.
3. **No model catalog**: No list of valid model aliases is passed to the LLM. It can pass any string as `model`, valid or not.
4. **No validation**: If the LLM passes an invalid model alias, it silently falls back to the parent model. The LLM gets no feedback to correct itself.
5. **Context bloat risk**: Users with multiple providers may have dozens of models in `config.toml`. Dumping all of them into the tool description would bloat context and cause choice paralysis.

## 2. Root Requirement

Make the LLM an informed decision-maker when selecting models for subagent dispatch:

1. **Expose default models**: The LLM should see each agent profile's `defaultModel` so it knows when not to override.
2. **Curated model list**: Only models with a user-provided `description` field are exposed to the LLM. This naturally limits the list to models the user has intentionally curated for agent dispatch.
3. **Actionable errors**: Invalid model aliases produce clear error messages listing valid options.
4. **Guided behavior**: The `model` parameter description guides the LLM to prefer defaults and only override with justification.

## 3. User Stories

### US-1: LLM sees agent default models

**As a** user with configured agent profiles
**I want** the LLM to see each agent's `defaultModel`
**So that** the LLM doesn't unnecessarily override the model I carefully selected for each agent

**Acceptance Criteria:**
- `buildSubagentDescriptions()` output includes a `Default model: <alias>` line for each agent that has a `defaultModel`
- Agents without `defaultModel` show `Default model: inherited` instead of omitting the line
- The existing description/tools format is preserved

### US-2: User curates which models are exposed to the LLM

**As a** user with many models configured across multiple providers
**I want** to control which models the LLM can choose from when dispatching subagents
**So that** the LLM isn't overwhelmed and only picks from models I've explicitly endorsed

**Acceptance Criteria:**
- A new optional `description` field is added to the model alias schema in `config.toml`
- Only models with a non-empty `description` appear in the "Available models" section of the `Agent` and `AgentSwarm` tool descriptions
- Models without `description` are still valid for use in `defaultModel` — they're just not presented as LLM-selectable options
- The `description` field is a short human-readable capability tag, e.g. `"balanced, strong at multi-file coding tasks"`

### US-3: LLM sees available models in tool description

**As a** the LLM dispatching a subagent
**I want** to see a list of valid model aliases with their capability descriptions
**So that** I can make an informed decision about whether to override the default

**Acceptance Criteria:**
- When at least one model has a `description`, the `Agent` tool description includes an "Available models" section
- Each entry shows: alias (the value to pass as `model` parameter) — description, max context size
- When no models have `description`, the section is omitted entirely and the `model` parameter description guides toward not overriding
- The section appears after the agent types list

### US-4: LLM sees available models in AgentSwarm tool

**As a** the LLM dispatching a swarm of subagents
**I want** the same model list available in the `AgentSwarm` tool description
**So that** I can choose a model for the swarm with the same information

**Acceptance Criteria:**
- `AgentSwarm` tool description includes the same "Available models" section
- The `model` parameter description is consistent with the `Agent` tool

### US-5: Invalid model alias returns actionable error

**As a** the LLM dispatching a subagent
**I want** to get a clear error if I pass an invalid model alias
**So that** I can correct my choice in the next turn instead of silently using a wrong fallback

**Acceptance Criteria:**
- When the LLM passes a `model` parameter that doesn't match any configured alias, the tool returns an error result (not an exception)
- The error message lists available model aliases (the curated ones with `description`)
- The error message includes the profile's `defaultModel` if set
- The subagent is not spawned

### US-6: `model` parameter description is dynamic

**As a** the LLM
**I want** the `model` parameter description to adapt based on whether curated models exist
**So that** I get appropriate guidance in both scenarios

**Acceptance Criteria:**
- When curated models exist: description lists valid aliases and says "Only override when task difficulty clearly differs from the agent purpose"
- When no curated models exist: description says "Most agents already have a sensible default — leave empty unless you have a specific reason to override" and does not reference a non-existent list

## 4. Non-Functional Requirements

### NFR-1: Context efficiency
- The "Available models" section must not exceed ~500 characters for typical configurations (2-5 curated models)
- Each model entry is one line: `- <alias> — <description>, <N>k context`

### NFR-2: No breaking changes
- The `description` field on models is optional. Existing `config.toml` files without it continue to work identically.
- No existing test should break.

### NFR-3: Performance
- Model catalog data is already loaded by `ModelCatalogService`; no additional I/O.
- The tool description getter is called once per turn, not per tool call.

### NFR-4: Security
- Model descriptions are user-configured; no injection risk (they're rendered as plain text in tool descriptions, not executed).
- Essential agent profiles cannot be overridden via model description manipulation.

## 5. Out of Scope

- Automatic model recommendation based on task analysis (future enhancement)
- Model cost/pricing display (not available in current schema)
- Per-agent model allowlist (over-complex for current needs)
- UI for editing model `description` field (users edit `config.toml` directly; the Web UI model manager is a separate feature)
