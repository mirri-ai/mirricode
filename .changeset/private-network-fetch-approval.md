---
"@mirri-ai/release": minor
---

Fetching private-network addresses (localhost, LAN hosts, Tailscale/CGNAT ranges) now asks for approval instead of failing outright. Approve once per host for the session via the approval prompt; cloud metadata endpoints (e.g. `169.254.169.254`) remain permanently blocked. Headless runs still refuse private-network fetches.
