---
"@mirri-ai/agent-core": minor
---

Scoped image limits and request-size management: the `[image]` config limits in config.toml now also apply to pasted images (CLI paste and ACP prompts), each core uses its own settings, model-read images honor a configurable byte budget and edge cap, oversized WebP is compressed, HEIC/HEIF reads are refused with a conversion command, and HTTP 413 request-too-large rejections recover automatically with media degradation.
