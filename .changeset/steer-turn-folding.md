---
"@mirri-ai/release": minor
---

Keep mid-turn steers inside the turn they belong to: a message typed while the agent is working no longer splits the conversation into a new turn. In the web UI it now renders as a full user bubble with a steer badge, in place inside the running turn, with the output before and after it staying in one continuous turn.

Steered messages now survive a restart: they are persisted as real conversation entries, so reopening a session shows them exactly where they were sent instead of losing them.

Undo and compaction now treat a steer as real user input: undo can rewind to a steered message, and compaction keeps it when picking which user messages to preserve.
