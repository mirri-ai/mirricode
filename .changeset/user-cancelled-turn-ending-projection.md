---
"@mirri-ai/release": patch
---

Fix turn-ending projection for user-initiated aborts: hard-aborted turns now report `cancelled` instead of `failed`, so stopping a turn no longer surfaces a "model request failed" error toast in the web UI.
