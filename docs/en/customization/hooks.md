# Hooks

Hooks are an automatic trigger mechanism: you tell Mirri Code CLI in advance "whenever X happens, run this script." The script runs on your local machine, and you can put any logic inside it. Typical use cases:

- **Security interception**: Before the Agent executes a shell command, check whether it contains dangerous operations (such as `rm -rf`) and block execution if so
- **Desktop notifications**: When a background task completes, pop up a system notification to bring you back to review the results
- **Automatic checks**: Each time the user submits a message, automatically append some background information to the context (such as the current Git branch)

## How Hooks Work

Configuring a hook rule requires specifying three things: **which event to trigger on**, **which targets to match**, and **which script to run**.

When triggered, the CLI packages the event's details (trigger reason, tool name, command content, etc.) into JSON and passes it to your script via **standard input** (stdin). The script reads this information and decides how to respond.

The script's response is determined by two things:

- **Exit code**: `0` means allow, `2` means block, other non-zero values default to allow
- **Standard output** (stdout): can include explanatory text

Even if the script errors or times out, the CLI **will not interrupt your work** as a result — this "allow on failure" design is called fail-open, preventing hook errors from becoming blockers.

::: warning Note
Precisely because of fail-open, Hooks are suitable for alerts and lightweight interception, but **should not be used as the sole security barrier**. For truly high-risk operations, rely on permission approvals and manual confirmation.
:::

## Quick Start: A Minimal Hook

The following hook flashes a notification in the terminal title bar each time a background task completes (macOS requires `terminal-notifier` to be installed):

```toml
# Written in ~/.mirri-code/config.toml
[[hooks]]
event = "Notification"           # Trigger: when a background task status changes
matcher = "task\\.completed"     # Only care about "completed" notifications
command = "terminal-notifier -title Mirri -message 'Task done'"
```

Save the config, start a new session, and a notification will appear the next time a background task completes.

## Configuration

All hook rules are written in the `[[hooks]]` array in `~/.mirri-code/config.toml`, where each entry is one rule:

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `event` | `string` | Yes | Trigger event name; must be one of the entries in the "Event Reference" table below |
| `matcher` | `string` | No | A regular expression to filter event targets; if omitted, matches all |
| `command` | `string` | Yes | The shell command to run when triggered |
| `timeout` | `integer` | No | Timeout in seconds, range 1–600; defaults to 30 seconds |

`[[hooks]]` only allows these four fields; extra fields will cause the config file to fail to load.

**When multiple rules match the same event**, all matching hooks run in parallel; multiple rules with identical `command` values run only once.

The working directory for hook commands is the current session's project directory. On non-Windows platforms, hook processes are placed in a separate process group; on timeout, a signal is sent first to give the process a chance to clean up, then it is forcibly terminated.

### Event Data Format

Each time a hook triggers, the CLI passes the following base information to the script via stdin:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project"
}
```

Specific events also include additional fields (such as tool name, command content, token usage, etc.). All field names use snake_case. The [Event payload reference](#event-payload-reference) section below documents the complete stdin payload for each event.

## Return Values

After the script exits, the CLI determines the hook's intent based on the exit code:

| Exit code | Meaning | CLI behavior |
| --- | --- | --- |
| `0` | Normal exit, allow | Continue execution; stdout content (if any) may be appended to context |
| `2` | Intentional block | Stop the current operation; stderr content (printed via `console.error`) is used as the reason for blocking |
| Other non-zero | Script error | Default allow (fail-open) |
| Timeout or crash | Script exception | Default allow (fail-open) |

You can also return a JSON object via stdout to block:

```json
{
  "hookSpecificOutput": {
    "permissionDecision": "deny",
    "permissionDecisionReason": "Please use rg instead of grep"
  }
}
```

::: info Which events support blocking?
Only **blockable events** (`PreToolUse`, `Stop`, `UserPromptSubmit`) have return values that affect the main flow. All other events are **observation-only events** — they fire and forget; the main flow is unaffected regardless of what the script returns.
:::

## Event Reference

| Event | Matcher matches | Supports blocking? | Description |
| --- | --- | --- | --- |
| `UserPromptSubmit` | The text submitted by the user | ✓ | Triggered when the user sends a message; returned text is appended to context; if blocked, the model is not called for this turn |
| `PreToolUse` | Tool name | ✓ | Triggered before a tool call (before permission checks); the tool will not execute if blocked |
| `Stop` | Empty string | ✓ | Triggered when the model is about to end the current turn; if blocked, a message can be appended to let the model continue |
| `PostToolUse` | Tool name | — | Triggered after a tool executes successfully (observation only) |
| `PostToolUseFailure` | Tool name | — | Triggered after a tool fails or is blocked (observation only) |
| `PermissionRequest` | Tool name | — | Triggered just before waiting for user approval (observation only) |
| `PermissionResult` | Tool name | — | Triggered after approval completes (observation only) |
| `SessionStart` | `startup` or `resume` | — | Triggered after a new session starts or a previous session resumes |
| `SessionEnd` | `exit` | — | Triggered after a session closes |
| `SubagentStart` | Sub-agent name | — | Triggered before a sub-agent starts running |
| `SubagentStop` | Sub-agent name | — | Triggered after a sub-agent completes successfully (observation only) |
| `StopFailure` | Error type | — | Triggered after the current turn fails due to an error (observation only) |
| `Interrupt` | Empty string | — | Triggered when the user interrupts the current turn (e.g. pressing Esc); not fired for timeouts or other programmatic aborts. `Stop` does not fire on interrupts, so this event fires instead. The payload includes a `reason` field (observation only) |
| `PreCompact` | `manual` or `auto` | — | Triggered before context compaction begins; return values are completely ignored |
| `PostCompact` | `manual` or `auto` | — | Triggered after context compaction completes (observation only) |
| `Notification` | Notification type (e.g. `task.completed`) | — | Triggered when a background task status changes (observation only) |
| `RewriteToolInput` | Tool name | — | Triggered before tool execution (before permission checks); can modify tool arguments via `updatedInput` in the JSON response (requires experimental flag `hook-command-rewrite`) |
| `PreLlmRequest` | Empty string | — | Triggered before the LLM API call, after messages and tools are assembled (observation only) |
| `PostLlmRequest` | Empty string | — | Triggered after the LLM response returns, before tool execution begins (observation only) |

## Event payload reference

The base fields (`hook_event_name`, `session_id`, `cwd`) are included in every event. Below are the additional fields each event sends via stdin. Field values shown are illustrative examples.

### `UserPromptSubmit`

```json
{
  "hook_event_name": "UserPromptSubmit",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "prompt": "Help me refactor this function"
}
```

| Field | Description |
|-------|-------------|
| `prompt` | The text the user submitted |

### `PreToolUse`

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git status" },
  "tool_call_id": "call_001"
}
```

| Field | Description |
|-------|-------------|
| `tool_name` | The tool about to be called |
| `tool_input` | The arguments passed to the tool |
| `tool_call_id` | Unique ID for this tool call |

### `Stop`

```json
{
  "hook_event_name": "Stop",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "stop_hook_active": false
}
```

| Field | Description |
|-------|-------------|
| `stop_hook_active` | Whether the Stop hook continuation has already been used in this turn (`true` on the second and subsequent triggers) |

### `PostToolUse`

```json
{
  "hook_event_name": "PostToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git status" },
  "tool_call_id": "call_001",
  "tool_output": "On branch main..."
}
```

| Field | Description |
|-------|-------------|
| `tool_name` | The tool that was called |
| `tool_input` | The arguments passed to the tool |
| `tool_call_id` | Unique ID for this tool call |
| `tool_output` | Tool output text (truncated to 2000 characters) |

### `PostToolUseFailure`

```json
{
  "hook_event_name": "PostToolUseFailure",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git push" },
  "tool_call_id": "call_001",
  "error": { "message": "Permission denied" }
}
```

| Field | Description |
|-------|-------------|
| `tool_name` | The tool that was called |
| `tool_input` | The arguments passed to the tool |
| `tool_call_id` | Unique ID for this tool call |
| `error` | Error payload describing why the tool failed |

### `PermissionRequest`

```json
{
  "hook_event_name": "PermissionRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "tool_input": { "command": "rm temp.txt" },
  "display": "rm temp.txt"
}
```

| Field | Description |
|-------|-------------|
| `turn_id` | The turn number this tool call belongs to |
| `tool_call_id` | Unique ID for this tool call |
| `tool_name` | The tool requesting permission |
| `action` | The permission action being requested |
| `tool_input` | The arguments passed to the tool |
| `display` | Human-readable summary of the tool call |

### `PermissionResult`

```json
{
  "hook_event_name": "PermissionResult",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "decision": "allow",
  "scope": "session"
}
```

When an error occurs during permission resolution, `decision` is `"error"` and an `error` field replaces `scope`:

```json
{
  "hook_event_name": "PermissionResult",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "decision": "error",
  "error": "Approval timed out"
}
```

| Field | Description |
|-------|-------------|
| `turn_id` | The turn number this tool call belongs to |
| `tool_call_id` | Unique ID for this tool call |
| `tool_name` | The tool that requested permission |
| `action` | The permission action that was requested |
| `decision` | The outcome: `allow`, `deny`, or `error` |
| `scope` | The scope of the decision (e.g. `session`, `project`); omitted when `decision` is `error` |
| `error` | Error message; present only when `decision` is `error` |

### `SessionStart`

```json
{
  "hook_event_name": "SessionStart",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "source": "startup"
}
```

| Field | Description |
|-------|-------------|
| `source` | How the session started: `startup` (new session) or `resume` (restored session) |

### `SessionEnd`

```json
{
  "hook_event_name": "SessionEnd",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "reason": "exit"
}
```

| Field | Description |
|-------|-------------|
| `reason` | Why the session ended; currently always `exit` |

### `SubagentStart`

```json
{
  "hook_event_name": "SubagentStart",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "agent_name": "coder",
  "prompt": "Refactor the auth module",
  "model_alias": "sonnet"
}
```

| Field | Description |
|-------|-------------|
| `agent_name` | The sub-agent's profile name |
| `prompt` | The prompt sent to the sub-agent (truncated for preview) |
| `model_alias` | The model alias configured for the sub-agent |

### `SubagentStop`

```json
{
  "hook_event_name": "SubagentStop",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "agent_name": "coder",
  "response": "Refactoring complete. Changed 3 files..."
}
```

| Field | Description |
|-------|-------------|
| `agent_name` | The sub-agent's profile name |
| `response` | The sub-agent's final response (truncated for preview) |

### `StopFailure`

```json
{
  "hook_event_name": "StopFailure",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "error_type": "APIError",
  "error_message": "Rate limit exceeded"
}
```

| Field | Description |
|-------|-------------|
| `error_type` | The error type name |
| `error_message` | The error message |

### `Interrupt`

```json
{
  "hook_event_name": "Interrupt",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "reason": "cancelled"
}
```

| Field | Description |
|-------|-------------|
| `turn_id` | The turn number that was interrupted |
| `reason` | Why the turn was interrupted; currently always `cancelled` |

### `PreCompact`

```json
{
  "hook_event_name": "PreCompact",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "trigger": "auto",
  "token_count": 45000
}
```

| Field | Description |
|-------|-------------|
| `trigger` | What initiated compaction: `manual` or `auto` |
| `token_count` | The token count at the time compaction was triggered |

### `PostCompact`

```json
{
  "hook_event_name": "PostCompact",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "trigger": "auto",
  "estimated_token_count": 12000
}
```

| Field | Description |
|-------|-------------|
| `trigger` | What initiated compaction: `manual` or `auto` |
| `estimated_token_count` | The estimated token count after compaction |

### `Notification`

```json
{
  "hook_event_name": "Notification",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "sink": "context",
  "notification_type": "task.completed",
  "title": "Background task completed",
  "body": "Linting finished with 0 errors",
  "severity": "info",
  "source_kind": "task"
}
```

| Field | Description |
|-------|-------------|
| `sink` | Where the notification is delivered (e.g. `context`) |
| `notification_type` | The notification type (e.g. `task.completed`) |
| `title` | Notification title |
| `body` | Notification body text |
| `severity` | Notification severity level (e.g. `info`, `warning`, `error`) |
| `source_kind` | What produced the notification (e.g. `task`) |

### `RewriteToolInput`

See the [command rewriting example](#example-command-rewriting) for the full payload and response format.

### `PreLlmRequest`

Fires once per loop step, after messages and tools are assembled but before the LLM API call is made. Useful for observing which model is being called, how the request grows over time, and whether dynamic tool loading works as expected.

Media parts (images, audio, video) in `messages` are replaced by a `size_bytes` marker because their base64 payloads can be several MB. Text, thinking blocks, tool calls, and tool declarations are preserved.

```json
{
  "hook_event_name": "PreLlmRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": "3",
  "step": 1,
  "model": "claude-sonnet-4-20250514",
  "message_count": 12,
  "tool_count": 8,
  "messages": [
    {
      "role": "system",
      "content": [
        { "type": "text", "text": "You are a helpful coding assistant..." }
      ],
      "tools": [
        {
          "name": "dynamic_tool",
          "description": "A tool loaded mid-conversation",
          "parameters": { "type": "object", "properties": { "x": { "type": "number" } } }
        }
      ]
    },
    {
      "role": "user",
      "content": [
        { "type": "text", "text": "Look at this screenshot" },
        { "type": "image_url", "size_bytes": 1843200 }
      ]
    },
    {
      "role": "assistant",
      "content": [
        { "type": "think", "think": "The user sent a screenshot, I need to analyze it..." }
      ],
      "tool_calls": [
        { "id": "call_abc", "name": "Read", "arguments": "{\"path\":\"/src/main.ts\"}" }
      ]
    },
    {
      "role": "tool",
      "tool_call_id": "call_abc",
      "content": [
        { "type": "text", "text": "1\timport { createApp } from 'vue'\n2\t..." }
      ]
    }
  ]
}
```

| Field | Description |
|-------|-------------|
| `turn_id` | The turn number this LLM call belongs to |
| `step` | The step number within the turn (starts at 1) |
| `model` | The model name being called |
| `message_count` | Number of messages in the request |
| `tool_count` | Number of tools available in the request |
| `messages` | Serialized conversation messages; media parts replaced by `size_bytes` |

### `PostLlmRequest`

Fires once per loop step, after the LLM response returns and usage is recorded, but before tool execution begins — earlier than `step.end`. Useful for tracking token usage, cost estimation, and response timing without waiting for tool calls to complete.

If the LLM call fails (after all retries), `PreLlmRequest` fires but `PostLlmRequest` does not — you can correlate the two to detect failures.

```json
{
  "hook_event_name": "PostLlmRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": "3",
  "step": 1,
  "model": "claude-sonnet-4-20250514",
  "finish_reason": "tool_use",
  "usage": {
    "input_other": 344,
    "output": 567,
    "input_cache_read": 890,
    "input_cache_creation": 0
  },
  "duration_ms": 3420,
  "ttft_ms": 850,
  "tool_call_count": 2
}
```

| Field | Description |
|-------|-------------|
| `turn_id` | The turn number this LLM call belongs to |
| `step` | The step number within the turn (starts at 1) |
| `model` | The model name that was called |
| `finish_reason` | Why the response ended: `end_turn`, `tool_use`, `max_tokens`, `filtered`, `paused`, or `unknown` |
| `usage` | Token usage breakdown (see below) |
| `duration_ms` | End-to-end LLM call duration in milliseconds (including retries) |
| `ttft_ms` | Time to first token in milliseconds (may be omitted if the provider does not report it) |
| `tool_call_count` | Number of tool calls in the response |

`usage` object fields:

| Field | Description |
|-------|-------------|
| `input_other` | Input tokens that were neither cache-read nor cache-created |
| `output` | Output (completion) tokens generated by the model |
| `input_cache_read` | Input tokens served from the provider's prompt cache |
| `input_cache_creation` | Input tokens written into the provider's prompt cache |

::: tip Token tracking example
```bash
#!/bin/bash
# ~/.mirri-code/hooks/token-tracker.sh
input=$(cat)
model=$(echo "$input" | jq -r '.model')
input_tokens=$(echo "$input" | jq -r '.usage.input_other + .usage.input_cache_read + .usage.input_cache_creation')
output_tokens=$(echo "$input" | jq -r '.usage.output')
echo "$(date -Iseconds),$model,$input_tokens,$output_tokens" >> ~/.mirri-code/token-usage.csv
```
:::

## Example: Blocking Dangerous Shell Commands

The following hook checks the command content before the Agent calls the `Bash` tool and blocks it if `rm -rf` is detected:

```toml
[[hooks]]
event = "PreToolUse"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/block-dangerous-bash.mjs"
timeout = 5
```

```js
// block-dangerous-bash.mjs
// Read event data passed by the CLI from stdin
let input = '';
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  const payload = JSON.parse(input);         // Parse event data
  const command = payload.tool_input?.command ?? '';

  if (command.includes('rm -rf')) {
    // Explain the blocking reason via stderr; exit code 2 means block
    console.error('Dangerous command detected, blocked');
    process.exit(2);
  }
  // Normal exit (exit code 0) means allow
});
```

After blocking, Mirri Code CLI writes the blocking reason back into the context, and the model can use this to choose a safer alternative.

::: warning Note
This example only demonstrates the blocking mechanism — it is not a production-grade security parser. Real scenarios are better served by whitelists, or a dedicated shell parser to handle quoting, variable expansion, and multi-command sequences.
:::

## Example: Command Rewriting

The `RewriteToolInput` event allows transparently rewriting tool arguments before execution.

First, enable the experimental feature:

```bash
# Via environment variable (for quick testing)
export MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE=true

# Or via config file (persistent)
# [experimental]
# hook-command-rewrite = true
```

Then configure a rewrite hook. The `matcher` field is a regex matched against the tool name — you can target `Bash` specifically or use `.*` to match all tools:

```toml
[[hooks]]
event = "RewriteToolInput"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/rewriter.mjs"
timeout = 5
```

### How It Works

The `RewriteToolInput` hook runs in the tool call lifecycle **after** the preparation phase (dedup, etc.) but **before** permission checks. This means:

1. Agent decides to call a tool (e.g. `Bash` with `git status`)
2. Preparation hooks run (dedup, etc.)
3. **`RewriteToolInput` hook fires** — your script can rewrite the arguments
4. Rewritten arguments are validated
5. Permission checks run (including `PreToolUse` hooks)
6. Tool executes with the final arguments

### Multiple Hooks

If multiple `RewriteToolInput` hooks match the same tool, the **first** hook that returns `updatedInput` wins. If a hook returns without `updatedInput`, it is skipped.

### Writing Your Own Rewrite Hook

Any script that reads JSON from stdin and writes a JSON response to stdout can be a rewrite hook.

Here is a production-quality Node.js example — a command wrapper that logs every rewritten command to a file for audit purposes:

```js
#!/usr/bin/env node
// ~/.mirri-code/hooks/command-logger.mjs
//
// Rewrites tool input and logs every rewrite to an audit file.
// Usage: set as a RewriteToolInput hook in config.toml.

import { appendFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

const LOG_DIR = join(process.env.HOME ?? '/tmp', '.mirricode-code', 'logs');
const LOG_FILE = join(LOG_DIR, 'rewrite-audit.log');

function log(message) {
  try {
    mkdirSync(LOG_DIR, { recursive: true });
    appendFileSync(LOG_FILE, `${new Date().toISOString()} ${message}\n`);
  } catch {
    // Logging failure must not break the hook
  }
}

let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  let payload;
  try {
    payload = JSON.parse(input);
  } catch (err) {
    log(`PARSE_ERROR: ${err instanceof Error ? err.message : err}`);
    process.exit(1);
  }

  const toolName = payload.tool_name;
  const toolInput = payload.tool_input;
  const command = toolInput?.command;

  // Only rewrite Bash tool calls
  if (toolName !== 'Bash' || typeof command !== 'string' || command.length === 0) {
    process.exit(0);
  }

  // Example rewrite: prefix with a custom wrapper
  const rewritten = `my-tool ${command}`;

  log(`REWRITE tool=${toolName} original=${JSON.stringify(command)} rewritten=${JSON.stringify(rewritten)}`);

  process.stdout.write(JSON.stringify({
    hookSpecificOutput: {
      permissionDecision: 'allow',
      updatedInput: { command: rewritten },
    },
  }));
});
```

Configure it:

```toml
[[hooks]]
event = "RewriteToolInput"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/command-logger.mjs"
timeout = 5
```

The input your script receives via stdin:

```json
{
  "hook_event_name": "RewriteToolInput",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "ls -la" },
  "tool_call_id": "call_001"
}
```

Your script must write a JSON response to stdout:

```json
{
  "hookSpecificOutput": {
    "permissionDecision": "allow",
    "updatedInput": { "command": "my-tool ls -la" }
  }
}
```

**Rules:**
- Exit `0` with `updatedInput` in the response → tool runs with rewritten arguments
- Exit `0` without `updatedInput` → tool runs with original arguments
- Exit non-zero → tool runs with original arguments (fail-open)
- Crash or timeout → tool runs with original arguments (fail-open)

::: tip
For production use, always validate `tool_name` and `tool_input` before rewriting. Not all tools have a `command` field — only rewrite what you understand.
:::

::: warning Security Note
Allowing hooks to rewrite tool arguments introduces a security risk. A malicious hook could transparently redirect commands. Only enable this feature with trusted hooks from verified sources.
:::

## Use Cases

- **Command wrapping**: Prepend a custom tool to every command (e.g. `my-tool git status`)
- **Token savings**: Integrate [rtk](https://github.com/rtk-ai/rtk) to compress CLI output
- **Audit logging**: Log every command before execution
- **Environment injection**: Add environment variables or flags to specific commands

## Next steps

- [Configuration files](../configuration/config-files.md#hooks) — Full field reference for `[[hooks]]` in `config.toml`
- [Agents and sub-agents](./agents.md) — Use the `SubagentStop` event to trigger notifications after a sub-agent completes
