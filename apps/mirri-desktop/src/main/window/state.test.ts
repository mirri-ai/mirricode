// apps/mirri-desktop/src/main/window/state.test.ts
//
// Window-bounds persistence, driven through the factory-injected state file
// path so the tests run against a temp dir instead of the real userData.

import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import { createWindowState } from './state';

function tempStateFile(): string {
  return join(mkdtempSync(join(tmpdir(), 'mirri-window-state-')), 'window-state.json');
}

describe('window state', () => {
  it('should return the default bounds when no state file exists yet', () => {
    const { loadBounds } = createWindowState(tempStateFile());
    expect(loadBounds()).toEqual({ width: 1280, height: 860 });
  });

  it('should persist bounds round-trip through saveBounds and loadBounds', () => {
    const file = tempStateFile();
    const { loadBounds, saveBounds } = createWindowState(file);
    // Minimal BrowserWindow-shaped object — saveBounds only reads getBounds().
    const fakeWin = { getBounds: () => ({ width: 1000, height: 700, x: 10, y: 20 }) };
    saveBounds(fakeWin as never);
    expect(loadBounds()).toEqual({ width: 1000, height: 700, x: 10, y: 20 });
  });

  it('should keep default bounds when the state file contains garbage', () => {
    const file = tempStateFile();
    writeFileSync(file, 'not json at all');
    const { loadBounds } = createWindowState(file);
    expect(loadBounds()).toEqual({ width: 1280, height: 860 });
  });
});
