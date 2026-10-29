# `mirri acp` Subcommand

`mirri acp` switches Mirri Code to **ACP (Agent Client Protocol)** mode: it communicates with an ACP client (such as Zed, JetBrains AI Chat, etc.) via JSON-RPC over stdin/stdout, letting the IDE directly drive mirri's sessions, prompts, and tool calls.

```sh
mirri acp
```

Once started, the command prints no banner and immediately waits for the ACP client to send an `initialize` request on stdin. Logs are written to stderr (as well as the diagnostic log under `~/.mirri-code/logs/`), so the ACP channel itself stays clean.

::: tip Who calls this?
You typically do not need to run `mirri acp` manually — this command is the subprocess entry point for IDEs. For IDE-side configuration, see [Using in IDEs](../guides/ides.md).
:::

## Capability Matrix

The table below lists the capabilities declared by the current ACP adapter layer. The `agentCapabilities` field is returned in full in the `initialize` response, so the IDE can adjust its UI accordingly.

| Capability | Value | Description |
| --- | --- | --- |
| `promptCapabilities.image` | `true` | Supports ACP `image` content blocks (base64 + mimeType) |
| `promptCapabilities.audio` | `false` | Audio prompts not yet supported |
| `promptCapabilities.embeddedContext` | `true` | Client may send `resource`/`resource_link` embedded resource blocks; text content is injected into the prompt as `<resource uri="...">...</resource>`; blob resources are dropped with a warn |
| `mcpCapabilities.http` | `true` | Forwards HTTP MCP services configured by the IDE |
| `mcpCapabilities.sse` | `true` | Forwards legacy SSE MCP services configured by the IDE |
| `loadSession` | `true` | Supports `session/load` to resume an existing session, replaying history on load |
| `sessionCapabilities.list` | `{}` | Supports `session/list` to enumerate the current user's sessions |

## ACP Method Coverage

The spec divides methods into a **stable** surface and an evolving **unstable** surface (handlers mounted with the `unstable_*` prefix in `@agentclientprotocol/sdk@0.23.0`). The two have entirely different stability guarantees — the stable surface covers methods every production ACP client uses, while the unstable surface covers experimental extensions (inline-edit prediction, document buffer sync, provider management, elicitation, etc.) — so they are tracked separately.

**Summary: stable agent-side 10/12 (83%) + client reverse-RPC 4/9 (44%); unstable surface has only `session/set_model` (1/19).** All methods needed for a normal agent flow (initialize → new/load/resume → prompt → cancel + file I/O + tool approval) are implemented.

### Stable agent-side — IDE → agent (10 / 12)

| Method | Implemented | Description |
| --- | --- | --- |
| `initialize` | Yes | Version negotiation; returns `agentInfo: { name: 'Mirri Code', version }`, capability matrix, and `authMethods` |
| `authenticate` | Yes | Managed login is retired: no auth methods are advertised; every `method_id` is rejected with `invalidParams (-32602)` |
| `session/new` | Yes | Accepts `cwd` / `mcpServers`; returns `configOptions[]` |
| `session/load` | Yes | Restores a session from disk and replays history via `session/update` |
| `session/resume` | Yes | Lightweight sibling of `session/load`; skips history replay |
| `session/prompt` | Yes | Accepts `text` / `image` / `resource` / `resource_link` content blocks; streams `agent_message_chunk` |
| `session/cancel` | Yes | Interrupts the current turn |
| `session/list` | Yes | Enumerates sessions on disk (advertised via `sessionCapabilities.list = {}`) |
| `session/set_mode` | Yes | Compatibility path; dispatches to the same handler as `set_config_option({configId:'mode'})` |
| `session/set_config_option` | Yes | Unified model / thinking / mode picker dispatcher |
| `session/close` | No | |
| `logout` | No | |

### Stable client-side reverse-RPC — agent → IDE (4 / 9)

| Method | Implemented | Description |
| --- | --- | --- |
| `session/update` | Yes | Streams `agent_message_chunk` / `tool_call*` / `plan` / `config_option_update` / `available_commands_update` |
| `session/request_permission` | Yes | Shared channel for tool approval and question elicitation |
| `fs/read_text_file` | Yes | Engine file reads are routed to the client (advertised via `fsCapabilities`) |
| `fs/write_text_file` | Yes | Engine file writes are routed to the client |
| `terminal/create` · `output` · `release` · `kill` · `wait_for_exit` | No | Terminal reverse-RPC not connected; shell commands use local execution |

### Unstable surface (1 / 19)

| Method | Implemented | Description |
| --- | --- | --- |
| `session/set_model` | Yes | Compatibility path; equivalent to `set_config_option({configId:'model'})` |
| Remaining 18 methods | No | Includes session lifecycle extensions, buffer sync, inline-edit prediction, provider management, etc. |

All methods not listed above return `methodNotFound`.

## MCP Forwarding

When an ACP client provides `mcpServers` in `session/new` or `session/load`, the adapter layer performs the following conversions:

- `http` → mirri's `transport: 'http'` configuration
- `stdio` → mirri's `transport: 'stdio'` configuration
- `sse` → mirri's `transport: 'sse'` configuration
- `acp` → discarded with a warn log entry

## Engine selection and desktop sharing

When `mirri acp` starts, it automatically probes for a locally running desktop service (the backend of the Mirri Code desktop app). When one is found, the ACP host reuses it as a remote client — sessions driven from the IDE run directly inside the desktop engine, and the desktop UI sees session progress (message increments, tool call progress) **live**; sessions are also stored by the desktop app. When you code in the IDE and switch to the desktop app for review, both sides are looking at the same session.

If the probe fails, it silently falls back to running standalone, with behavior identical to before — no configuration needed:

- The desktop app is not running
- The desktop app is too old, or the connection fails
- The desktop app quits mid-session and reconnection fails — in-flight turns end with a connection error, and new sessions created afterwards are served by the standalone engine

Related environment variable switches:

- `MIRRI_ACP_ENGINE=embedded`: skip the probe and always run standalone — useful for debugging or full isolation from the desktop app
- `MIRRI_ACP_REMOTE_ENGINE`: explicitly point at a remote engine and connect directly (highest priority; behavior unchanged from before)

When the desktop engine is reused, there is one directory requirement: the IDE session's directory must have been opened in the desktop app before (registered as a workspace). Creating a session in an unregistered directory is rejected, with an error prompting you to open that directory in the desktop app first.

## Next steps

- [Using in IDEs](../guides/ides.md) — Zed / JetBrains configuration steps and troubleshooting
- [`mirri` Command Reference](./mirri-command.md) — Complete subcommand list
