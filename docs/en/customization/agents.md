# Agents and Sub-Agents

Every session in Mirri Code CLI is driven by a **main Agent**. The main Agent understands the user's intent, plans steps, calls tools, and when needed dispatches **sub-agents** to handle more focused sub-tasks — for example, exploring an unfamiliar codebase, reviewing multiple implementations in parallel, or planning a large refactor without touching the main context.

A sub-agent receives a task description from the main Agent, works in its own isolated context, and then returns its conclusions. It does not communicate with the user directly, and its intermediate reasoning and tool call records do not mix into the main Agent's history.

## Built-in Sub-Agents

Mirri Code CLI includes three built-in sub-agents, ready to use out of the box, each aimed at a different task shape:

- **`coder`**: The default sub-agent — a general-purpose software engineering assistant that can read and write files, execute commands, search code, and land concrete changes.
- **`explore`**: Dedicated to codebase exploration; performs read-only operations only and does not modify any files. Ideal for quickly searching, reading, and summarizing a repository without touching files.
- **`plan`**: Dedicated to implementation planning and architecture design; even shell commands are not available, keeping the focus on "figuring out how to do something" rather than "actually doing it."

Built-in sub-agents other than the essential `agent` profile can be disabled via `disabled_agents` in `config.toml` or from the settings UI. See [Enabling and Disabling Agents](#enabling-and-disabling-agents).

A `coder` sub-agent shares most of the main Agent's tool set: it can run shell commands in the background, maintain todo lists, enter Plan mode, invoke Agent Skills, and dispatch its own nested sub-agents when a task decomposes naturally. If it finishes its turn while background tasks are still running, its run only reports completion after those tasks settle, so the parent receives the result after the underlying work has actually finished.

## How to Invoke

Sub-agents are scheduled automatically by the main Agent — based on task complexity, context consumption, and sub-task independence, they are dispatched at the right moment without the user having to specify one.

Each dispatch is presented in the terminal as an approval request (unless it matches an allow rule or YOLO mode is active), giving you a chance to review the task description. You can also instruct the main Agent directly in conversation to use a specific sub-agent, for example: "Use explore to map out the relevant files before making any changes."

Sub-agents support running in the background: results are automatically returned to the main Agent upon completion, with no manual polling needed. You can also call back an existing sub-agent instance to continue the same task.

## Context Isolation and Resource Cost

Each sub-agent has a fully independent context window. It can only see the task description explicitly passed by the main Agent and cannot see the main Agent's conversation history. The sub-agent's own intermediate reasoning and tool call records do not flow back; only the final result appears in the main Agent's context.

This isolation provides two benefits:

- **The main Agent's context stays lean** and is not filled with large volumes of exploratory logs during long sessions.
- **Multiple sub-agents can run in parallel** without interfering with each other.

Note that each sub-agent independently consumes model tokens. For simple tasks, there is no need to dispatch a sub-agent — the main Agent handles them more economically.

## Permission Inheritance

Sub-agent permission rules are inherited from the main Agent: "always allow" rules that the main Agent has accepted via `/permission` or through an approval dialog automatically propagate to all sub-agents it dispatches, so sub-agents do not need to re-approve the same types of tool calls. The `Agent` tool itself is allowed by default, enabling the main Agent to delegate multiple times without interrupting the user.

If you need a particular type of tool to be permanently unavailable inside sub-agents, tighten the corresponding permission rule on the main Agent.

## Custom Agent Profiles

You can define custom agent profiles in Markdown files (YAML frontmatter + body). The system discovers them automatically and merges them with the built-in profiles. Legacy `.yaml`/`.yml` files are still read for backward compatibility, but new profiles are saved as `.md`.

### File Locations

Custom profiles are loaded from these directories, in priority order (first match wins by name):

1. **Project**: `<project-root>/.mirri-code/agents/*.md`
2. **User**: `$MIRRICODE_HOME/agents/*.md` (default: `~/.mirri-code/agents/`)
3. **Extra**: directories listed in `extra_agent_dirs` in `config.toml`

When both `.md` and `.yaml` exist for the same agent name in the same directory, `.md` takes priority.

A custom profile with the same name as a built-in **partially merges** with the built-in — only the fields you declare override the built-in's values. This lets you change a single field (like `defaultModel`) without copying the entire built-in definition:

```markdown
---
# ~/.mirri-code/agents/coder.md
name: coder
defaultModel: gpt-4o
---
```

This overrides only `defaultModel`; `tools`, `whenToUse`, and the system prompt are all preserved from the built-in coder. When the built-in is updated in a new release, your override automatically inherits the changes.

### Markdown Format

Each file defines one agent profile — YAML frontmatter for structured metadata, Markdown body for the system prompt:

```markdown
---
name: reviewer
description: Code review specialist
defaultModel: claude-sonnet
whenToUse: Use this agent for code review tasks
tools:
  - Read
  - Grep
  - Glob
---

You are a code review specialist. Focus on security, performance, and style consistency.
```

#### Fields

| Field | Location | Type | Description |
|-------|----------|------|-------------|
| `name` | frontmatter | string (required) | Profile name, must be kebab-case (`[a-z0-9-]+`) |
| `description` | frontmatter | string (required) | Short description shown in UI |
| `whenToUse` | frontmatter | string | Guidance for when the main agent should choose this sub-agent |
| `defaultModel` | frontmatter | string | Model alias from `config.toml` `models` map |
| `tools` | frontmatter | string[] | Exact tool names available to this agent (omitted = all tools) |
| `subagents` | frontmatter | string[] | Sub-agent profile names this agent can dispatch (omitted = all) |
| `capabilitiesRequired` | frontmatter | string[] | Required capabilities (gates which tools are visible) |
| *(body)* | Markdown body | string | System prompt template — the Markdown content after the closing `---` fence |

### Model Selection for Sub-Agents

When the main agent spawns a sub-agent via the `Agent` or `AgentSwarm` tool, it can specify a `model` parameter to override the default. The model resolves in this order:

1. **Explicit model** — the `model` parameter passed by the LLM in the tool call
2. **Profile default** — the `defaultModel` field declared in the agent profile
3. **Parent inheritance** — the main agent's current model alias

The tool description lists available agent types with their default models, so the LLM knows when not to override. Additionally, models that have a `description` field set in `config.toml` appear in an "Available models" section, giving the LLM context about each model's strengths:

```toml
[models."sonnet"]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_context_size = 200000
description = "balanced, strong at multi-file coding tasks"

[models."haiku"]
provider = "anthropic"
model = "claude-haiku-3-5-20241022"
max_context_size = 200000
description = "fast, best for simple lookups and quick tasks"
```

Models without a `description` are still valid for `defaultModel` but are not offered as LLM-selectable overrides. This keeps the model list concise — only models you've intentionally curated for agent dispatch appear.

If the LLM passes an invalid model alias, the tool returns an error listing the valid options, allowing it to correct its choice in the next turn.

## Enabling and Disabling Agents

Built-in sub-agents (except the essential `agent` profile) and custom agents can be enabled or disabled.

### Via config.toml

```toml
disabled_agents = ["explore", "plan"]
```

The `agent` profile is essential and cannot be disabled — entries for `agent` in `disabled_agents` are silently ignored.

### Via Settings UI

The Web and Desktop settings page lists all agent profiles with an enable/disable toggle. Essential profiles have a disabled toggle.

### Via TUI

Use the `/agents` slash command to list profiles and toggle their status.

## Instruction Files

Global Mirri-specific instructions can live at `$MIRRICODE_HOME/AGENTS.md` (default: `~/.mirri-code/AGENTS.md`). When you relocate the data root with `MIRRICODE_HOME`, this global instruction file moves with it. Generic cross-tool instructions can still live under `~/.agents/AGENTS.md` in the real OS home, and project-level instructions remain under the project tree, for example `.mirri-code/AGENTS.md` or `AGENTS.md`.

## Storage Location in the Session Directory

Sub-agent runtime state is persisted to the `agents/` subdirectory of the current session directory. Each sub-agent instance has its own directory, which contains a `wire.jsonl` file that records prompts, message history, and final state in chronological order. Background sub-agents also expose their lifecycle status through a `tasks/` subdirectory.

::: warning Note
Session directories, wire files, and task records are all local debug materials that may contain user prompts, command output, repository paths, tool return values, or traces of credentials. Do not commit these files directly to public repositories, issues, or chat logs; redact sensitive information before sharing.
:::

## Next steps

- [Hooks](./hooks.md) — Trigger local script notifications or interceptions at key points such as sub-agent completion
- [Agent Skills](./skills.md) — Inject specialized knowledge and workflows into sub-agents
