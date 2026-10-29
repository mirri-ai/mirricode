# Sessions and context

Mirri Code persists every conversation as a "session" — storing message history and metadata so you can close the app and pick up right where you left off. This page covers how to resume sessions, manage context, and export or fork sessions.

## Session storage

All sessions are saved under `$MIRRICODE_HOME/sessions/` (default: `~/.mirri-code/sessions/`), grouped by working directory:

```text
~/.mirri-code/
├── config.toml
├── session_index.jsonl
└── sessions/
    └── <workDirKey>/
        └── <sessionId>/
            ├── state.json
            └── agents/
                ├── main/
                │   └── wire.jsonl
                └── <subagentId>/
                    └── wire.jsonl
```

- `state.json`: session metadata such as title and creation time.
- `agents/*/wire.jsonl`: the agent event stream, used for session recovery and replay. It also carries a request trace — the tool schemas, request parameters, and MCP tool listings sent to the model — for debugging.

::: warning
Do not manually edit files inside the `sessions/` directory — doing so may prevent sessions from being restored correctly.
:::

## Starting and resuming sessions

Past sessions are stored locally and can be resumed without any special flags:

**In the web UI:** the session sidebar lists the workspace's past sessions — select one to continue it.

**List all sessions:**

```sh
mirri session-list
```

## Switching sessions in the web UI

You can manage sessions without leaving the interface: the web UI's session sidebar lets you browse, switch to, and rename past sessions. In addition, the following slash commands work in the input box when the agent is idle:

- **`/new`** (alias `/clear`): switch to a new session, discarding the current context.
- **`/fork`**: fork the current session (see below).

## Recovering interrupted tasks

If the app exits — whether you closed it mid-task or the engine stopped unexpectedly — tasks that were still running are not lost, and you do not have to figure out what was in flight:

- **On the next launch**, a notification card lists every task that was running when the service exited. Each row shows the workspace, the session title, and when it was interrupted, with **Resume** and **Dismiss** buttons per task, plus **Resume all (N)** and **Dismiss all** for batches. Closing the card only hides it for this window — it does not dismiss anything.
- **The session sidebar** marks interrupted sessions with an **Interrupted** badge, together with a per-workspace count. Hovering an interrupted row gives you the same **Resume** / **Dismiss** actions, so you can recover even after closing the notification card.
- **Resume picks up where the task stopped**: the session opens and a new turn continues from the interruption point, without re-doing work that had already finished. If the interrupted turn was waiting on a background command, you are asked to check its result before re-running anything.
- Ignored interruptions clear their badge immediately; unresolved ones fade out after 72 hours. Archived sessions never show as interrupted.

## Context compression

As a conversation grows, Mirri Code automatically compresses the message history when the context approaches the window limit, freeing up token space. You can also trigger compression manually at any time:

```
/compact
```

You can pass a hint to tell the model what to prioritize when compressing:

```
/compact Keep the discussion about database migrations
```

## Forking a session

To explore a new direction without disrupting the current conversation, use `/fork`:

```
/fork
```

The two resulting sessions are completely independent and do not affect each other. You can switch back to the original at any time from the session sidebar. A saved `/goal` is not copied to the fork. Start a new goal there if you want autonomous goal work.

## Exporting a session

Use `mirri export` to package a session as a ZIP file — useful for sharing, archiving, or filing a bug report:

```sh
mirri export <sessionId>
```

Omitting `sessionId` exports the most recent session in the current directory (with an interactive confirmation prompt; add `-y` to skip). Use `-o` to specify an output path:

```sh
mirri export <sessionId> -o ~/Desktop/my-session.zip
```

The export includes all files in the session directory, including diagnostic logs. The global diagnostic log (`~/.mirri-code/logs/mirri-code.log`) is also bundled by default; add `--no-include-global-log` to exclude it.

::: tip
Exported files may contain code, command output, and file paths that are sensitive. Review the content before sharing.
:::

## Next steps

- [Data locations](../configuration/data-locations.md) — full directory layout for session files
- [mirri command reference](../reference/mirri-command.md) — every command the engine binary accepts
