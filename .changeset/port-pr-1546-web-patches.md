---
"@mirri-ai/mirri-code": patch
---

web: Port upstream kimi-code patches: fix sidebar lag with many sessions by avoiding repeated session list scans; match the current model by id in the model picker dropdown to avoid false checkmarks on same-named models from other providers; auto-enable the default thinking effort when switching to an effort-capable model; persist the server access token across tab close and browser restarts for up to 7 days; keep the connecting splash up and retry the first-load auth check when the server is temporarily unreachable.
