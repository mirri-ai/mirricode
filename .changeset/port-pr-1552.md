---
"@mirri-ai/mirri-web": patch
"@mirri-ai/agent-core": patch
---

Port from kimi-code #1552: fix(web): keep ReadMediaFile media rendering after session resume

After a session resume or reload, the REST snapshot now passes media content
(image/video/audio) through as raw content parts instead of flattening them to
text. The web layer's media URL parser was updated to handle both the mapped
protocol types (from REST snapshots) and the original kosong types (from the
live event stream).
