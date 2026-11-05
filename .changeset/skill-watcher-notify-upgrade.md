---
"@mirri-ai/release": patch
---

Upgrade the `notify` file-watching dependency from 6.1.1 to 8.2.1. On macOS under CPU load, notify 6's FSEvents teardown degraded skill-directory watch registration from milliseconds to seconds each, which stalled the engine's hot-reload watcher and starved test runs; notify 8's rewritten backend keeps teardown flat under the same load. Skill hot-reload responsiveness under load is restored.
