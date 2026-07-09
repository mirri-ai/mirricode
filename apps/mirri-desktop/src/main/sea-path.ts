import { join } from 'node:path';

import { app } from 'electron';

// The bundled backend targets the same 6 platform/arch pairs the mirri-code
// native SEA build supports (apps/mirri-code/scripts/native/native-deps.mjs).
const SUPPORTED_TARGETS = new Set([
  'darwin-arm64',
  'darwin-x64',
  'linux-arm64',
  'linux-x64',
  'win32-arm64',
  'win32-x64',
]);

/** `<platform>-<arch>` triple for the current process, validated against the SEA targets. */
export function currentTarget(): string {
  const target = `${process.platform}-${process.arch}`;
  if (!SUPPORTED_TARGETS.has(target)) {
    throw new Error(`No bundled Mirri server for this platform: ${target}`);
  }
  return target;
}

function executableName(): string {
  return process.platform === 'win32' ? 'mirri.exe' : 'mirri';
}

/**
 * Absolute path to the bundled SEA backend executable.
 *
 * - packaged: `<resources>/bin/<target>/mirri[.exe]` — placed there by
 *   electron-builder `extraResources`.
 * - dev: `apps/mirri-code/dist-native/bin/<target>/mirri[.exe]` — produced by
 *   `pnpm -C apps/mirri-code build:native:sea`. In dev `app.getAppPath()` is
 *   `apps/mirri-desktop`, so the sibling app is one level up.
 */
export function resolveSeaPath(): string {
  const target = currentTarget();
  const exe = executableName();
  if (app.isPackaged) {
    return join(process.resourcesPath, 'bin', target, exe);
  }
  return join(app.getAppPath(), '..', 'mirri-code', 'dist-native', 'bin', target, exe);
}
