---
"@mirri-ai/release": major
---

Retire the TypeScript engine chain and the TS CLI/TUI shell. The `mirri` rust binary is now the sole engine and CLI entry point (`mirri web` / `serve` / `acp` / `print` / `migrate` / `session-list` / `config-show` / `doctor`); the web UI and desktop apps are unchanged and keep running against the rust core. Removed with the shell: the interactive terminal UI, `mirri vis` (session visualizer), `mirri web rotate-token`, multi-instance port auto-allocation, and the self-upgrade command (re-running the install script is the upgrade path). Old-engine sessions migrate with `mirri migrate`. The npm packages `@mirri-ai/mirri-code` / `@mirri-ai/mirri-node` remain frozen at their last published versions.
