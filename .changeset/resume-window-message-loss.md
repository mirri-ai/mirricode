---
"@mirri-ai/release": patch
---

Fix a resumed session discarding messages sent while it was still loading. Reopening a session briefly registered it before its history had finished loading, so a message arriving in that window (a scheduled task firing, a second client sending) ran a turn whose prompt was never written to the transcript — the reply appeared with no question above it, and a scheduled reminder in that window was dropped for good. Such sends now report a retryable conflict instead, and scheduled fires wait for the load to finish.
