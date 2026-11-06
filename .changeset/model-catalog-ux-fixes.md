---
'@mirri-ai/release': patch
---

Fix the model catalog experience: the model picker now opens instantly on the cached model list (the background provider refresh no longer drives its loading state, and a failed refresh surfaces a visible warning instead of being swallowed), and a successful catalog refresh is now actually served — the refreshed directory replaces the in-memory catalog immediately (scoped refreshes keep the whole fetched directory instead of truncating it to the one addressed provider), so newly published providers and models appear in the catalog browse without a restart.
