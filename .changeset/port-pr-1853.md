---
"@mirri-ai/mirri-code": patch
---

Run `mirri web` and the TUI `/web` command in the foreground by default instead of backgrounding a daemon; the command stays attached to the terminal until Ctrl+C. Pass `--background` to keep the previous background behavior. An already-running server is reused as-is across upgrades, with a version mismatch pointed out in the output.

Ported from MoonshotAI/kimi-code#1853.
