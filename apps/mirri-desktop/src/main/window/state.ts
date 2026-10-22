// apps/mirri-desktop/src/main/window/state.ts
//
// Window bounds persistence (window-state.json under userData). The factory
// injects the state file path so tests can run against a temp dir; the
// production wiring in index.ts passes the real userData path.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';

import type { BrowserWindow } from 'electron';

export interface WindowBounds {
  width: number;
  height: number;
  x?: number;
  y?: number;
}

const DEFAULT_BOUNDS: WindowBounds = { width: 1280, height: 860 };

export function createWindowState(stateFilePath: string) {
  function loadBounds(): WindowBounds {
    try {
      const parsed = JSON.parse(readFileSync(stateFilePath, 'utf-8')) as Partial<WindowBounds>;
      if (typeof parsed.width === 'number' && typeof parsed.height === 'number') {
        return {
          width: parsed.width,
          height: parsed.height,
          x: typeof parsed.x === 'number' ? parsed.x : undefined,
          y: typeof parsed.y === 'number' ? parsed.y : undefined,
        };
      }
    } catch {
      // No saved state yet, or it is unreadable — fall back to defaults.
    }
    return DEFAULT_BOUNDS;
  }

  function saveBounds(win: BrowserWindow): void {
    try {
      const bounds = win.getBounds();
      mkdirSync(dirname(stateFilePath), { recursive: true });
      writeFileSync(
        stateFilePath,
        JSON.stringify({ width: bounds.width, height: bounds.height, x: bounds.x, y: bounds.y }),
      );
    } catch {
      // Best-effort; losing window position is not worth surfacing an error.
    }
  }

  return { loadBounds, saveBounds };
}
