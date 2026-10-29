# `mirri` Command

`mirri` is the Mirri Code engine binary. It runs the agent locally and serves the interfaces that connect to it — the desktop app, the built-in web UI, and ACP clients all talk to the same engine.

The binary has **no interactive terminal UI**. Running `mirri` with no arguments prints its help text. To work in a browser, use [`mirri web`](#mirri-web); to run a single prompt from a script, use [`mirri print`](#mirri-print).

```sh
mirri <command> [options]
```

::: tip Coming from a pre-2.0 install?
Earlier releases shipped a terminal UI. The Rust engine replaced it, and the engine no longer provides an interactive terminal mode — `mirri web` and the desktop app are the interactive front ends. Run [`mirri migrate`](#mirri-migrate) to carry existing sessions over.
:::

## Commands

| Command | Purpose |
| --- | --- |
| [`mirri web`](#mirri-web) | Start the local server, serve the web UI, and open it in the browser |
| [`mirri serve`](#mirri-serve) | Start the local server on its own (API-first, for front ends and IPC clients) |
| [`mirri print`](#mirri-print) | Run one prompt non-interactively and stream the reply to stdout |
| [`mirri acp`](#mirri-acp) | Run the Agent Client Protocol host for IDE clients (Zed, JetBrains) |
| [`mirri migrate`](#mirri-migrate) | Copy legacy TypeScript-engine sessions into the Rust engine's storage root |
| [`mirri session-list`](#mirri-session-list) | List the sessions stored on this machine |
| [`mirri config-show`](#mirri-config-show) | Print the configuration the engine resolves |
| [`mirri doctor`](#mirri-doctor) | Validate `config.toml` against the engine's schema |

Global options: `-h` / `--help` shows help for the binary or a command, and `-V` / `--version` prints the engine version.

## `mirri web`

Starts the local server and opens the web UI in your default browser. This is the command to use when you want to interact with Mirri Code from the terminal.

```sh
mirri web                 # start the server and open the browser
mirri web --no-open       # start the server without opening a browser
mirri web --port 8080     # use a different port
```

The server runs in the foreground: the command stays attached to your terminal, and `Ctrl-C` stops it.

| Option | Default | Description |
| --- | --- | --- |
| `--host <host>` | `127.0.0.1` | Bind address. Accepts an IP literal or a hostname |
| `--port <port>` | `58627` | Bind port. The command exits with a bind error when the port is already taken — pick another one explicitly |
| `--workspace <dir>` | current directory | Workspace root to open. When the current directory is the filesystem root (a packaged app can inherit `/`), this falls back to your home directory |
| `--web-dir <dir>` | embedded UI | Directory holding a built web frontend to serve instead of the bundled copy |
| `--no-open` | off | Do not launch a browser |
| `--log-level <level>` | `info` | Log filter, `RUST_LOG` style |
| `--auth-token <token>` | `<mirri-home>/server.token` | Bearer token for the HTTP API, created on first boot when absent |
| `--dangerous-bypass-auth` | off | Disable Bearer-token auth entirely |

The web UI is served from the copy embedded in the binary. A build that ships without it probes for a frontend staged next to the executable, and `--web-dir` overrides both.

## `mirri serve`

Starts the same server as `mirri web`, without the web-UI conveniences: no browser is launched, and the SPA is only served when you pass `--web-dir`. Use it when something else provides the front end — the desktop app, a remote client, or your own tooling.

```sh
mirri serve
mirri serve --ipc-path /tmp/mirri.sock --port 58627
```

| Option | Default | Description |
| --- | --- | --- |
| `--host <host>` | `127.0.0.1` | Bind address |
| `--port <port>` | `58627` | Bind port |
| `--ipc-path <path>` | off | Unix socket path for IPC clients. Omitting it disables IPC |
| `--workspace <dir>` | most recently opened workspace | Workspace root to serve. Without this flag the server serves every workspace in the registry and makes the most recently opened one the default; an empty registry falls back to the current directory |
| `--web-dir <dir>` | off | Serve a built web frontend alongside the API |
| `--log-level <level>` | `info` | Log filter, `RUST_LOG` style |
| `--auth-token <token>` | `<mirri-home>/server.token` | Bearer token for the HTTP API |
| `--dangerous-bypass-auth` | off | Disable Bearer-token auth entirely |

## Authentication

Both server commands enable Bearer-token auth by default. When `--auth-token` is omitted, the token is loaded from `<mirri-home>/server.token`, which is created on first boot — the same file the desktop app and web UI use. With the token in place, every route except `/api/v1/healthz` and `/api/v1/meta` requires `Authorization: Bearer <token>`.

`--dangerous-bypass-auth` disables that check. Every route except `/api/v1/shutdown` — which stays limited to loopback peers — becomes unauthenticated.

::: danger
Only use `--dangerous-bypass-auth` on a trusted network, or behind a reverse proxy that authenticates requests itself. Anyone who can reach the port gets full access to your sessions, files, and shell otherwise.
:::

## `mirri print`

Sends one prompt and streams the reply to stdout, then exits. Intended for scripts, CI, and quick one-off questions.

```sh
mirri print "Summarize the current repository status"
mirri print --model claude-sonnet-4-5 "Explain the latest diff"
mirri print --provider openai --model gpt-5 "List the TODO comments in src/"
```

| Option | Default | Description |
| --- | --- | --- |
| `<PROMPT>` | required | The prompt to send |
| `--model <model>` | engine default | Model ID to use for this run |
| `--provider <type>` | `anthropic` | Provider type: `anthropic` or `openai`. Requires `--model` as well |

The provider comes from the environment: `anthropic` reads `ANTHROPIC_API_KEY` (and optional `ANTHROPIC_BASE_URL`), while `openai` reads `OPENAI_API_KEY` (and optional `OPENAI_BASE_URL`).

Assistant text goes to stdout and is flushed as it arrives; approval and question prompts go to stderr. The command exits `1` when the turn fails. The session is in-memory and is discarded when the process exits, so `mirri print` does not add to your session history.

::: warning
`mirri print` installs no approval responder. A tool call that would need approval blocks until the engine's 300-second turn timeout. Prefer `mirri web` for anything that writes files or runs shell commands.
:::

## `mirri acp`

Runs the Agent Client Protocol (ACP) host on stdin/stdout — the subprocess entry point IDE clients (Zed, JetBrains, custom front ends) launch to drive Mirri Code sessions over JSON-RPC 2.0. Logs go to stderr, keeping the protocol channel clean. The engine is rooted at the current working directory; set `MIRRI_ACP_REMOTE_ENGINE` to connect to a remote engine instead. When the desktop app is running, its engine is reused automatically (sessions sync to the desktop UI live). See the [`mirri acp` reference](./mirri-acp.md) and [Using in IDEs](../guides/ides.md) for configuration details.

## `mirri migrate`

Copies sessions written by the retired TypeScript engine into the Rust engine's storage root, so they resume normally. Migrated sessions are registered so the engine can route them after the next server start.

```sh
mirri migrate                                  # migrate the global home sessions
mirri migrate --workspace ~/projects/app       # also migrate that workspace's sessions
mirri migrate --dry-run                        # report what would be copied
```

| Option | Description |
| --- | --- |
| `--workspace <dir>` | Also migrate `<dir>/.mirri/sessions/`. Omit to migrate only `<mirri-home>/sessions/` |
| `--dry-run` | Report what would be migrated without changing anything |

Migration copies rather than moves — the legacy root stays readable by the old engine — and verifies each copy. It is idempotent: sessions already present in the new root are skipped and reported. Wire protocol versions are upgraded lazily, when a session is actually resumed.

## `mirri session-list`

Lists the sessions stored on this machine, newest first, reading both the Rust engine's storage root and the legacy TypeScript one.

```sh
mirri session-list
mirri session-list --workspace ~/projects/app
```

| Option | Default | Description |
| --- | --- | --- |
| `--workspace <dir>` | current directory | Workspace directory to inspect |

## `mirri config-show`

Prints the configuration the engine resolves, so you can confirm what is actually in effect.

```sh
mirri config-show
mirri config-show --workspace ~/projects/app
```

| Option | Default | Description |
| --- | --- | --- |
| `--workspace <dir>` | current directory | Workspace directory to inspect |

When no `config.toml` exists, the command says so and reports that built-in defaults apply.

## `mirri doctor`

Validates `config.toml` against the engine's schema without starting anything or modifying the file.

```sh
mirri doctor                             # validate the default config.toml
mirri doctor --config ./candidate.toml   # validate a file before installing it
```

| Option | Description |
| --- | --- |
| `--config <path>` | Validate this file instead of the default `<mirri-home>/config.toml` |

A missing default file is reported as skipped, because built-in defaults can apply. A path passed with `--config` must exist. The command exits `0` when everything it checked is valid or skipped, and `1` when a file is invalid or a requested file is missing.

## Related binaries

`mirri` is not the only executable the project ships:

- **Desktop app** — bundles the same engine and adds a window, native menus, and app integrations.

## Next steps

- [Configuration Files](../configuration/config-files.md) — persistent configuration for the model, permission mode, and other startup parameters
- [Data Locations](../configuration/data-locations.md) — where sessions, logs, and config live on disk
- [Agent Skills](../customization/skills.md) — skill file format and where they are discovered
