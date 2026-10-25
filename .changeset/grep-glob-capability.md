---
"@mirri-ai/release": minor
---

Upgrade the Grep tool to return ready-to-read code blocks in content mode: matching lines are grouped with line-numbered context into blocks with a `path:start-end` header, so one call answers find-and-read without a follow-up Read. Grep also accepts an array of up to 32 paths in `path` (search several known files in one pass), supports `exclude_pattern` to drop noise lines (e.g. tests, generated code) in all output modes, and tightens the content-mode default result cap to 30 matches with a 4 KB byte budget so results stay compact by default.

Upgrade the Glob tool with result filtering and sizing: a new `filter` argument narrows matched paths by substring or regex (replacing shell pipelines like `rg --files | rg 'x'`), a new `limit` argument (default 200, `0` for the full set) replaces the fixed 100-match cap, and every result now starts with a count header telling you how many files matched before and after filtering.

Both search tools now fall back to a cached ripgrep binary at `~/.mirri-code/bin/rg` when `rg` is not on `PATH`, so searches keep working on machines where ripgrep was bootstrapped but the shell environment lacks it.
