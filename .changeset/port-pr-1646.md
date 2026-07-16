---
"@mirri-ai/server": patch
"@mirri-ai/agent-core-v2": patch
"@mirri-ai/protocol": patch
"@mirri-ai/mirri-web": patch
---

Port from kimi-code #1646: feat(web): add session diagnostic export

Adds session diagnostic export to download a session and bounded metadata-only troubleshooting logs as a ZIP. Run `/export` or pick Export session from a session's more menu. Web downloads are limited to 64 MiB.
