---
"@mirri-ai/release": patch
---

Fix `mirri web` falling back to a useless workspace when started from the filesystem root on Windows packaged app launches — it now resolves the user's home directory instead of the drive root.
