---
"@mirri-ai/release": patch
---

Stop the Read tool from reporting a line window it fully delivered as truncated, so the agent no longer re-reads content it already has or falls back to shell commands. The shell tools' descriptions and the base prompt now name the idioms they replace (`sed -n 'A,Bp'`, `head`/`tail`, `grep`), pointing them at Read/Grep/Glob/Edit/Write instead.
