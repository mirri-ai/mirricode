---
"@mirri-ai/release": minor
---

Provider API keys now live in a dedicated credential store instead of config.toml, so your config file can be safely shared or committed.

Existing keys in config.toml migrate automatically on startup: nothing to do, and the old fields are cleaned up behind you. Each provider holds exactly one credential; setting a new one overwrites the old (takes effect on the very next model call, no restart).

New `mirri credential` subcommands: `list`, `set` (hidden input), `remove`, `show` (the only plaintext reveal outlet, with a TTY warning), and `migrate --dry-run`.

Providers can also use an external credential source such as the Azure CLI: the store keeps only a reference and the key itself never lands on disk — when the source is missing, the error says so and points at `az login` instead of pretending to be a wrong API key.

New RPC surface for settings pages: `credential.list/set/remove/status` (no plaintext over the wire) plus a `credential.updated` notification. Deleting a provider removes its stored credential.
