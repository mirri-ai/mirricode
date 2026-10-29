---
"@mirri-ai/release": patch
---

Fix the ACP model picker listing only the active model: it now lists every model alias configured in `config.toml` (using `displayName` when set), so IDE integrations like Zed and JetBrains can switch between all configured models.
