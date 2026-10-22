import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

import { app } from 'electron';

import { currentPlatform } from './platform';

// The bundled backend targets the same 6 platform/arch pairs the Rust
// mirri-cli build pipeline supports (rust/mirri-cli).
const SUPPORTED_TARGETS = new Set([
  'darwin-arm64',
  'darwin-x64',
  'linux-arm64',
  'linux-x64',
  'win32-arm64',
  'win32-x64',
]);

/** `<platform>-<arch>` triple for the current process, validated against the supported targets. */
export function currentTarget(): string {
  const target = `${currentPlatform()}-${process.arch}`;
  if (!SUPPORTED_TARGETS.has(target)) {
    throw new Error(`No bundled Mirri server for this platform: ${target}`);
  }
  return target;
}

function executableName(): string {
  return currentPlatform() === 'win32' ? 'mirri.exe' : 'mirri';
}

/**
 * Repo root in dev: `app.getAppPath()` is `apps/mirri-desktop`, so the repo
 * root is two levels up (`<repo>/apps/mirri-desktop` → `<repo>`).
 */
export function devRepoRoot(): string {
  return resolve(app.getAppPath(), '..', '..');
}

/**
 * Absolute path to the Rust-compiled mirri server binary.
 *
 * - packaged: `<resources>/bin/<target>/mirri[.exe]` — staged there by the
 *   electron-builder `extraResources` config.
 * - dev: probed in order:
 *   1. `MIRRICODE_DESKTOP_SERVER_BIN` — explicit override, returned as-is
 *      when set (no existence check).
 *   2. `<apps/mirri-desktop>/build/backend/<target>/mirri[.exe]` — the
 *      desktop-local staging directory.
 *   3. `<repoRoot>/rust/target/release/mirri[.exe]` — normal cargo release.
 *   4. `<repoRoot>/rust/target/debug/mirri[.exe]` — normal cargo debug.
 *
 * When none of the dev candidates exists, throws with the list of probed
 * paths and the two ways to obtain a binary (env override or building).
 */
export function resolveServerBinaryPath(): string {
  const target = currentTarget();
  const exe = executableName();
  if (app.isPackaged) {
    return join(process.resourcesPath, 'bin', target, exe);
  }

  const envOverride = process.env['MIRRICODE_DESKTOP_SERVER_BIN'];
  if (envOverride !== undefined && envOverride.trim().length > 0) {
    return envOverride;
  }

  const desktopRoot = app.getAppPath();
  const repoRoot = devRepoRoot();
  const candidates = [
    join(desktopRoot, 'build', 'backend', target, exe),
    join(repoRoot, 'rust', 'target', 'release', exe),
    join(repoRoot, 'rust', 'target', 'debug', exe),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }

  throw new Error(
    `No Mirri server binary found for platform "${target}". Probed:\n` +
      candidates.map((path) => `  - ${path}`).join('\n') +
      '\n' +
      'Set MIRRICODE_DESKTOP_SERVER_BIN to point at a built binary, or build one with ' +
      '`pnpm -C apps/mirri-desktop run build:backend` / `cargo build -p mirri-cli`.',
  );
}
