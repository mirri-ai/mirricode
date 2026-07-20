---
"@mirri-ai/mirri-code": patch
---

Port from kimi-code #1867: fix: unify YOLO and Auto permission mode descriptions across surfaces

Correct the YOLO and Auto permission mode descriptions to match their actual behavior: YOLO auto-approves regular tool actions but the agent may still ask questions, while Auto is fully autonomous and never asks. Unifies the wording across the CLI --help, TUI slash commands and permission selector, session replay notice, web slash command list and mobile permission sheet (en/zh), ACP adapter mode descriptions, and the configuration/interaction docs.
