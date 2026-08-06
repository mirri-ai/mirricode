---
"@mirri-ai/mirri-code": patch
---

Show an actionable error message when a skill frontmatter value starts with Markdown emphasis that the YAML parser misreads as an anchor alias, telling the author to quote the value instead of reporting the raw parse error.