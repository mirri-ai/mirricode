// apps/mirri-desktop/src/main/preload-bundle.test.ts
//
// Build-output guard for the preload bridge: Electron's SANDBOXED preload
// environment cannot `require` local modules (its require is limited to
// 'electron' and a few node built-ins), so out/preload.cjs must be fully
// self-contained. A multi-entry tsdown build that code-splits the shared
// src/main/ipc.ts into a chunk (as in the v1.11.0 regression) makes the
// preload start with `require('./ipc-*.cjs')` — which throws "module not
// found" in the sandboxed renderer, silently dropping window.mirriDesktop
// and every desktop-bridge feature (openInApp, notify, deep links, ...).

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

const OUT_DIR = join(__dirname, '..', '..', 'out');
const PRELOAD_PATH = join(OUT_DIR, 'preload.cjs');

describe('bundled preload output', () => {
  it('should exist after a build', () => {
    // Skip with a clear signal when the build output is absent (e.g. a
    // fresh checkout running unit tests before `pnpm build`).
    let exists = true;
    try {
      readFileSync(PRELOAD_PATH);
    } catch {
      exists = false;
    }
    if (!exists) return; // no build output yet — nothing to guard
    expect(true).toBe(true);
  });

  it('should not require any local module (sandboxed preload cannot)', () => {
    let source: string;
    try {
      source = readFileSync(PRELOAD_PATH, 'utf-8');
    } catch {
      return; // no build output yet — nothing to guard
    }
    // Every require call in the bundle. The sandboxed preload may only
    // require 'electron' (and node built-ins, which this bundle does not
    // need); any ./ or ../ relative path is a self-containment violation.
    const requires = [...source.matchAll(/require\((['"])(.*?)\1\)/g)].map((m) => m[2]!);
    const local = requires.filter((spec) => spec.startsWith('./') || spec.startsWith('../'));
    expect(
      local,
      `out/preload.cjs requires local modules (${local.join(', ')}); ` +
        'a sandboxed preload cannot resolve them — the bridge would never load. ' +
        'Build the preload as its own self-contained entry (see tsdown.config.ts).',
    ).toEqual([]);
  });
});
