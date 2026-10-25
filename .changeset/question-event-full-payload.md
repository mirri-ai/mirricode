---
"@mirri-ai/release": patch
---

Fix a structured question sometimes losing its answer options: a question card restored after a reconnect, or shown in a second window, now offers the same choices as the original prompt instead of only a free-text field. When the same question reaches the client twice, the card with the full option list is the one kept.
