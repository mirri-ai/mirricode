---
"@mirri-ai/release": patch
---

Fix per-turn token usage, duration and completion time missing for scheduled-task and reminder turns after reopening a session — the session total was correct, but each turn's usage pill stayed empty because the turn could not be matched back to its rows.
