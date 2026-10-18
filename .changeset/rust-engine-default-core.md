---
"@mirri-ai/release": minor
---

Make the native Rust engine the core behind the CLI, desktop app, web UI and ACP clients, replacing the TypeScript core, which no longer receives new work. Every client now receives the same live event stream, including tool progress, thinking deltas, compaction lifecycle and background task events. Run `mirri migrate` to carry existing sessions over, and `mirri web` to serve the built-in web UI.
