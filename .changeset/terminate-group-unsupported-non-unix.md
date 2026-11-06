---
"@mirri-ai/release": patch
---

Fail with an explicit unsupported-platform error instead of silently succeeding when terminating a background task's process group on non-Unix platforms, so orphaned processes can no longer be reported as cleaned up while still running. Only affects non-Unix builds, where this path is not currently reachable; this is a defensive fix ahead of future Windows support.
