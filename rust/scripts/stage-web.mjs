#!/usr/bin/env node
// Stages the built web SPA (apps/mirri-web/dist) into the rust-embed source
// directory (rust/crates/mirri-server/web-dist) so a full `cargo build`
// (`mirri-cli` with its default `embedded-web` feature) embeds the SPA into
// the binary. The lite build (`--no-default-features`) never reads this
// directory.
//
// Usage:
//   node rust/scripts/stage-web.mjs
//
// Prerequisite: `pnpm -C apps/mirri-web run build` (the script refuses to
// stage anything that is not a real SPA build).

import { cp, mkdir, readdir, rm, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const rustDir = resolve(import.meta.dirname, '..');
const repoDir = resolve(rustDir, '..');
const sourceDir = resolve(repoDir, 'apps/mirri-web/dist');
const targetDir = resolve(rustDir, 'crates/mirri-server/web-dist');

function fail(message) {
  console.error(message);
  process.exit(1);
}

try {
  const entries = await readdir(sourceDir);
  if (!entries.includes('index.html')) {
    fail(`no SPA build found at ${sourceDir} (missing index.html)`);
  }
} catch {
  fail(`no SPA build found at ${sourceDir} — run \`pnpm -C apps/mirri-web run build\` first`);
}

// Wipe the staging directory before copying: a stale file set would embed
// outdated assets (or leftovers from a different build) into the binary.
await rm(targetDir, { recursive: true, force: true });
await mkdir(targetDir, { recursive: true });
await cp(sourceDir, targetDir, { recursive: true });
// Keep the committed directory placeholder alive (git-tracked, and the
// bare-checkout guarantee that `web-dist/` always exists for rust-embed).
await writeFile(resolve(targetDir, '.gitkeep'), '');

console.log(`Staged web SPA: ${sourceDir} -> ${targetDir}`);
