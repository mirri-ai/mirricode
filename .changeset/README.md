# Changesets

This repository uses [changesets](https://github.com/changesets/changesets) to manage product versioning and releases.

## Package Publishing Strategy

This repository uses a **single version anchor** strategy: every changeset targets `@mirri-ai/release` (`packages/release`), a private package that is never published to npm. It holds the product-level version that becomes the GitHub Release tag `vX.Y.Z`; `rust/scripts/sync-version.mjs` fans the version out to the rust workspace and the desktop app.

npm publishing is retired. `@mirri-ai/mirri-code`, `@mirri-ai/mirri-node`, and `@mirri-ai/mirri-code-sdk` are frozen at their last published versions (`mirri-code` and `mirri-node` are excluded via `ignore` in `.changeset/config.json` and no longer appear in the `pnpm changeset` prompt).

Current publishable packages:

| Package | Directory | Description |
| --- | --- | --- |
| `@mirri-ai/release` | `packages/release` | Version anchor for the whole product — version + changelog only, private, never published |

All other workspace packages are private internal packages and are not targeted by release changesets:

- `@mirri-ai/acp-adapter`
- `@mirri-ai/agent-core`
- `@mirri-ai/kaos`
- `@mirri-ai/kimi-telemetry`
- `@mirri-ai/mirri-web`
- `@mirri-ai/kosong`
- `@mirri-ai/protocol`
- `@mirri-ai/server-e2e`
- `@mirri-ai/vis`
- `@mirri-ai/vis-server`
- `@mirri-ai/vis-web`
- `@mirri-ai/mirri-code` (npm publishing retired)
- `@mirri-ai/mirri-node` (npm publishing retired)
- `@mirri-ai/mirri-code-sdk` (npm publishing retired — package is private, not published)

Version impact from product components must be judged at the product level. Any user-perceivable change — rust core, desktop, web, or CLI — gets a changeset against the anchor: `minor` for features, `patch` for fixes. Do not bump component versions by hand; the sync script derives them from the anchor.

Example scenarios:

| Change | Changeset selection |
| --- | --- |
| New user-facing feature (rust core / desktop / web / CLI) | Add `minor` to `@mirri-ai/release` |
| Bug fix that users can observe | Add `patch` to `@mirri-ai/release` |
| Test-only, internal refactor, docs, or private debug tooling changes | Usually no changeset needed |
| Bundled official plugin change under `plugins/` (e.g. `mirri-datasource`) | No changeset — the plugin is versioned via its own `mirri.plugin.json` / `plugins/marketplace.json` and shipped through the marketplace CDN, not the anchor |

## Development Workflow

### 1. Implement the feature or fix

Complete code, tests, and documentation changes as usual. A changeset is required when the change is user-perceivable (rust core, desktop, web, CLI behavior or user-facing API).

### 2. Generate a changeset

From the repository root:

```sh
pnpm changeset
```

Follow the prompts to choose:

- `@mirri-ai/release` as the bumped package;
- The version bump level:
  - `patch`: bug fixes, small changes, follow-up dependency updates;
  - `minor`: backward-compatible new features;
- A user-facing description of the change.

The command creates a `.changeset/*.md` file that must be committed alongside the code.

### 3. Commit the changeset

```sh
git add .changeset/
git commit -m "chore: add changeset for release"
git push
```

Commit messages must follow Conventional Commit style. Do not include any author/agent identity in the commit message.

### 4. CI generates the release PR

Once the changeset file is merged into `main`, `.github/workflows/release.yml` uses `changesets/action@v1` to create or update the release PR titled `ci: release packages`.

The release PR runs:

- `pnpm changeset version` (via `pnpm run version:release`): bumps the anchor version and updates its changelog;
- Deletes the consumed `.changeset/*.md` files;
- Uses the commit/title `ci: release packages`.

### 5. Merge the release PR

Once the release PR is merged into `main`, the same workflow detects the anchor version change and publishes a GitHub Release tagged `vX.Y.Z` with the desktop installers, the rust CLI and `mirri-acp` binaries for all supported platforms, and `manifest.json`. There is no npm publish step anymore.

## Notes

- Every PR with a user-perceivable change should include a changeset against `@mirri-ai/release`.
- Changes under `plugins/` (the bundled official plugins such as `mirri-datasource`) do **not** need a changeset: each plugin carries its own version in `mirri.plugin.json` and `plugins/marketplace.json` and is distributed via the marketplace CDN, separately from the anchor.
- Changeset files must be committed to the repository — release PRs are only triggered after they're merged.
- Release PRs require human review and merge; they will not publish automatically.
- Do not add changesets for private internal packages; only select `@mirri-ai/release`.
- Do not bump `apps/mirri-code`, `apps/mirri-desktop`, or rust workspace versions by hand — run `node rust/scripts/sync-version.mjs` (with `--check` in CI) to sync them from the anchor.

## References

- [Changesets documentation](https://github.com/changesets/changesets)
- [Changesets GitHub Action](https://github.com/changesets/action)
- [GitHub Releases documentation](https://docs.github.com/repositories/releasing-projects-on-github/managing-releases-in-a-repository)