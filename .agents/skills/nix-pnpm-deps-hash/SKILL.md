---
name: nix-pnpm-deps-hash
description: Use when a change touches pnpm-lock.yaml or flake.nix's workspace lists and the Nix Build workflow (a weekly canary and manual dispatch, not a PR gate) reports a `fetchPnpmDeps` hash mismatch. Covers refreshing the fixed-output hash locally when Nix is installed, letting the canary report it when it is not, and the Nix-free static checks.
---

# Refresh the `pnpmDeps` Hash

`flake.nix` pins one fixed-output hash:

```nix
pnpmDeps = pkgs.fetchPnpmDeps {
  pname = "mirri-web-dist";
  inherit version;
  src = jsSrc;
  pnpmWorkspaces = [ "." ] ++ workspaceNames;
  inherit pnpm;
  fetcherVersion = 3;
  hash = "sha256-…";   # ← the value this skill refreshes (flake.nix:145)
};
```

It is a hash over the **fetched dependency tree**, so it is invalidated by any
of its inputs changing: `pnpm-lock.yaml`, `jsSrc` (the root dotfiles and
`workspacePaths`), or `workspaceNames` / `pnpm` / `fetcherVersion`. The Nix
Build job then fails with:

```
error: hash mismatch in fixed-output derivation '…-mirri-web-dist-pnpm-deps.drv':
         specified: sha256-<stale>
             got:   sha256-<correct>
```

**Nix is optional tooling in this repository, not a prerequisite.** Pick the
path that matches your machine.

## Path A — Nix is already installed (~a minute)

Build ONLY the fixed-output derivation. Never `nix build .#mirri-code` for
this: that compiles the rust toolchain and the whole package to answer a
question about one small derivation.

```sh
# 1. Placeholder
#    hash = pkgs.lib.fakeHash;

# 2. Build just the dependency fetch and read the reported hash
nix build .#webDist.pnpmDeps 2>&1 | grep 'got:'

# 3. Paste that SRI hash back, then confirm it settles
nix build .#webDist.pnpmDeps          # must exit 0
nix hash path "$(readlink result)"    # must equal the pinned value
rm -f result                          # not gitignored
```

No host toolchain is involved: the derivation brings its own `pnpm` and node
from the Nix store and runs `pnpm install --frozen-lockfile` itself, so you do
not need a local `pnpm install` or a particular node version. It does need
network access to the npm registry.

If the build appears to hang, check npm registry reachability and the Nix
proxy/mirror configuration — that step is the only networked part.

## Path B — Nix is not installed

Do **not** install Nix just to compute this value.

The `Nix Build` workflow is **not a PR gate** (it runs on `workflow_dispatch`
and a weekly cron), so nothing posts the hash to your PR any more. You have
two options:

1. **Keep Nix only for this one derivation.** Copy the placeholder into
   `flake.nix`, then run Path A on a machine that has Nix — it builds nothing
   but `webDist.pnpmDeps`, so it is about a minute.
2. **Let the canary tell you.** Merge the lockfile change without touching the
   hash; the next weekly `Nix Build` run fails with
   `specified: <stale>` / `got: <correct>` in its log, and
   `gh run view --log-failed` on that run hands you the value. This is slower
   but needs nothing installed and never blocks the PR.

Either way, the PR itself is unaffected: `ci.yml` does not depend on the flake.

## Verify without Nix

Regardless of which path you take, these run anywhere:

```sh
# Every path the flake's fileset references still exists
for p in .npmrc .nvmrc package.json pnpm-lock.yaml pnpm-workspace.yaml \
         tsconfig.json LICENSE packages/* apps/mirri-web apps/mirri-desktop docs; do
  test -e "$p" || echo "MISSING: $p"
done

# Workspace lists agree with pnpm-workspace.yaml
node scripts/check-nix-workspace.mjs
```

`check-nix-workspace.mjs` only walks the transitive closure of its anchor
package, so a green result is not proof that `flake.nix` is fully in sync —
keep `workspacePaths` / `workspaceNames` updated by hand on every add/remove.

## Common trap

`nix/lib.fileset` rejects a nonexistent path outright, so deleting a directory
without removing it from `jsSrc` fails the build during **evaluation**, before
any hash is considered:

```
error: lib.fileset.unions: Element 0 (/nix/store/…-source/build) is a path
that does not exist. To create a file set from a path that may not exist, use
`lib.fileset.maybeMissing`.
```

Fix the fileset first — the hash mismatch can be hiding behind it.
