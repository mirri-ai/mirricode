---
"@mirri-ai/release": patch
---

Fix oversized file reads misleading the agent: results now stay within the 50,000-byte context budget at the source (wide lines no longer get the whole output replaced by a 2000-character preview), multibyte text gets the full budget instead of roughly a third, and truncation notices now state which line range was actually delivered and point at paging with `line_offset` instead of chaining into loops.
