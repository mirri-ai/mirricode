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
// Usage:
//   node apps/mirri-desktop/scripts/build-backend.mjs
//   pnpm -C apps/mirri-desktop run build:backend

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { copyFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';

const desktopDir = resolve(import.meta.dirname, '..');
const repoDir = resolve(desktopDir, '..', '..');
const isWindows = process.platform === 'win32';

// pnpm installed via corepack / npm-global is a `pnpm.cmd` shim on Windows;
// `node` and `cargo` are real executables and keep their plain names.
const pnpmCommand = isWindows ? 'pnpm.cmd' : 'pnpm';

function fail(message) {
  console.error(`[build-backend] ${message}`);
  process.exit(1);
}

function run(command, args, label) {
  const result = spawnSync(command, args, {
    stdio: 'inherit',
    cwd: repoDir,
    // Only cmd.exe can execute a `.cmd` shim on Windows; route it through the
    // shell. The args are internal constants (no user input), so shell quoting
    // is not a hazard here.
    shell: isWindows && command.endsWith('.cmd'),
  });
  if (result.error !== undefined) {
    fail(`${label}: failed to run \`${command}\` (${result.error.message})`);
  }
  if (result.status !== 0) {
    fail(`${label}: \`${command} ${args.join(' ')}\` exited with ${result.status}`);
  }
}

// 1. Build the web SPA. stage-web.mjs refuses to stage anything that is not a
// real SPA build, so the web build must come first.
run(pnpmCommand, ['-C', 'apps/mirri-web', 'run', 'build'], 'building the web SPA');

// 2. Stage the SPA into the rust-embed source directory (with --locked the
// cargo build below embeds exactly these assets).
run('node', ['rust/scripts/stage-web.mjs'], 'staging the web SPA');

// 3. Compile the host-target binary with the release profile. No --target, so
// cargo produces the native host build under rust/target/dist/. The manifest
// path is explicit because this script runs from the repo root, where cargo
// would otherwise find no Cargo.toml (the workspace lives under rust/).
run(
  'cargo',
  ['build', '--profile', 'dist', '--locked', '-p', 'mirri-cli', '--manifest-path', 'rust/Cargo.toml'],
  'building the mirri binary',
);

// 4. Copy the binary into the desktop staging directory consumed by
// scripts/before-pack.cjs and the dev resolution order. All step-4 IO lives in
// one try/catch so any failure exits through the same fail-loud channel as the
// build steps above (label + explicit error + non-zero exit), never as an
// unhandled rejection.
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
