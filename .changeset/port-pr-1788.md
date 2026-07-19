---
"@mirri-ai/mirri-code": patch
---

Port from kimi-code #1788: fix: use timestamped default filename for session debug exports. The default export filename for session debug ZIP exports is now `mirri-debug-<shortId>-<timestamp>.zip`, so repeated exports on the same session no longer overwrite each other.
