# Config overrides

Mirri Code CLI has three places where runtime parameters can be influenced: the config file, command-line options, and environment variables. They are not a simple "whoever has higher priority wins" relationship — the three serve different scenarios and have non-overlapping scopes:

- **Config file** stores long-term preferences (model, keys, loop control, etc.); takes effect on every startup
- **Command-line options** make one-off changes for the current startup; discarded after exit
- **Environment variables** primarily handle data directory location, OAuth endpoint switching, and a small number of runtime switches — **not a general fallback mechanism for config fields**

This distinction matters: many users run `export MIRRI_API_KEY=xxx` in the shell expecting the CLI to pick it up automatically. For ordinary runtime parameters that is not the case — but provider credentials are a deliberate exception. See [Provider credentials](#provider-credentials) below for why.

## Three roles of environment variables

Environment variables fall into three categories by function and cannot be collapsed into a single linear priority order:

1. **Locating the config file**: `MIRRICODE_HOME` sets the data root directory, making the config file path `$MIRRICODE_HOME/config.toml`. This step runs before all other resolution and is not a fallback for individual parameters.
2. **Runtime switches**: A small set of variables like `MIRRICODE_DISABLE_TELEMETRY` directly shut down the corresponding subsystem — even if `config.toml` has `telemetry = true`, setting this variable to a truthy value disables telemetry. The semantics are "additionally disable", not "ordinary override".
3. **Runtime endpoints and diagnostics**: Variables like `MIRRICODE_OAUTH_HOST`, `MIRRICODE_BASE_URL`, and `MIRRICODE_LOG_LEVEL` are read when the OAuth or logging subsystems initialize. For the full list, see [Environment variables](./env-vars.md).

## Priority for ordinary runtime parameters

For ordinary runtime parameters such as model alias, Plan mode, yolo mode, and Skills directories, priority from highest to lowest is:

1. **Command-line options** (`-m`, `--plan`, `--yolo`, etc.): apply only to the current startup
2. **User config file** (`~/.mirri-code/config.toml`): stores long-term preferences

A small number of environment variables explicitly override specific config file fields — for example, `MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT` has higher priority than `[background].keep_alive_on_exit`. These exceptions are noted in [Environment variables](./env-vars.md) and in the relevant field descriptions in [Configuration files](./config-files.md).

::: warning
**Ordinary runtime parameters do not fall back to shell environment variables.** Provider `api_key` / `base_url` are a deliberate exception: they follow their own three-step chain — the config field, then the `[providers.<name>.env]` sub-table, then the provider type's conventional shell environment variable — see [Provider credentials](#provider-credentials). The only other exception is the explicit `MIRRICODE_MODEL_*` channel — see [Define a model from environment variables](./env-vars.md#define-a-model-from-environment-variables-mirricode-model).
:::

The CLI currently reads a single user-level config file and has no project-level config file mechanism. To isolate config between different projects, point `MIRRICODE_HOME` at different data directories — see [Common scenarios](#common-scenarios) below.

## Provider credentials

Provider credentials (`api_key`, `base_url`) follow their own resolution rules, separate from the ordinary parameter priority chain.

For a single provider, credentials are resolved in this order:

1. `[providers.<name>].api_key` — key written directly in the config file; highest priority
2. The matching key inside the `[providers.<name>.env]` sub-table — `ANTHROPIC_API_KEY` for `type = "anthropic"`, `OPENAI_API_KEY` for `openai` / `openai_responses`, `GOOGLE_API_KEY` for `google` / `google-genai`, `MIRRI_API_KEY` for `kimi` — consulted only when `api_key` is empty or absent
3. The provider type's conventional shell environment variable (same key names) — consulted when both are absent; types without a conventional variable, such as `vertexai`, skip this step
4. If none is set — startup fails with an error indicating the provider is missing credentials

`base_url` is resolved the same way: first `[providers.<name>].base_url`, then the `*_BASE_URL` key in `[providers.<name>.env]`, then the same-named shell environment variable (`ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`, `GOOGLE_GEMINI_BASE_URL`, `MIRRI_BASE_URL`), and finally the provider type's built-in default URL.

> The `[providers.<name>.env]` sub-table is a TOML section in the config file — it does not write anything into the shell environment; it is only read as the second step of the credential chain. See the `env` sub-table notes in [providers](./config-files.md#providers).

For the full list of credential key names, see [Environment variables: provider credential key names](./env-vars.md#provider-credential-key-names).

## Command-line options

Options passed at startup have the highest priority and apply only to the current session:

| Option | Effect |
| --- | --- |
| `-S, --session [id]` | Resume a specific session; enters interactive selection when no id is given |
| `-c, --continue` | Resume the last session for the current working directory |
| `-y, --yolo` | Auto-approve all tool calls |
| `--plan` | Start in Plan mode |
| `-m, --model <model>` | Use a specific model alias for this session |
| `-p, --prompt <prompt>` | Run in non-interactive mode: execute a single prompt and exit |
| `--output-format <format>` | Output format for `-p` mode: `text` or `stream-json` |
| `--skills-dir <dir>` | Replace auto-discovered Skills directories (repeatable; applies to this session only) |

Mutual exclusion rules (startup fails if violated):

- `--output-format` can only be used with `-p`
- `--prompt` cannot be combined with `--yolo` or `--plan`
- `--continue` and `--session` cannot be used together
- In non-prompt mode, `--yolo` and `--plan` cannot be combined with `--continue` or `--session`

::: tip
`--skills-dir` is a one-shot replacement that only affects the current startup. To persistently add search directories, write `extra_skill_dirs` in `config.toml` (see [Agent Skills](../customization/skills.md)).
:::

## Common scenarios

**Isolated test environment** — use a separate data directory to avoid polluting the main config and sessions:

```sh
MIRRICODE_HOME="$PWD/.mirricode-sandbox" mirri
```

**One-off test key** — write a test key into the `env` sub-table so it takes precedence over any shell environment variable:

```toml
[providers.kimi.env]
MIRRI_API_KEY = "sk-test"
```

**Skip approval for batch tasks**:

```sh
mirri --yolo -p "Batch rename the following files..."
```

**Enter Plan mode temporarily** (to make it permanent, set `default_plan_mode = true` in the config file):

```sh
mirri --plan
```

## Next steps

- [Configuration files](./config-files.md) — complete reference for all configurable fields
- [Environment variables](./env-vars.md) — full list and description of `MIRRICODE_HOME` and related variables
