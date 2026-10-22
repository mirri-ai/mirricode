---
outline: 2
---

# Changelog

This page documents the important changes in each Mirri Code 2.x release. Releases before 2.0 are no longer listed.

## 2.1.0 (2026-10-22)

### Features

- Agent profiles now default to denying subagent dispatch: a profile that does not list `subagents` can no longer spawn any subagent. The built-in main agent ships with the built-in subagents allowed, so the out-of-the-box setup is unchanged; custom profiles that relied on the old "omitted means all" behavior must list the subagents they want to allow.

### Bug Fixes

- Fix subagents spawned mid-session starting without the session's connected MCP tools, which left a subagent profile's MCP tool authorization unusable until the app restarted.

## 2.0.1 (2026-10-21)

### Bug Fixes

- Restore native release packaging for the CLI and ACP binaries on every platform, so native builds ship again.
- Fix the Linux desktop artifacts failing to build, so the AppImage and `.deb` installers ship again.

## 2.0.0 (2026-10-21)

### Features

- **Native Rust engine:** the engine behind the CLI, desktop app, web UI and ACP clients is now the Rust core; the TypeScript core no longer receives new work. Every client receives the same live event stream, including tool progress, thinking deltas, compaction lifecycle and background task events. Run `mirri migrate` to carry existing sessions over, and `mirri web` to serve the built-in web UI.
- **Breaking:** managed account login is removed. The `mirri login` and `mirri auth` subcommands and the `/login` and `/logout` slash commands are gone, the web UI opens provider setup instead of a sign-in gate, and ACP clients no longer see account authentication. Providers previously set up with OAuth need an explicit API key.
- Rework sessions: fork at any completed turn, resume from the session log without losing history, and run compaction with real lifecycle events. Sessions become workspace-aware — register workspaces, list and search sessions across them, choose the workspace a new session belongs to, and gate project-scoped MCP servers and instructions behind a trust flag.
- Add Markdown agent profiles with same-name overrides, enable/disable controls and full editing from the CLI and web settings, alongside built-in skills and per-skill enablement. MCP configuration becomes layered per user, workspace and plugin, with live pool reconciliation, OAuth support and automatic reconnect.
- Broaden provider and model support with Google (Gemini) and Kimi, Anthropic prompt caching and one-time attachment uploads, and model resolution that never silently swaps your selection. Usage reporting now includes cache read and write tokens, and provider calls are bounded by timeouts with quota errors surfaced instead of retried.
- web: Expand settings to cover providers and models with catalog import, MCP servers, skills, agent profiles, permission rules, cron jobs and background tasks. The chat gains inline skill chips, attachment tiles, a terminal panel, per-turn usage stats, workspace switching and clearer approval and question cards.

### Bug Fixes

- Fix a batch of stability problems: healthy turns flagged as stalled, background subagents left running after a stall, memory spikes when reopening long sessions, web sessions stuck after a stop, dropped or duplicated terminal and event frames, and file uploads truncated on binary payloads.
- Fix a daemon crash when archiving a session whose log contains a step that began but never ended, and stop wedged provider streams from hanging turns indefinitely.
