---
"@mirri-ai/release": patch
---

Fix a wedged provider SSE stream hanging agent turns indefinitely: streams that only send keep-alive comments (or nothing at all) now fail after a 300-second idle timeout instead of hanging until the user aborts, and SSE read/decode errors are now retried instead of failing on the first attempt.
