---
"@mirri-ai/release": minor
---

Add automatic archive compression: enable the `[retention]` section in `config.toml` (`enabled`, `archive_grace_days`, `interval_hours`) and archived sessions are gzip-compressed in the background once they pass the grace window. Compressed sessions stay fully readable, searchable and exportable; restoring one decompresses it transparently; forking requires a restore first. Off by default — nothing runs and nothing changes on disk until enabled.

Add an archived-sessions management page in web settings: browse archived and compressed sessions with disk usage, savings and age columns, restore or delete single sessions, or run tiered batch cleanup for sessions compressed more than 30/90/180/365 days. Deleting a session now also reclaims image attachments no other session references. The terminal UI gains a compressed badge and a `/retention` command with per-session restore/delete.
