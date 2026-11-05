---
"@mirri-ai/release": patch
---

Fix quitting the desktop app leaving its background engine process alive, which made the next launch fail with "Address already in use" on port 58827. Quitting now waits for the engine to stop (with a force-kill backstop) and cleans up the desktop state file. In-flight agent turns are interrupted at shutdown and become resumable on the next launch instead of being killed mid-run.
