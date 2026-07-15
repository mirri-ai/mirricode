---
"@mirri-ai/mirri-code": patch
---

Fix the WebP decoder failing to bundle into the standalone binary, which broke `pnpm build:native:sea` and the released single-file CLI.
