---
"@mirri-ai/release": minor
---

Make the session todo list durable session state instead of a value that only lives while the session is running. Todo writes are now recorded in the session log, so resuming a session restores the list (the terminal panel no longer empties after resume), undo checkpoints include it, and compaction summaries keep it. The stale-todo reminder now also fires for the main agent, and only when the TodoList tool is available to the agent. The web panel stays populated after compaction because the list is served from the session snapshot.