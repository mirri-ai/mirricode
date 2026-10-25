---
"@mirri-ai/release": minor
---

Rename the built-in `Grep` tool's output modes for clarity and drop a deprecated `Glob` option. Use `files` instead of `files_with_matches` and `count` instead of `count_matches` when selecting how `Grep` reports results (the `Glob` tool's unused `include_dirs` option is removed). Calls still passing the old mode names are rejected; the tool now lists its valid arguments when one is unrecognized.
