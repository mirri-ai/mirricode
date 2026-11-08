# Configuration files

Mirri Code writes all long-term preferences — which model to use, which API key to fill in, how many steps an Agent can run per turn — into TOML (a plain-text configuration format with a clear structure) files. Change them once and they take effect on every startup. Agent and runtime settings live in `config.toml`; terminal-UI and client preferences (theme, editor, notifications, auto-update) live in a companion `tui.toml`.

Default location: `~/.mirri-code/config.toml`, created automatically on first run.

## Config file location

The CLI reads configuration from `~/.mirri-code/config.toml`. To relocate the data directory, override it with the `MIRRICODE_HOME` environment variable:

```sh
export MIRRICODE_HOME=/path/to/mirri-home
```

The config file path then becomes `$MIRRICODE_HOME/config.toml`. Regardless of where the directory lives, the file name is always `config.toml`.

::: tip
TOML field names always use snake_case, for example `default_model` and `max_context_size`. If a key contains `.`, you must quote it — for example `[models."gpt-4.1"]` — otherwise TOML treats `.` as a nested table separator.
:::

## Complete example

The following example covers the most commonly used configuration fields. You can copy it and adjust as needed:

```toml
default_model = "deepseek-flash"
default_permission_mode = "manual"
default_plan_mode = false
merge_all_available_skills = true
telemetry = true

[providers.deepseek]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = ""

[models."deepseek-flash"]
provider = "deepseek"
model = "deepseek-flash"
max_context_size = 131072

[loop_control]
max_retries_per_step = 10
reserved_context_size = 50000
# Per-turn stall watchdog window, in milliseconds. Default is 600000
# (600 s); 0 disables the watchdog. Example: raise to 900 s for models
# with long thinking phases and no streaming output.
# turnStallTimeoutMs = 900000

[background]
# all of these fields can also live under [task], which overrides them per field
max_running_tasks = 4
keep_alive_on_exit = false

[[permission.rules]]
decision = "allow"
pattern = "Read"

[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"

[[hooks]]
event = "PreToolUse"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/check-bash.mjs"
timeout = 5
```

## Top-level fields

Fields in the config file fall into two categories: **top-level scalars** that directly control default behavior, and **nested tables** (`providers`, `models`, `loop_control`, etc.) that each have their own structure, described individually in the sections below.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `default_model` | `string` | — | Default model alias; must be defined in `models` |
| `default_provider` | `string` | — | Default provider key in the `providers` map |
| `default_permission_mode` | `string` | `manual` | Default permission mode for new sessions; one of `manual` (prompt each time), `auto` (auto-approve read operations), or `yolo` (auto-approve everything) |
| `default_plan_mode` | `boolean` | `false` | Whether new sessions start in Plan mode (produce a plan before executing) by default |
| `merge_all_available_skills` | `boolean` | `true` | Whether to merge Agent Skills from all available directories |
| `extra_skill_dirs` | `array<string>` | — | Extra skill search directories, layered on top of the default directories |
| `extra_agent_dirs` | `array<string>` | — | Extra agent profile search directories for custom agent discovery |
| `disabled_agents` | `array<string>` | — | Deprecated — no longer controls agent enablement; kept parsed only so old config files keep loading (a non-empty value logs a deprecation warning at startup). Disable agents with `enabled: false` in the agent profile file → [Enabling and Disabling Agents](../customization/agents.md#enabling-and-disabling-agents) |
| `telemetry` | `boolean` | `true` | Whether anonymous telemetry is enabled; disabled only when explicitly set to `false` |
| `disable_model_prompts` | `boolean` | `false` | Set to `true` to stop injecting any model's dedicated prompt into the system prompt. Existing prompt files stay on disk — this switch stops injection only, not generation or editing → [Model-specific prompts](#model-specific-prompts) |
| `providers` | `table` | `{}` | API provider table → [`providers`](#providers) |
| `models` | `table` | — | Model alias table → [`models`](#models) |
| `thinking` | `table` | — | Default parameters for Thinking mode → [`thinking`](#thinking) |
| `loop_control` | `table` | — | Agent loop control parameters → [`loop_control`](#loop_control) |
| `background` | `table` | — | Background task runtime parameters → [`background`](#background) |
| `task` | `table` | — | Background task lifecycle parameters (the current spelling) → [`task`](#task) |
| `services` | `table` | — | Built-in external service configuration → [`services`](#services) |
| `permission` | `table` | — | Initial permission rules → [`permission`](#permission) |
| `hooks` | `array<table>` | — | Lifecycle hooks; see [Hooks](../customization/hooks.md) |

The following sections cover each of the nested tables in turn: `providers`, `models`, `thinking`, `loop_control`, `background`, `task`, `subagent`, `experimental`, `services`, and `permission`.

## `providers`

Each entry in the `providers` table defines an API provider, keyed by a unique name. When the engine builds a connection, the API key is resolved in this order:

1. The provider's active credential-store entry (managed by the UI — Settings → Providers, or `/provider` in the TUI);
2. Otherwise, the provider type's conventional key read from the `[providers.<name>.env]` sub-table — `ANTHROPIC_API_KEY` for `type = "anthropic"`, `OPENAI_API_KEY` for `openai` / `openai_responses`, `GOOGLE_API_KEY` for `google` / `google-genai`, and `MIRRI_API_KEY` for `kimi`;
3. If neither is set, startup fails with a clear configuration error. Shell environment variables are never consulted implicitly — if you previously relied on `export ANTHROPIC_API_KEY=xxx`, move the key into the `env` sub-table (or the credential store).

The `base_url` follows the same chain: the `base_url` config field first, then the same conventional key in the `env` sub-table (`ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`, `GOOGLE_GEMINI_BASE_URL`, `MIRRI_BASE_URL`), then the type's built-in default endpoint — first-party providers only; every other provider must configure a base URL explicitly.

See [Config overrides](./overrides.md#provider-credentials) for the full priority rules.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `type` | `string` | Recommended | Provider type: `kimi`, `anthropic`, `openai`, `openai_responses`, `google-genai`. Defaults to `openai` when omitted. `vertexai` is recognized by the TypeScript engine only — the rust engine rejects it at connection time |
| `api_key` | `string` | No | API key, written in plain text in the config file |
| `base_url` | `string` | No | API base URL |
| `oauth` | `table` | No | OAuth credential reference (`storage` and `key` fields); normally managed by the CLI — no need to write this by hand |
| `env` | `table<string, string>` | No | Extra key-value pairs passed to the provider adapter; values support `${VAR}` expansion. Provider-conventional key names (such as `MIRRI_API_KEY`) also serve as the credential source for `api_key` / `base_url` — see below |
| `custom_headers` | `table<string, string>` | No | Custom HTTP headers attached to each request |
| `removed_model_ids` | `array<string>` | No | Denylist of upstream model ids; catalog refreshes never re-add a model listed here |
| `thinking_off` | `string` | No | How this endpoint expresses "stop thinking"; same values as the model-level key. Applies to every model under the provider that does not override it |
| `media_file_refs` | `boolean` | No | `anthropic` provider only. Allow image uploads through the provider's Files API and reference by file id instead of inlining base64; defaults to `true` |

**`env` sub-table**: You can write provider-conventional key names (such as `MIRRI_API_KEY`) inside `[providers.<name>.env]` — the explicit configuration tier the engine reads for `api_key` / `base_url` when the credential store has no active entry. The sub-table itself is read from the config file only and does not modify the shell environment:

```toml
[providers.kimi.env]
MIRRI_API_KEY = "sk-xxx"
MIRRI_BASE_URL = "https://api.moonshot.ai/v1"
```

Priority: credential store > `env` sub-table key > if neither is set, startup fails with a clear configuration error. The same chain applies to `base_url`, ending in the built-in default URL (first-party providers only) instead of an error.

## `models`

Each entry in the `models` table defines a model alias (the name used in `default_model` or the `-m` flag), keyed by a unique name.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `provider` | `string` | Yes | Name of the provider to use; must be defined in `providers` |
| `model` | `string` | Yes | Model identifier sent to the server when calling the API |
| `max_context_size` | `integer` | Yes | Maximum context length in tokens; must be at least 1 |
| `max_output_size` | `integer` | No | Per-request output token cap (maps to `max_tokens`). Currently only the `anthropic` provider honors it. When set for a Claude model, this explicit value overrides the built-in server-side maximum |
| `capabilities` | `array<string>` | No | Capability tags to add explicitly: `thinking`, `always_thinking`, `image_in`, `video_in`, `audio_in`, `tool_use`. Unioned with the capabilities auto-detected by the provider — entries can only be added, never removed |
| `support_efforts` | `array<string>` | No | Thinking effort levels the model supports for extended thinking (e.g. `["low", "high", "max"]`) |
| `default_effort` | `string` | No | Default thinking effort for the model |
| `thinking_off` | `string` | No | `openai` providers only. Which field carries "stop thinking" to this endpoint: `thinking_type` / `reasoning_effort` / `enable_thinking`. Omit to infer it from the endpoint (the builtin table covers MiMo, DeepSeek, Zhipu, Kimi, Qwen, MiniMax) |
| `display_name` | `string` | No | Name shown in the UI; falls back to `model` when unset |
| `description` | `string` | No | Short capability description exposed to the LLM in the `Agent`/`AgentSwarm` tool description. When set, this model appears in the "Available models" list, allowing the LLM to make informed model-selection decisions when dispatching subagents. Models without `description` remain valid for `default_model` but are not offered as LLM-selectable overrides |
| `reasoning_key` | `string` | No | `openai` provider only. Override the field name used for reasoning content when the gateway returns it under a non-standard name; by default `reasoning_content`, `reasoning_details`, and `reasoning` are auto-detected |
| `adaptive_thinking` | `boolean` | No | `anthropic` provider only. Force adaptive thinking on or off, overriding the version inference based on the model name. Omit to infer automatically (Claude ≥ 4.6 uses adaptive) |
| `protocol` | `string` | No | Force the wire protocol (`anthropic`, `openai`, `openai_responses`, `google-genai`), overriding what the provider type implies |
| `beta_api` | `boolean` | No | Route requests through the provider's beta API endpoint |

When an alias contains `.`, use a quoted key:

```toml
[models."gpt-4.1"]
provider = "openai"
model = "gpt-4.1"
max_context_size = 1047576
```

### Catalog refresh behavior

Provider-model refreshes (startup, every 6 hours, or manual "Refresh All") only **append** new models discovered in the catalog. Existing model aliases are never modified or deleted — your configuration is your own. Models you delete are remembered on a per-provider denylist (`removed_model_ids`) so a refresh cannot silently re-add them.

You can also switch models temporarily without touching the config file — by setting `MIRRICODE_MODEL_*` environment variables, the CLI synthesizes a temporary provider in memory that does not persist after restart. See [Define a model from environment variables](./env-vars.md#define-a-model-from-environment-variables-mirricode-model).

## Model-specific prompts

Some models are noticeably verbose in a particular way — long reasoning, slow to act. The product ships a corrective prompt for such models and materializes it as a plain Markdown file **you can open and edit**, rather than a black box inside the program.

### Location and naming

```
~/.mirri-code/model_prompts/<model-name>.md
```

One file per **raw model name** (not per alias). When several providers reach the same model they share this one file — a model's traits belong to the model, not to the way it is reached.

A file looks like this:

```markdown
---
model: "mimo-v2.6-pro"
---
<prompt body>
```

The `model:` field declares which model the file belongs to and is the file's only identity; the file name is for humans. If two files declare the same model, the lexicographically first name wins and a diagnostic is logged.

### When a file is created

- When a model enters your configuration and matches a built-in rule — importing a provider, adding a provider, adding a model, or renaming a model's model id all count;
- or when a new session assembles its prompt and finds a model that matches a rule but has no file yet (the backstop for models configured before the rule existed).

The built-in rules currently cover `mimo-v2.6-*` (such as `mimo-v2.6-pro` and `mimo-v2.6-flash`), keeping reasoning within what the task needs, moving straight to the next action, and planning at most three tool actions per round.

### Three guarantees

1. **An existing file is never overwritten**, whether or not you edited it. Product updates to the built-in wording only affect models added afterwards — your tuning is never silently changed.
2. **An empty body means "this model needs no extra prompt"**. It is not refilled.
3. **Injection happens when a session is created**. Switching models mid-session does not change the running session's system prompt; new sessions (and resumed ones) use the latest content.

### Writing your own prompt for a model

Any model's file can be viewed and edited in settings (Models → a model → Model-specific prompt); models that match no rule can be created there too. Leave the body empty to give the model no extra prompt.

A body over 16 KiB is **not injected at all** (not truncated — half a directive is more dangerous than none), and the assembly diagnostic names the file and the excess.

### Turning it off

```toml
disable_model_prompts = true
```

This turns off **injection**: existing prompt files stay on disk, new models still get files, and the editor remains available. After turning it back on, sessions created or resumed from then on inject whatever the files currently contain.

## `loop_control`

`loop_control` governs the step count limit, per-step retry count, and the threshold that triggers automatic context compaction in the Agent execution loop.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_steps_per_turn` | `integer` | — | Maximum steps per turn; unset or `0` means unlimited |
| `max_retries_per_step` | `integer` | `10` | Maximum retries after a step failure |
| `reserved_context_size` | `integer` | — | Number of tokens reserved for model output; automatic compaction is triggered when the remaining context window falls below this value |
| `compaction_trigger_ratio` | `number` | — | Context-usage ratio at which automatic compaction is triggered |
| `max_overflow_compaction_attempts` | `integer` | — | How many compaction retries are attempted after a context overflow before the error surfaces; `0` fails on the first overflow |
| `max_history_messages` | `integer` | `10000` | Cap on messages in the canonical history; `0` rejects all further appends |
| `max_deferred_messages` | `integer` | `1000` | Cap on messages buffered while a tool call is still open; `0` rejects all further buffering |
| `retention_tail_ratio` / `retention_tail_tokens` | `number` / `integer` | — | Retention-tail budget for compaction, as a ratio of the context window or an absolute token count. Mutually exclusive — setting both fails config load; the ratio must satisfy `0 < ratio < 1` |
| `turnStallTimeoutMs` | `integer` | engine default `600000` (600 s) | Per-turn stall watchdog window, in milliseconds — a turn that emits no live event (LLM delta, tool event, retry, …) for this long is cooperatively cancelled by the engine. Unset keeps the 600 s engine default; `0` disables the watchdog entirely. Keep this value **above** the provider HTTP read timeout (240 s): a silently hung model stream then trips the provider's retryable read error first, with the watchdog as the engine-side backstop. Setting it below `240000` logs a startup warning (not a rejection). Read once at engine construction — restart to apply config changes |

## `background`

`background` controls the concurrency behavior of background tasks (launched via the `Bash` tool or the `Agent` tool's `run_in_background=true` parameter). The `background` section is the legacy spelling — current configurations should use [`task`](#task), which overrides it per field.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_running_tasks` | `integer` | — | Maximum number of background tasks running concurrently |
| `keep_alive_on_exit` | `boolean` | `false` | Whether to keep still-running background tasks when the session closes. By default, Mirri Code requests that all background tasks stop before the process exits; set this to `true` only when you want tasks to outlive the session. In print mode (`mirri -p`), this is only a legacy fallback used when `print_background_mode` is unset: `true` is equivalent to `print_background_mode = "drain"` |
| `bash_task_timeout_s` | `integer` | `600` | Default timeout (seconds) for background Bash tasks when the call omits `timeout`; foreground commands moved to the background on timeout are also re-armed with this value. `0` means no timeout — the task runs until it finishes or is manually stopped. Explicit per-call `timeout` values are unaffected. Defaults to `0` in print mode (`mirri -p`) when not explicitly set |
| `print_background_mode` | `"exit" \| "drain" \| "steer"` | `"steer"` | Print mode (`mirri -p`) only. Governs how pending background tasks are handled once the main agent's turn ends: `"exit"` exits immediately; `"drain"` waits for every background task to reach a terminal state before exiting (results are not fed back to the main agent); `"steer"` stays alive so a completing background task — like a background subagent — injects a synthetic user message that steers the main agent into a new turn, looping until a turn ends with no pending background tasks or a limit is hit. Takes precedence over the `keep_alive_on_exit` print fallback |
| `print_wait_ceiling_s` | `integer` | `315360000` | In print mode (`mirri -p`), the wall-clock ceiling (seconds) for the wait/steer loop when `print_background_mode` is `"drain"` or `"steer"` (default 10 years, effectively unbounded). Has no effect outside print mode or when it is `"exit"` |
| `print_max_turns` | `integer` | `100000` | In print mode (`mirri -p`) with `print_background_mode = "steer"`, the maximum number of new turns that may be triggered by background-task completions, to keep the steering loop bounded (default effectively unbounded) |

`keep_alive_on_exit` can be overridden by the `MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT` environment variable, which takes higher priority than `config.toml`.

In print mode (`mirri -p "<prompt>"`), Mirri Code stays alive by default as long as there are pending background tasks: each task completion feeds back to the main agent as a synthetic user message, steering new turns (default `print_background_mode = "steer"`), until a turn ends with no pending tasks. This loop is bounded by `print_wait_ceiling_s` and `print_max_turns`, both effectively unbounded by default. Background work is also not killed by wall-clock timeouts in print mode: background Bash tasks default to no timeout (`bash_task_timeout_s = 0`), subagents default to no timeout (`[subagent] timeout_ms = 0`), and only the model itself can stop a task. Set `print_background_mode` to `"drain"` to wait for tasks without feeding results back, or `"exit"` to exit immediately after the main agent finishes.

## `task`

`task` is the current home for background-task lifecycle settings. It supersedes the legacy `background` section: when both are present, `task` overrides `background` **per field** — a `task` key that is unset falls through to the `background` value. New configurations should write `task` directly.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_running_tasks` | `integer` | `0` (unbounded) | Maximum number of background tasks running concurrently; values below `1` are rejected at load |
| `keep_alive_on_exit` | `boolean` | `false` | Whether still-running background tasks survive session close |
| `bash_task_timeout_s` | `integer` | `600` | Default timeout (seconds) for background Bash tasks; `0` means no timeout |
| `bash_auto_background_on_timeout` | `boolean` | — | Move a foreground Bash command to the background instead of killing it when it hits the timeout |
| `kill_grace_period_ms` | `integer` | `5000` | Grace window before an escalated stop settles the task as `stopped` |
| `print_background_mode` | `"exit" \| "drain" \| "steer"` | `"steer"` | Print-mode (`mirri -p`) background strategy; same semantics as the `background` spelling |
| `print_wait_ceiling_s` | `integer` | `315360000` | Wall-clock ceiling (seconds) for the drain/steer wait loop |
| `print_max_turns` | `integer` | `100000` | Maximum new turns triggered by background completions in steer mode |
| `heartbeat_interval_ms` | `integer` | `300000` | How long (milliseconds) a running task may go without activity before being flagged stale; `0` disables staleness detection |

## `subagent`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `timeout_ms` | `integer` | `7200000` (2 hours) | Maximum wall-clock time (milliseconds) a single subagent (`Agent` / `AgentSwarm`) is allowed to run before it is settled as `timed_out`. `0` means no timeout — the subagent runs until it finishes or is manually stopped. This is the background-task manager's per-task timeout for each subagent task, so it applies to both foreground and background subagents. Defaults to `0` in print mode (`mirri -p`) when not explicitly set. Note: any value above `2147483647` (about 24.8 days) is clamped to ~24.8 days by the runtime. |
| `max_nesting_depth` | `integer` | `5` | Maximum subagent nesting depth (the main agent is depth 1). `0` means unlimited |
| `max_swarm_items` | `integer` | `3` | Maximum number of members a single `AgentSwarm` dispatch may launch. `0` means unlimited. Overridable by the `MIRRI_AGENT_SWARM_MAX_ITEMS` environment variable (env > config > default) |

`timeout_ms` can be overridden by the `MIRRICODE_SUBAGENT_TIMEOUT_MS` environment variable, which takes higher priority than `config.toml`.

Both `max_nesting_depth` and `max_swarm_items` are enforced at the spawn gate — a dispatch that exceeds either is refused with a message naming the limit.

## `experimental`

`experimental` stores persistent overrides for experimental-feature flags. Flags default to `false`; set a flag to `true` to enable it.

::: warning
Engine support for experimental flags differs: the rust engine currently implements `hook-command-rewrite` and `session-consistency-follow`; `tool-select` is only honored by the TypeScript engine.
:::

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `tool-select` | `boolean` | `false` | Keep MCP tool schemas out of the top-level tools array; the model loads them on demand |
| `hook-command-rewrite` | `boolean` | `false` | Allow `RewriteToolInput` hooks to modify tool arguments before execution (e.g. rtk integration) |
| `session-consistency-follow` | `boolean` | `false` | On session resume, spawn a driver that incrementally absorbs appends written to the session journal by other processes |

## `services`

`services` configures two built-in services: web search (`moonshot_search`) and web fetch (`moonshot_fetch`). Only these two fixed keys are recognized; other keys are ignored. Both entries share the same fields:

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `base_url` | `string` | No | Service API URL |
| `api_key` | `string` | No | API key |
| `oauth` | `table` | No | OAuth credential reference, same structure as `providers.*.oauth` |
| `custom_headers` | `table<string, string>` | No | Custom HTTP headers attached to each request |

```toml
[services.moonshot_search]
base_url = "https://api.moonshot.cn/v1/search"
api_key = "sk-xxx"

[services.moonshot_fetch]
base_url = "https://api.moonshot.cn/v1/fetch"
api_key = "sk-xxx"
```

## `permission`

`permission` sets permission rules that are automatically loaded when a session starts, controlling whether the Agent needs user confirmation before calling a tool. Rules are written as a `[[permission.rules]]` array of tables, matched in order — the first matching rule takes effect.

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `decision` | `string` | Yes | Action on match: `allow` (permit immediately), `deny` (reject immediately), `ask` (prompt each time) |
| `scope` | `string` | No | Rule scope: `turn-override`, `session-runtime`, `project`, `user`; defaults to `user` |
| `pattern` | `string` | Yes | Match pattern in the form `ToolName` or `ToolName(arg-pattern)`, e.g. `Read` or `Bash(rm -rf*)` |
| `reason` | `string` | No | Rule description for debugging and auditing |

Built-in tool names are listed in [Built-in tools](../reference/tools.md). Most built-in tools that accept rule arguments define their own matching subject, such as `Bash(command-pattern)` or `Read(path-pattern)`. `AgentSwarm`, MCP tools, and custom tools can only be matched by tool name — argument patterns are not supported for them.

```toml
[[permission.rules]]
decision = "allow"
pattern = "Read"

[[permission.rules]]
decision = "allow"
pattern = "Grep"

[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"

[[permission.rules]]
decision = "ask"
pattern = "Bash"
```

::: tip
MCP server declarations are configured in `~/.mirri-code/mcp.json` or the project-local `.mirri-code/mcp.json`, not in `config.toml`. The interactive configuration entry point is `/mcp-config`; see [Model Context Protocol](../customization/mcp.md).
:::

## `tui.toml`

Alongside `config.toml`, the CLI keeps terminal-UI and client preferences in a companion `tui.toml` in the same directory (`~/.mirri-code/tui.toml`, or `$MIRRICODE_HOME/tui.toml` when overridden). It is created with defaults on first run, and the interactive commands `/config`, `/theme`, and `/editor` write to it for you — so you rarely need to edit it by hand. If the file is malformed, the CLI falls back to defaults and shows a notice instead of failing to start.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `theme` | `string` | `auto` | Color theme: `auto` (follow the terminal), `dark`, `light`, or the name of a [custom theme](../customization/themes) |
| `disable_paste_burst` | `boolean` | `false` | Disable the non-bracketed paste-burst fallback that keeps rapid multi-line pastes from submitting line by line |
| `[editor].command` | `string` | `""` | External editor command for composing long input; empty falls back to `$VISUAL` / `$EDITOR` |
| `[notifications].enabled` | `boolean` | `true` | Whether desktop notifications are sent |
| `[notifications].notification_condition` | `string` | `unfocused` | When to notify: `unfocused` (only when the terminal is not focused) or `always` |
| `[upgrade].auto_install` | `boolean` | `true` | Whether new versions are installed automatically |

```toml
# ~/.mirri-code/tui.toml
theme = "auto" # "auto" | "dark" | "light" | custom theme name
disable_paste_burst = false # true disables non-bracketed paste-burst fallback

[editor]
command = "" # empty uses $VISUAL / $EDITOR

[notifications]
enabled = true
notification_condition = "unfocused" # "unfocused" | "always"

[upgrade]
auto_install = true
```

Changes apply on the next start, or immediately with `/reload-tui` (which reloads only `tui.toml`); `/reload` reloads both `config.toml` and `tui.toml`.

## Project-local configuration

In addition to the user-level files under `~/.mirri-code`, Mirri Code reads a project-local configuration file at `<project-root>/.mirri-code/local.toml`. It holds settings that are specific to one project checkout and typically should not be shared with teammates.

The file is created automatically when you add an extra workspace directory with [`/add-dir`](../reference/slash-commands.md) and choose to remember it for the project. You rarely need to edit it by hand.

### `[workspace]`

The `[workspace]` table groups project-level workspace settings:

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `additional_dir` | `array<string>` | No | Additional workspace directories, stored as absolute paths. Written automatically when you confirm "remember this directory" in `/add-dir`; read back on startup so the directories are available in every session of this project |

```toml
[workspace]
additional_dir = ["/absolute/path/to/shared"]
```

Because directories are stored as absolute paths, which are specific to your machine, we recommend adding `.mirri-code/local.toml` to your project's `.gitignore` so it is not committed.

## Next steps

- [Providers and models](./providers.md) — connection examples for each provider type (Mirri, Claude, OpenAI, Gemini)
- [Config overrides](./overrides.md) — priority rules for CLI options, config file, and environment variables
- [Environment variables](./env-vars.md) — complete list of runtime variables like `MIRRICODE_HOME`
