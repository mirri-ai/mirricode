---
"@mirri-ai/release": patch
---

Log every rejected server credential (the server used to answer 401 silently) and report the running rejection count on the auth summary. The Desktop-spawned server's console log level is now configurable via `MIRRICODE_DESKTOP_LOG_LEVEL` (default `error`); the file log under `~/.mirri-code/logs/` always keeps `info`.
