---
"@mirri-ai/release": minor
---

web: Rework the session pin control into a single colored dot at the left edge of each row. Pinning applies a red dot by default, hovering a pinned dot opens the color palette, and clicking it unpins. Pinned sessions also stay visible inside their workspace group instead of only in the "Pinned" section at the top. Pin and unpin now show a toast with an Undo action, and opening a session no longer silently drops its pin.
