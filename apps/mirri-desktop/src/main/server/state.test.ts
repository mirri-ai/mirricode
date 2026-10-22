import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  desktopStatePath,
  mirriHome,
  readDesktopState,
  removeDesktopState,
  writeDesktopState,
} from './state';

// Override MIRRICODE_HOME so all file operations target a tmpdir.

let tmpHome: string;

beforeEach(() => {
  tmpHome = mkdtempSync(join(tmpdir(), 'mirri-desktop-test-'));
  process.env['MIRRICODE_HOME'] = tmpHome;
});

afterEach(() => {
  rmSync(tmpHome, { recursive: true, force: true });
  delete process.env['MIRRICODE_HOME'];
});

// ---------------------------------------------------------------------------
// Desktop state file
// ---------------------------------------------------------------------------

describe('Desktop state file', () => {
  it('should write and read desktop state', () => {
    writeDesktopState({ pid: 12345, port: 58827, startedAt: 1000 });
    const state = readDesktopState();
    expect(state).toEqual({ pid: 12345, port: 58827, startedAt: 1000 });
  });

  it('should return undefined when state file does not exist', () => {
    expect(readDesktopState()).toBeUndefined();
  });

  it('should return undefined when state file is corrupt', () => {
    const path = desktopStatePath();
    mkdirSync(join(mirriHome(), 'server'), { recursive: true });
    writeFileSync(path, 'not valid json');
    expect(readDesktopState()).toBeUndefined();
  });

  it('should delete state file on cleanup', () => {
    writeDesktopState({ pid: 12345, port: 58827, startedAt: 1000 });
    expect(existsSync(desktopStatePath())).toBe(true);
    removeDesktopState();
    expect(existsSync(desktopStatePath())).toBe(false);
  });
});
