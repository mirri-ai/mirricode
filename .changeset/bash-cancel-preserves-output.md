---
"@mirri-ai/release": patch
---

Preserve the partial output of a cancelled foreground command. Cancelling a running command — or stopping it externally — no longer discards what it already printed: the accumulated output stays attached to the task and remains readable via TaskOutput.
