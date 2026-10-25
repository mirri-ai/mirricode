---
"@mirri-ai/release": minor
---

Keep the sidebar session list stable while background sessions stream: rows no longer jump to the top as conversations produce output — position now only moves when you send a message or open a session, and run/wait/finished status shows as an inline badge instead. Also cuts renderer CPU during parallel sessions: state updates are coalesced per frame and each session only refreshes its own view, and code blocks highlight once when a reply finishes instead of re-highlighting while streaming.
