import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { currentPlatform } from './platform';
import { resolveServerBinaryPath } from './server-binary';

const { mockedApp } = vi.hoisted(() => ({
  mockedApp: {
    isPackaged: false,
    getAppPath: vi.fn(() => '/default/desktop-root'),
  },
}));

vi.mock('electron', () => ({ app: mockedApp }));

let tmpRoot: string;

/** `<platform>-<arch>` for the current (real) process. */
function currentTarget(): string {
  return `${currentPlatform()}-${process.arch}`;
}

function executableName(): string {
  return currentPlatform() === 'win32' ? 'mirri.exe' : 'mirri';
}

/** Lay out `<tmp>/apps/mirri-desktop` + `<tmp>/rust` so dev path math matches the repo. */
function desktopRoot(): string {
  return join(tmpRoot, 'apps', 'mirri-desktop');
}

beforeEach(() => {
  tmpRoot = mkdtempSync(join(tmpdir(), 'mirri-server-binary-'));
  mockedApp.getAppPath.mockReturnValue(desktopRoot());
  mockedApp.isPackaged = false;
  delete process.env['MIRRICODE_DESKTOP_SERVER_BIN'];
});

afterEach(() => {
  rmSync(tmpRoot, { recursive: true, force: true });
  delete process.env['MIRRICODE_DESKTOP_SERVER_BIN'];
  vi.restoreAllMocks();
});

describe('resolveServerBinaryPath()', () => {
  it('should return the env override when MIRRICODE_DESKTOP_SERVER_BIN is set', () => {
    process.env['MIRRICODE_DESKTOP_SERVER_BIN'] = '/custom/path/to/mirri';

    expect(resolveServerBinaryPath()).toBe('/custom/path/to/mirri');
  });

  it('should resolve the packaged resources path when app is packaged', () => {
    mockedApp.isPackaged = true;
    const resources = join(tmpRoot, 'resources');
    Object.defineProperty(process, 'resourcesPath', { configurable: true, value: resources });

    expect(resolveServerBinaryPath()).toBe(
      join(resources, 'bin', currentTarget(), executableName()),
    );
  });

  it('should resolve build/backend staging in dev when present', () => {
    const staged = join(desktopRoot(), 'build', 'backend', currentTarget(), executableName());
    mkdirSync(join(desktopRoot(), 'build', 'backend', currentTarget()), { recursive: true });
    writeFileSync(staged, '');

    expect(resolveServerBinaryPath()).toBe(staged);
  });

  it('should fall back to rust target release then debug in dev', () => {
    const release = join(tmpRoot, 'rust', 'target', 'release', executableName());
    const debug = join(tmpRoot, 'rust', 'target', 'debug', executableName());
    mkdirSync(dirname(release), { recursive: true });
    mkdirSync(dirname(debug), { recursive: true });
    writeFileSync(release, '');
    writeFileSync(debug, '');

    expect(resolveServerBinaryPath()).toBe(release);

    rmSync(dirname(release), { recursive: true, force: true });
    expect(resolveServerBinaryPath()).toBe(debug);
  });

  it('should throw with actionable message when no binary exists', () => {
    const expectedCandidates = [
      join(desktopRoot(), 'build', 'backend', currentTarget(), executableName()),
      join(tmpRoot, 'rust', 'target', 'release', executableName()),
      join(tmpRoot, 'rust', 'target', 'debug', executableName()),
    ];

    const error = captureError(() => resolveServerBinaryPath());

    expect(error).toBeDefined();
    for (const candidate of expectedCandidates) {
      expect(error!.message).toContain(candidate);
    }
    expect(error!.message).toContain('MIRRICODE_DESKTOP_SERVER_BIN');
    expect(error!.message).toContain('pnpm -C apps/mirri-desktop run build:backend');
    expect(error!.message).toContain('cargo build -p mirri-cli');
  });

  it('should reject unsupported platform targets', () => {
    const platformDesc = Object.getOwnPropertyDescriptor(process, 'platform');
    const archDesc = Object.getOwnPropertyDescriptor(process, 'arch');
    try {
      Object.defineProperty(process, 'platform', { configurable: true, value: 'freebsd' });
      Object.defineProperty(process, 'arch', { configurable: true, value: 'x64' });

      const error = captureError(() => resolveServerBinaryPath());

      expect(error).toBeDefined();
      expect(error!.message).toContain('freebsd-x64');
    } finally {
      Object.defineProperty(process, 'platform', platformDesc!);
      Object.defineProperty(process, 'arch', archDesc!);
    }
  });
});

/** Run `fn` and return the thrown error, or undefined when it does not throw. */
function captureError(fn: () => void): Error | undefined {
  try {
    fn();
    return undefined;
  } catch (error) {
    return error instanceof Error ? error : new Error(String(error));
  }
}
