#!/usr/bin/env node
// Syncs the rust workspace and desktop versions from the release anchor
// package (@mirri-ai/release, see docs/specs/release-architecture.md ADR-002).
//
// The anchor package is the version source of truth: all changesets bump it,
// and the rust workspace + desktop package must report the same version or
// users see mismatched --version output and broken update comparisons.
//
// Usage:
//   node rust/scripts/sync-version.mjs          # sync versions to the anchor
//   node rust/scripts/sync-version.mjs --check # exit 1 on drift (CI)

import { readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const rustDir = join(import.meta.dirname, '..');
const repoDir = join(rustDir, '..');
const desktopPackagePath = join(repoDir, 'apps/mirri-desktop/package.json');
const workspaceTomlPath = join(rustDir, 'Cargo.toml');

export function readAnchorVersion(repoDir = join(import.meta.dirname, '..', '..')) {
  return JSON.parse(
    readFileSync(join(repoDir, 'packages/release/package.json'), 'utf-8'),
  ).version;
}

export function readDesktopVersion(repoDir = join(import.meta.dirname, '..', '..')) {
  return JSON.parse(
    readFileSync(join(repoDir, 'apps/mirri-desktop/package.json'), 'utf-8'),
  ).version;
}

export function readRustWorkspaceVersion(rustDir = join(import.meta.dirname, '..')) {
  const toml = readFileSync(join(rustDir, 'Cargo.toml'), 'utf-8');
  const section = toml.match(/^\[workspace\.package\]\n(?:(?!\[)[^\n]*\n)*/m);
  const versionMatch = section?.[0].match(/^version\s*=\s*"([^"]+)"/m);
  return versionMatch?.[1] ?? null;
}

export function checkVersionSync({ repoDir, rustDir }) {
  const problems = [];
  const anchorVersion = readAnchorVersion(repoDir);
  if (typeof anchorVersion !== 'string' || !/^\d+\.\d+\.\d+/.test(anchorVersion)) {
    throw new Error(`invalid anchor version in ${repoDir}/packages/release/package.json`);
  }
  const workspaceVersion = readRustWorkspaceVersion(rustDir);
  if (workspaceVersion !== anchorVersion) {
    problems.push(
      `rust workspace version ${workspaceVersion} != anchor version ${anchorVersion}`,
    );
  }
  const desktopVersion = readDesktopVersion(repoDir);
  if (desktopVersion !== anchorVersion) {
    problems.push(
      `desktop version ${desktopVersion} != anchor version ${anchorVersion}`,
    );
  }
  // Cargo.lock entries for workspace members must match too (read-only when
  // the file is absent, e.g. unit tests on temp trees).
  let lock = null;
  try {
    lock = readFileSync(join(rustDir, 'Cargo.lock'), 'utf-8');
  } catch {
    lock = null;
  }
  if (lock !== null) {
    let found = 0;
    for (const match of lock.matchAll(
      /\[\[package\]\]\s*\nname = "(mirri-[^"]+)"\s*\nversion = "([^"]+)"/g,
    )) {
      found += 1;
      if (match[2] !== anchorVersion) {
        problems.push(`Cargo.lock ${match[1]} version ${match[2]} != ${anchorVersion}`);
      }
    }
    if (found === 0) {
      problems.push('no mirri-* packages found in Cargo.lock — is the lockfile resolved?');
    }
  }
  return problems;
}

function sync() {
  const anchorVersion = readAnchorVersion(repoDir);
  const oldVersion = readRustWorkspaceVersion(rustDir);
  if (oldVersion !== anchorVersion) {
    const toml = readFileSync(workspaceTomlPath, 'utf-8');
    const section = toml.match(/^\[workspace\.package\]\n(?:(?!\[)[^\n]*\n)*/m);
    if (!section || !/^version\s*=\s*"/m.test(section[0])) {
      console.error(`no version in [workspace.package] of ${workspaceTomlPath}`);
      process.exit(1);
    }
    const updated = section[0].replace(
      /^version\s*=\s*"[^"]+"/m,
      `version = "${anchorVersion}"`,
    );
    writeFileSync(workspaceTomlPath, toml.replace(section[0], updated));
    console.log(`rust workspace version: ${oldVersion} -> ${anchorVersion}`);
  }

  const desktopVersion = readDesktopVersion(repoDir);
  if (desktopVersion !== anchorVersion) {
    const pkg = JSON.parse(readFileSync(desktopPackagePath, 'utf-8'));
    pkg.version = anchorVersion;
    writeFileSync(desktopPackagePath, `${JSON.stringify(pkg, null, 2)}\n`);
    console.log(`desktop version: ${desktopVersion} -> ${anchorVersion}`);
  }

  const result = spawnSync('cargo', ['update', '--workspace'], {
    cwd: rustDir,
    stdio: 'inherit',
  });
  if (result.status !== 0) {
    console.error('cargo update --workspace failed');
    process.exit(result.status ?? 1);
  }
  const problems = checkVersionSync({ repoDir, rustDir });
  if (problems.length > 0) {
    for (const problem of problems) console.error(`  - ${problem}`);
    process.exit(1);
  }
  console.log(`version sync ok: ${anchorVersion} (workspace + desktop + lock)`);
}

if (process.argv.includes('--check')) {
  const problems = checkVersionSync({ repoDir, rustDir });
  if (problems.length > 0) {
    console.error('version drift detected:');
    for (const problem of problems) console.error(`  - ${problem}`);
    console.error('run `node rust/scripts/sync-version.mjs` to fix');
    process.exit(1);
  }
  console.log(`version sync ok: ${readAnchorVersion(repoDir)} (workspace + desktop + lock)`);
} else if (resolve(process.argv[1]) === import.meta.filename) {
  sync();
}
