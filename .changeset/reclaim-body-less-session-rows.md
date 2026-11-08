---
"@mirri-ai/release": patch
---

core: Reclaim the session-index row when opening a listed session whose persisted body is gone. Deleting the sessions directory (or one session's folder) outside the engine used to leave rows that every listing re-offered and every click failed on; the open path (`session.resume` / `session.restore`) now drops those rows, while never-conversed rows (the legal "row present, body absent" state), compressed bodies, and live sessions are kept untouched.
