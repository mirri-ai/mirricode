---
"@mirri-ai/agent-profile": minor
"@mirri-ai/agent-core-v2": minor
"@mirri-ai/mirri-code": minor
---

Unify agent-file discovery and agent-root resolution across engines by converging the v2 workspace loader onto the shared agent-profile package (single source of truth; v2 host-fs error semantics preserved through the fs adapter).