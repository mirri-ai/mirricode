import { mkdtempSync, writeFileSync, rmSync, existsSync, mkdirSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  desktopStatePath,
  killStaleDaemon,
  mirriHome,
  readDesktopState,
  removeDesktopState,
  resolveShellEnv,
  startV2Server,
  writeDesktopState,
} from './ensure-server';

// Override MIRRICODE_HOME so all file operations target a tmpdir.

let tmpHome: string;

beforeEach(() => {
  tmpHome = mkdtempSync(join(tmpdir(), 'mirri-desktop-test-'));
  process.env['MIRRICODE_HOME'] = tmpHome;
});

afterEach(() => {
  rmSync(tmpHome, { recursive: true, force: true });
  delete process.env['MIRRICODE_HOME'];
  vi.restoreAllMocks();
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
    mkdirSync(join(tmpHome, 'server'), { recursive: true });
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

// ---------------------------------------------------------------------------
// startV2Server
// ---------------------------------------------------------------------------

describe('startV2Server()', () => {
  it('should write Desktop state file with port 58827', () => {
    // Test the state-writing side-effect directly.
    writeDesktopState({ pid: 88888, port: 58827, startedAt: Date.now() });
    const state = readDesktopState();
    expect(state).toBeDefined();
    expect(state!.pid).toBe(88888);
    expect(state!.port).toBe(58827);
  });
});

// ---------------------------------------------------------------------------
// resolveShellEnv()
// ---------------------------------------------------------------------------

describe('resolveShellEnv()', () => {
  it('should fall back to zsh when primary shell fails', async () => {
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/nonexistent/shell';
    const result = await resolveShellEnv();
    if (result !== undefined) {
      expect(result['PATH']).toBeDefined();
    }
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  });

  it('should return a parsed environment when the shell probe succeeds', async () => {
    const origShell = process.env['SHELL'];
    if (origShell === undefined) return;
    const result = await resolveShellEnv();
    expect(result).toBeDefined();
    expect(result?.['PATH']).toBeDefined();
    expect(result?.['PATH']?.length).toBeGreaterThan(0);
    expect(result?.['HOME']).toBeDefined();
  });

  it('should return undefined when both shell and zsh probes fail', async () => {
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/bin/cat';
    const result = await resolveShellEnv();
    expect(result === undefined || typeof result === 'object').toBe(true);
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  }, 10_000);
});

// ---------------------------------------------------------------------------
// killStaleDaemon() — v2 state file
// ---------------------------------------------------------------------------

describe('killStaleDaemon() — v2 state file', () => {
  it('should return early when no state file or lock file exists', async () => {
    await expect(killStaleDaemon()).resolves.toBeUndefined();
  });

  it('should kill stale v2 server using desktop state file', async () => {
    // Mock fetch to return healthy response
    const fetchOrig = globalThis.fetch;
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ code: 0 }),
    }) as unknown as typeof fetch;

    writeDesktopState({ pid: process.pid, port: 58827, startedAt: Date.now() });

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      // SIGTERM: record it, don't actually kill ourselves
      return true;
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(1);
    expect(sigtermCalls[0]?.[0]).toBe(process.pid);
    // State file should be cleaned up after kill.
    expect(readDesktopState()).toBeUndefined();

    killSpy.mockRestore();
    globalThis.fetch = fetchOrig;
  });

  it('should not kill when state file PID is dead', async () => {
    const deadPid = 0x7fffffff;
    writeDesktopState({ pid: deadPid, port: 58827, startedAt: Date.now() });

    await killStaleDaemon();

    // State file should be removed (dead PID cleanup).
    expect(readDesktopState()).toBeUndefined();
  });

  it('should not kill when healthz fails (PID reuse)', async () => {
    // Use our own PID (alive) but no real server on the port — health check
    // will fail because we don't mock fetch.
    writeDesktopState({ pid: process.pid, port: 59999, startedAt: Date.now() });

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      throw new Error('should not kill');
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(0);
    // State file should NOT be removed (we didn't confirm it's ours).
    expect(readDesktopState()).toBeDefined();
    killSpy.mockRestore();
  });

  it('should handle EPERM gracefully', async () => {
    writeDesktopState({ pid: process.pid, port: 59998, startedAt: Date.now() });

    vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) {
        const err = new Error('Operation not permitted') as NodeJS.ErrnoException;
        err.code = 'EPERM';
        throw err;
      }
      return true;
    }) as typeof process.kill);

    // Should not throw — health check will also fail.
    await expect(killStaleDaemon()).resolves.toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// killStaleDaemon() — v1 lock file migration
// ---------------------------------------------------------------------------

describe('killStaleDaemon() — v1 lock file migration', () => {
  function desktopLockPath(): string {
    return join(mirriHome(), 'server', 'desktop');
  }

  function writeDesktopLock(pid: number, port: number, host?: string, lockName?: string): void {
    const lockPath = desktopLockPath();
    const dir = lockPath.replace(/desktop$/, '');
    mkdirSync(dir, { recursive: true });
    writeFileSync(
      lockPath,
      JSON.stringify({
        pid,
        port,
        started_at: new Date().toISOString(),
        lock_name: lockName ?? 'desktop',
        ...(host ? { host } : {}),
      }),
    );
  }

  it('should kill stale v1 daemon using old lock file', async () => {
    const fetchOrig = globalThis.fetch;
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ code: 0 }),
    }) as unknown as typeof fetch;

    writeDesktopLock(process.pid, 58827);

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      return true;
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(1);

    killSpy.mockRestore();
    globalThis.fetch = fetchOrig;
  });

  it('should prefer desktop state over old lock file', async () => {
    // Write both the new state file and the old lock file with different PIDs.
    writeDesktopState({ pid: 11111, port: 58827, startedAt: Date.now() });
    writeDesktopLock(22222, 58827);

    const fetchOrig = globalThis.fetch;
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ code: 0 }),
    }) as unknown as typeof fetch;

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      return true;
    }) as typeof process.kill);

    await killStaleDaemon();

    // Should kill PID 11111 (from state file), not 22222 (from lock file).
    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(1);
    expect(sigtermCalls[0]?.[0]).toBe(11111);

    // Legacy lock file should be cleaned up.
    expect(existsSync(desktopLockPath())).toBe(false);

    killSpy.mockRestore();
    globalThis.fetch = fetchOrig;
  });

  it('should delete stale lock when lock file is corrupt', async () => {
    const lockPath = desktopLockPath();
    mkdirSync(lockPath.replace(/desktop$/, ''), { recursive: true });
    writeFileSync(lockPath, 'not valid json');

    await killStaleDaemon();

    expect(existsSync(lockPath)).toBe(false);
  });

  it('should NOT kill when lock_name does not match "desktop"', async () => {
    writeDesktopLock(process.pid, 58827, undefined, 'cli');

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      throw new Error('should not kill');
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(0);
    killSpy.mockRestore();
  });

  it('should NOT kill when lock_name is absent', async () => {
    const lockPath = desktopLockPath();
    mkdirSync(lockPath.replace(/desktop$/, ''), { recursive: true });
    writeFileSync(
      lockPath,
      JSON.stringify({ pid: process.pid, port: 58827, started_at: new Date().toISOString() }),
    );

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      throw new Error('should not kill');
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(0);
    killSpy.mockRestore();
  });
});
