---
"@mirri-ai/release": patch
---

Fix a forked session re-reading the entire conversation on its first message, which made the first reply after forking take far longer than a normal turn. Forked sessions now continue under the source session's conversation attribution, so the already-read history is billed and processed as already read.
