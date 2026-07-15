# Tool Integrations

When you connect an MCP server (a program that exposes tools over [Model Context Protocol](./mcp.md)), the Agent sees those tools alongside built-in ones like `Grep` and `Glob`. By default, the Agent has no way to know that an MCP tool serves the same purpose as a built-in — so it cannot automatically prefer the better one.

`integrations.yaml` solves this. It lets you declare what **capability** an MCP server provides (for example, code search) and which built-in tools it should be **preferred over**. Once declared, the Agent automatically selects the right tool at profile activation time and injects a preference hint into the system prompt so the model knows which tool to reach for first.

## How it works

Each tool can declare one or more **capability tags** — abstract labels like `code.explore` that describe what the tool does. Built-in tools already declare their own capabilities (for instance, both `Grep` and `Glob` declare `code.explore`). When you add the same capability to an MCP server in `integrations.yaml`, the Agent treats those MCP tools as alternatives to the built-ins.

If you also set `preferOver`, the Agent orders providers so the MCP tool appears first in the preference list. The ordering is driven entirely by explicit `preferOver` declarations — there is no implicit MCP-first behavior.

At profile activation, the Agent reads the active profile's tool list, discovers any additional tools that match the profile's required capabilities, and merges them into the active set. If no profile declares `capabilitiesRequired`, the integration metadata is still recorded in the registry but no automatic tool injection happens.

The Agent also generates a short preference hint and injects it into the system prompt:

```
Tool preference hints (derived from installed integrations):
- For `code.explore`, prefer `mcp__codebase-memory-mcp__search_graph` (falls back to: Grep, Glob).
```

This tells the model which tool to reach for first without requiring the user to manually adjust prompts.

## Configuration

### File location

`integrations.yaml` uses the same two-level pattern as [`mcp.json`](./mcp.md#configuration):

- **User level**: `~/.mirri-code/integrations.yaml` (or `$MIRRICODE_HOME/integrations.yaml`), shared across all projects
- **Project level**: `.mirri-code/integrations.yaml` in the repository root, effective only for the current project

Both files are optional. When both exist, entries with the same server name are merged — the project-level entry replaces the user-level entry entirely for that key. Entries unique to either scope are preserved.

### Schema

The root key is `integrations`, a map from MCP server name to its integration spec:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `capabilities` | `string[]` | No | Capability tags this server provides. Defaults to `[]`. |
| `preferOver` | `string[]` | No | Built-in tool names this server should be preferred over. |

Both arrays must contain non-empty strings.

### Example

A codebase intelligence MCP server that should be preferred over `Grep` and `Glob` for code exploration:

```yaml
integrations:
  codebase-memory-mcp:
    capabilities:
      - code.explore
    preferOver:
      - Grep
      - Glob
```

A server that only declares a capability without preferring over any built-in:

```yaml
integrations:
  github-mcp:
    capabilities:
      - code.navigate
```

A server with only a preference claim (useful when the capability is already declared by the built-in tools themselves):

```yaml
integrations:
  fast-search:
    preferOver:
      - Grep
```

## Available capabilities

| Capability | Description | Built-in providers |
|------------|-------------|--------------------|
| `code.explore` | Searching and exploring code (file search, content grep, semantic search) | `Grep`, `Glob` |

More capabilities will be added as the tool ecosystem grows. You can use any non-empty string as a capability tag — the registry accepts custom values, though only the ones listed above have built-in tool counterparts today.

## Next steps

- [Model Context Protocol](./mcp.md) — MCP server configuration and connection methods
- [Agents and Subagents](./agents.md) — Agent profiles, tool lists, and `capabilitiesRequired`
