---
"@mirri-ai/release": patch
---

Fix workspace file grep returning no matches while scanning nothing. `rg` was invoked without a search path, and with a non-terminal stdin (the normal case for a server, daemon or CI run) ripgrep then searches stdin instead of the workspace — so greps silently came back empty. The search path is now explicit, stdin is closed, and the scanned-file count falls back to counting the matching candidates when ripgrep's own report omits it (ripgrep 14 and earlier only count files that produced output).
