# @mirri-ai/release

Version anchor for Mirri product releases. This package is **not installable** and **never published** — it exists so changesets has a single version source that is not tied to any individual shippable artifact (desktop, rust CLI, ACP).

- All changesets target this package. Merging the release PR bumps its `version` and appends to its `CHANGELOG.md`.
- The version fans out one-way: `rust/Cargo.toml` workspace version and `apps/mirri-desktop/package.json` version are synced from here (run `node rust/scripts/sync-version.mjs`).
- Release trigger: a change of this package's version on `main` triggers the GitHub Release pipeline.
