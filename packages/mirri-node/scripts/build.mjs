// scripts/build.mjs — Compile mirri-node and copy the .node cdylib next to index.js.
//
// Usage:
//   node scripts/build.mjs          # release build
//   node scripts/build.mjs --debug  # debug build

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
