---
"@mirri-ai/release": patch
---

Fix structured questions silently failing after a client reconnect: when the connection dropped mid-turn, the agent's next question used to fail instantly and fall back to plain text. Questions now stay pending while waiting for the client to reconnect (up to 5 minutes by default, configurable via `[interaction] question_channel_wait_secs` in `config.toml`), and only fail with a clear error if the connection does not come back in time.
