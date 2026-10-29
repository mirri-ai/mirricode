---
"@mirri-ai/release": minor
---

Rename the Grep `exclude_pattern` argument to `exclude_line_pattern` (breaking rename with no alias; it had zero usage so far). Its description now states it matches line text — like `| grep -v` — and points to `glob` with a `'!'` prefix for excluding files or directories, which the `glob` description now discloses as well. The result now reports how many matches the exclude pattern dropped (`excluded_matches`), and when the pattern drops nothing the result carries a note hinting at the path-dimension alternative.
