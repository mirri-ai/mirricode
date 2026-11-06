---
"@mirri-ai/release": patch
---

Mount skill directory watchers explicitly at each host's startup instead of on an engine's first request. Skill hot-reload used to attach its file watcher lazily, so every engine construction in a process registered a macOS file-system watch — with many short-lived engines that starved the system's file-event service and could hang both the app and test runs. Hot-reload still works out of the box in the desktop app, the web server, and ACP hosts; workspaces opened after startup are picked up by a throttled backstop sweep, and the manual Rescan button still reports whether automatic detection is active.
