#!/usr/bin/env node
// Builds the "packaging-equivalent" Rust backend for the Electron desktop app.
//
// The packaged app embeds the web SPA into the `mirri` binary (rust-embed,
// default `embedded-web` feature) and ships that binary via electron-builder's
// extraResources. This script produces exactly the artifact before-pack.cjs
// stages: a native `mirri` / `mirri.exe` under
// `apps/mirri-desktop/build/backend/<platform>-<arch>/`, matching the dev
// resolution order in src/main/server-binary.ts.
//
// Steps (mirrors what desktop-build.yml runs per platform on CI):
//   1. pnpm -C apps/mirri-web run build        — build the web SPA
//   2. node rust/scripts/stage-web.mjs         — stage it into the rust-embed
//                                                source directory
//   3. cargo build --profile dist --locked -p mirri-cli
//                                                — compile the host-target binary
//                                                (default features incl. embedded-web)
//   4. copy rust/target/dist/mirri[.exe] → build/backend/<platform>-<arch>/
//
// Every step fails loudly: a clear error on stderr and a non-zero exit code.
//
// Cross-shell note: pnpm's Windows launcher varies by installer —
//   * standalone installer  → pnpm.exe   (real exe, no shell needed)
//   * npm / corepack shim   → pnpm.cmd   (batch file, needs cmd.exe)
//   * always                → pnpm       (bash shim; Node cannot exec it)
// Git Bash exposes the bash shim as `pnpm`, so a hard-coded `pnpm.cmd` breaks
// there when pnpm was installed standalone. We probe PATH and pick the first
// usable launcher.
//
// Usage:
//   node apps/mirri-desktop/scripts/build-backend.mjs
//   pnpm -C apps/mirri-desktop run build:backend

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { copyFile } from 'node:fs/promises';
import { delimiter, dirname, join, resolve } from 'node:path';

const desktopDir = resolve(import.meta.dirname, '..');
const repoDir = resolve(desktopDir, '..', '..');
const isWindows = process.platform === 'win32';

function fail(message) {
  console.error(`[build-backend] ${message}`);
  process.exit(1);
}

/** Diagnostics only — the real cross-shell work is done by resolvePnpm(). */
function detectShell() {
  if (process.env.MSYSTEM) return 'gitbash';
  if (process.env.PSModulePath) return 'powershell';
  if (process.platform === 'win32') return 'cmd';
  return 'posix';
}

/**
 * Decide how to invoke pnpm on the current platform.
 *
 * Returns { command, shell }:
 *   - command: launcher filename to spawn
 *   - shell:   whether spawnSync must go through cmd.exe to resolve the file
 */
function resolvePnpm() {
  if (!isWindows) return { command: 'pnpm', shell: false };

  const pathDirs = (process.env.PATH ?? '').split(delimiter).filter(Boolean);
  const has = (name) =>
    pathDirs.some((dir) => {
      try {
        return existsSync(join(dir, name));
      } catch {
        return false;
      }
    });

  // Prefer .exe: no shell needed, no PATHEXT ambiguity.
  if (has('pnpm.exe')) return { command: 'pnpm.exe', shell: false };

  // .cmd: needs cmd.exe. This is what the previous hard-coded path assumed.
  if (has('pnpm.cmd')) return { command: 'pnpm.cmd', shell: true };

  // Only the bash shim is present — it cannot be exec'd by Node on Windows.
  if (has('pnpm')) {
    fail(
      'found only a bash shim for pnpm on PATH.\n' +
        '        Reinstall pnpm so a Windows launcher exists:\n' +
        '          corepack enable pnpm   (or)   npm i -g pnpm\n' +
        '        then re-run this script.',
    );
  }

  fail(
    'could not find pnpm on PATH.\n' +
      '        Install it with `corepack enable pnpm` or `npm i -g pnpm`.',
  );
}

const pnpm = resolvePnpm();

function run(command, args, label, shell = false) {
  const pretty = `${command} ${args.join(' ')}`;
  console.log(`[build-backend] $ ${pretty}${shell ? '   (via shell)' : ''}`);
  const result = spawnSync(command, args, {
    stdio: 'inherit',
    cwd: repoDir,
    shell,
  });
  if (result.error !== undefined) {
    fail(`${label}: failed to run \`${command}\` (${result.error.message})`);
  }
  if (result.status !== 0) {
    fail(`${label}: \`${pretty}\` exited with ${result.status}`);
  }
}

console.log(
  `[build-backend] shell=${detectShell()}  pnpm=${pnpm.command}${pnpm.shell ? ' (via shell)' : ''}`,
);

// 1. Build the web SPA. stage-web.mjs refuses to stage anything that is not a
// real SPA build, so the web build must come first.
run(
  pnpm.command,
  ['-C', 'apps/mirri-web', 'run', 'build'],
  'building the web SPA',
  pnpm.shell,
);

// 2. Stage the SPA into the rust-embed source directory (with --locked the
// cargo build below embeds exactly these assets).
run('node', ['rust/scripts/stage-web.mjs'], 'staging the web SPA');

// 3. Compile the host-target binary with the release profile. No --target, so
// cargo produces the native host build under rust/target/dist/.
run(
  'cargo',
  [
    'build',
    '--profile',
    'dist',
    '--locked',
    '-p',
    'mirri-cli',
    '--manifest-path',
    'rust/Cargo.toml',
  ],
  'building the mirri binary',
);

// 4. Copy the binary into the desktop staging directory consumed by
// scripts/before-pack.cjs and the dev resolution order.
const target = `${process.platform}-${process.arch}`;
const exe = isWindows ? 'mirri.exe' : 'mirri';
const sourceBinary = resolve(repoDir, 'rust', 'target', 'dist', exe);
const stageBinary = resolve(desktopDir, 'build', 'backend', target, exe);
try {
  if (!existsSync(sourceBinary)) {
    throw new Error(
      `built binary not found at ${sourceBinary}. The cargo build above should ` +
        'have produced it — check the output.',
    );
  }
  mkdirSync(dirname(stageBinary), { recursive: true });
  await copyFile(sourceBinary, stageBinary);
} catch (error) {
  fail(`staging the mirri binary: ${error.message}`);
}
console.log(`[build-backend] staged Mirri backend (${target}) -> ${stageBinary}`);
