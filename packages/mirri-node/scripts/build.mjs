// scripts/build.mjs — Compile mirri-node and copy the .node cdylib next to index.js.
//
// Deliberately NOT named `build`: `pnpm -r run build` runs every workspace
// package's `build` script, and that would put a full `cargo build --release`
// of this cdylib into every CI and release run. The binding is only needed by
// `packages/node-sdk`'s rust-engine tests (which are frozen and run on
// demand), so it is built explicitly:
//
//   pnpm --filter @mirri-ai/mirri-node run build:cdylib        # release
//   pnpm --filter @mirri-ai/mirri-node run build:cdylib:debug  # debug
//
// `index.js` also falls back to loading the cdylib straight out of
// `rust/target/{release,debug}/`, so a plain `cargo build -p mirri-node` is
// enough for local work — this script only adds the copy into the package.

import { execSync } from 'node:child_process';
import { copyFileSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';

const here = import.meta.dirname;
const root = resolve(here, '..', '..', '..');
const rustDir = join(root, 'rust');
const pkgDir = resolve(here, '..');

const debug = process.argv.includes('--debug');
const profile = debug ? 'debug' : 'release';

// 1. Compile the Rust crate.
console.log(`Building mirri-node (${profile})…`);
execSync(`cargo build -p mirri-node ${debug ? '' : '--release'}`, {
  cwd: rustDir,
  stdio: 'inherit',
});

// 2. Locate the compiled cdylib and copy it as a .node file.
const ext = process.platform === 'win32' ? '.dll'
  : process.platform === 'darwin' ? '.dylib'
  : '.so';

const srcName = `libmirri_node${ext}`;
const src = join(rustDir, 'target', profile, srcName);

if (!existsSync(src)) {
  console.error(`Build output not found: ${src}`);
  process.exit(1);
}

const platformTag = `${process.platform}-${process.arch}`;
const dest = join(pkgDir, `mirri-node.${platformTag}.node`);

copyFileSync(src, dest);
console.log(`Copied ${src} → ${dest}`);

// Also copy a bare-name copy for simple require() paths.
const bareDest = join(pkgDir, 'mirri-node.node');
copyFileSync(src, bareDest);
console.log(`Copied ${src} → ${bareDest}`);
