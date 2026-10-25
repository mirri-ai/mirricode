---
"@mirri-ai/release": patch
---

web: Stop flagging pasted log and code text as invalid skill references before sending — the pre-send warning now only checks skill chips actually inserted from the menu, and "remove invalid references" can no longer delete pasted text. Drafts saved by older builds with compact `[$name]` chips now resolve back to full skill links on load.
