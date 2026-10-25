---
"@mirri-ai/release": patch
---

Fix the server process crashing when FetchURL fetched a page containing non-ASCII characters (such as emoji or accented letters) before a script block; the fetch now succeeds and preserves the text as-is instead of taking down the server and corrupting non-ASCII content.
