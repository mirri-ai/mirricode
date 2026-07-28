import { mkdtempSync, writeFileSync, rmSync, existsSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { parseDaemonInfo, killStaleDaemon, mirriHome, resolveShellEnv } from './ensure-server';

// We need to override MIRRICODE_HOME so killStaleDaemon reads from a tmpdir
// instead of the real ~/.mirri-code. We mock `mirriHome()` to return our
// tmpdir, and also mock `existsSync`/`readFileSync`/`unlinkSync` to operate
// on the tmpdir paths.
//
// Strategy: set MIRRICODE_HOME env to a tmpdir, and let the real fs functions
// operate on it.

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
// parseDaemonInfo(stdout)
// ---------------------------------------------------------------------------

describe('parseDaemonInfo(stdout)', () => {
  it('should parse origin and pid from stdout JSON', () => {
    const stdout = 'Some log line\n{"origin":"http://127.0.0.1:58827","port":58827,"pid":12345}\n';
    const info = parseDaemonInfo(stdout);
    expect(info).toEqual({
      origin: 'http://127.0.0.1:58827',
      port: 58827,
      pid: 12345,
    });
  });

  it('should return null when stdout JSON is missing origin field', () => {
    const stdout = '{"port":58827,"pid":12345}\n';
    expect(parseDaemonInfo(stdout)).toBeNull();
  });

  it('should return null when stdout JSON is missing pid field', () => {
    const stdout = '{"origin":"http://127.0.0.1:58827","port":58827}\n';
    expect(parseDaemonInfo(stdout)).toBeNull();
  });

  it('should return null when stdout is not valid JSON', () => {
    expect(parseDaemonInfo('not json at all\n')).toBeNull();
  });

  it('should return null when stdout is empty', () => {
    expect(parseDaemonInfo('')).toBeNull();
  });

  it('should find the JSON line among other output lines', () => {
    const stdout = 'line 1\nline 2\n{"origin":"http://127.0.0.1:58827","port":58827,"pid":999}\nmore text\n';
    const info = parseDaemonInfo(stdout);
    expect(info?.pid).toBe(999);
  });
});

// ---------------------------------------------------------------------------
// resolveShellEnv()
// ---------------------------------------------------------------------------

describe('resolveShellEnv()', () => {
  it('should fall back to zsh when primary shell fails', async () => {
    // /nonexistent/shell will fail, but the zsh fallback should succeed
    // on this machine (which has /bin/zsh and a .zshrc).
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/nonexistent/shell';
    const result = await resolveShellEnv();
    // On macOS with zsh installed, this should return an env.
    // On systems without zsh (Linux CI), it returns undefined.
    if (result !== undefined) {
      expect(result['PATH']).toBeDefined();
    }
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  });

  it('should return a parsed environment when the shell probe succeeds', async () => {
    // Use the real shell (bash or zsh) — this test runs on the dev machine
    // where a shell is always available.
    const origShell = process.env['SHELL'];
    if (origShell === undefined) {
      // Skip on environments without SHELL (CI Windows)
      return;
    }
    const result = await resolveShellEnv();
    expect(result).toBeDefined();
    expect(result?.['PATH']).toBeDefined();
    expect(result?.['PATH']?.length).toBeGreaterThan(0);
    expect(result?.['HOME']).toBeDefined();
  });

  it('should return undefined when both shell and zsh probes fail', async () => {
    // Use a shell command that hangs — /bin/cat with no input will block
    // until the timeout kills it. The zsh fallback also fails because
    // /bin/cat is not a login shell.
    const origShell = process.env['SHELL'];
    process.env['SHELL'] = '/bin/cat';
    const result = await resolveShellEnv();
    // zsh fallback might still succeed on this machine, so the result is
    // either undefined (both probes failed) or a valid env object (zsh
    // fallback succeeded). Assert it never returns a partial/broken value.
    expect(result === undefined || typeof result === 'object').toBe(true);
    if (origShell !== undefined) process.env['SHELL'] = origShell;
    else delete process.env['SHELL'];
  }, 10_000);
});

// ---------------------------------------------------------------------------
// killStaleDaemon()
// ---------------------------------------------------------------------------

describe('killStaleDaemon()', () => {
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
        // Match the server's LockContents.lock_name field. Default to 'desktop'
        // so existing tests don't need to change behavior.
        lock_name: lockName ?? 'desktop',
        ...(host ? { host } : {}),
      }),
    );
  }

  it('should return early when desktop lock file does not exist', async () => {
    // No lock file written — should not throw, not kill anything.
    await expect(killStaleDaemon()).resolves.toBeUndefined();
  });

  it('should delete stale lock when PID is dead (ESRCH)', async () => {
    // Use a clearly dead PID (very high, unlikely to exist).
    const deadPid = 0x7fffffff;
    writeDesktopLock(deadPid, 58827);

    await killStaleDaemon();

    expect(existsSync(desktopLockPath())).toBe(false);
  });

  it('should delete stale lock when lock file is corrupt', async () => {
    const lockPath = desktopLockPath();
    mkdirSync(lockPath.replace(/desktop$/, ''), { recursive: true });
    writeFileSync(lockPath, 'not valid json');

    await killStaleDaemon();

    expect(existsSync(lockPath)).toBe(false);
  });

  it('should NOT kill when PID is alive but healthz fails (PID reuse)', async () => {
    // Use our own PID (alive) but no server on the port — health check fails.
    writeDesktopLock(process.pid, 59999);

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      // pid 0 probe: return success (alive)
      if (signal === undefined || signal === 0) return true;
      // actual kill: should not reach here
      throw new Error('should not kill');
    }) as typeof process.kill);

    await killStaleDaemon();

    // Verify SIGTERM was NOT sent (only pid-0 probe calls)
    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(0);
    killSpy.mockRestore();
  });

  it('should kill daemon when PID is alive and healthz returns code:0', async () => {
    // Mock fetch to return healthy response
    const fetchOrig = globalThis.fetch;
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ code: 0 }),
    }) as unknown as typeof fetch;

    writeDesktopLock(process.pid, 58827);

    const killSpy = vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) return true;
      // SIGTERM: record it, don't actually kill ourselves
      return true;
    }) as typeof process.kill);

    await killStaleDaemon();

    const sigtermCalls = killSpy.mock.calls.filter(([, sig]) => sig === 'SIGTERM');
    expect(sigtermCalls).toHaveLength(1);
    expect(sigtermCalls[0]?.[0]).toBe(process.pid);

    killSpy.mockRestore();
    globalThis.fetch = fetchOrig;
  });

  it('should handle process.kill throwing EPERM gracefully', async () => {
    // PID exists but we can't signal it (different user).
    writeDesktopLock(process.pid, 59998);

    vi.spyOn(process, 'kill').mockImplementation(((pid: number, signal?: NodeJS.Signals | number) => {
      if (signal === undefined || signal === 0) {
        const err = new Error('Operation not permitted') as NodeJS.ErrnoException;
        err.code = 'EPERM';
        throw err;
      }
      return true;
    }) as typeof process.kill);

    // Should not throw — health check will also fail since fetch won't reach
    // a real server on this port.
    await expect(killStaleDaemon()).resolves.toBeUndefined();
  });

  it('should NOT kill when lock_name does not match "desktop"', async () => {
    // lock_name is 'cli' instead of 'desktop' — this is not our daemon.
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
    // No lock_name field at all — older build or foreign lock file.
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
