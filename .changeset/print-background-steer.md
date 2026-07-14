---
"@mirri-ai/mirri-code": patch
---

Add `background.print_background_mode` (`exit`/`drain`/`steer`) for `mirri -p`: in `steer` mode, a completing background task (including `Bash(run_in_background=true)`) behaves like a background subagent — it injects a synthetic user message that steers the main agent into a new turn so it can act on the result. Bounded by `print_wait_ceiling_s` and the new `print_max_turns`.
