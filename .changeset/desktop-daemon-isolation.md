---
"@mirri-ai/mirri-code": patch
"@mirri-ai/server": patch
"@mirri-ai/mirri-desktop": patch
---

Add a hidden `--lock-name` flag to `mirri server run` so the Desktop app can run its own isolated daemon instance with a separate lock file, log file, and port range (starting at 58827). The server's session index reindex now uses a cross-process lock to avoid concurrent rebuilds when two daemons start simultaneously.
