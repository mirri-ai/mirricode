---
"@mirri-ai/release": minor
---

IDE sessions now run inside the desktop engine when the desktop app is running. `mirri acp` probes for a live desktop server at startup and connects to it as a remote client, so sessions driven from the IDE appear live in the desktop UI (message deltas, tool progress, turn lifecycle) and are stored by the desktop server. When the desktop is not running — or the connection dies mid-flight — the host falls back to standalone operation with the previous behavior. Set `MIRRI_ACP_ENGINE=embedded` to force standalone mode without probing. The session directory must be a workspace already opened in the desktop app; unregistered directories are rejected with a hint to open them there first.
