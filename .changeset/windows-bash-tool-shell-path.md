---
"@mirri-ai/release": patch
---

Fix the Bash tool failing to run commands on Windows: the resolved Git Bash path is now kept alive through command execution instead of being dropped before the shell starts.
