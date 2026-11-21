---
"@mirri-ai/release": patch
---

Fix the TaskOutput hint claiming "Only the tail is shown above" when a paged read (offset_bytes / max_bytes) served a middle window of the log — the hint now describes the requested window and no longer prints a truncation banner for windows the caller asked for.
