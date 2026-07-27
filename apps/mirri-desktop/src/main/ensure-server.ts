import { execFile } from 'node:child_process';
import { existsSync, readFileSync, unlinkSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

/** Overall budget for the bundled `mirri server run` to finish ensuring a daemon. */
const RUN_TIMEOUT_MS = 30_000;
/** How long to keep polling `/healthz` before declaring the daemon unhealthy. */
const HEALTH_TIMEOUT_MS = 20_000;
const HEALTH_POLL_MS = 200;
/** Lock file name for the Desktop daemon (separate from the CLI's `lock`). */
const DESKTOP_LOCK_NAME = 'desktop';
/** Timeout for the stale-daemon health check before deciding not to kill. */
const STALE_HEALTH_TIMEOUT_MS = 2_000;

/** Subset of the server lock JSON we read (apps/mirri-code writes the full shape). */
interface LockContents {
  pid: number;
  host?: string;
  port: number;
  /** Mirrors `LockContents.lock_name` from the server — identifies which
      daemon instance (e.g. `"desktop"`) owns this lock. */
  lock_name?: string;
}

/** Parsed stdout JSON emitted by `mirri server run --lock-name desktop`. */
interface DaemonInfo {
  origin: string;
  port: number;
  pid: number;
}

/** `<MIRRICODE_HOME>` or `~/.mirri-code` — must match the server's `resolveMirriHome`. */
export function mirriHome(): string {
  const override = process.env['MIRRICODE_HOME'];
  if (override !== undefined && override.trim().length > 0) {
    return override;
  }
  return join(homedir(), '.mirri-code');
}

/** Path of the Desktop daemon's lock file. */
function desktopLockPath(): string {
  return join(mirriHome(), 'server', DESKTOP_LOCK_NAME);
}

/** Background daemon log written by the SEA — surfaced in the error screen / menu. */
export function serverLogPath(): string {
  return join(mirriHome(), 'server', 'server-desktop.log');
}

function readLock(path: string): LockContents | null {
  try {
    const parsed = JSON.parse(readFileSync(path, 'utf-8')) as Partial<LockContents>;
    if (typeof parsed.port === 'number' && typeof parsed.pid === 'number') {
      return {
        pid: parsed.pid,
        port: parsed.port,
        host: typeof parsed.host === 'string' ? parsed.host : undefined,
        lock_name: typeof parsed.lock_name === 'string' ? parsed.lock_name : undefined,
      };
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Parse the stdout JSON line emitted by `mirri server run --lock-name desktop`.
 *
 * The daemon writes exactly one JSON line to stdout after it is ready:
 * `{"origin":"http://127.0.0.1:58827","port":58827,"pid":12345}`
 *
 * Returns `null` when the stdout does not contain a valid JSON line.
 */
export function parseDaemonInfo(stdout: string): DaemonInfo | null {
  for (const line of stdout.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed.startsWith('{') || !trimmed.includes('"origin"')) continue;
    try {
      const parsed = JSON.parse(trimmed) as Partial<DaemonInfo>;
      if (
        typeof parsed.origin === 'string' &&
        typeof parsed.port === 'number' &&
        typeof parsed.pid === 'number'
      ) {
        return parsed as DaemonInfo;
      }
    } catch {
      // Not valid JSON on this line — skip.
    }
  }
  return null;
}

async function isHealthy(origin: string, timeoutMs: number): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => {
    controller.abort();
  }, timeoutMs);
  try {
    const res = await fetch(`${origin}/api/v1/healthz`, { signal: controller.signal });
    if (!res.ok) {
      return false;
    }
    const body = (await res.json()) as { code?: unknown };
    return body.code === 0;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Run the bundled SEA's `server run --lock-name desktop`, which spawns a
 * Desktop-isolated daemon and outputs its origin/pid as a JSON line to stdout.
 */
function runServerRun(seaPath: string): Promise<DaemonInfo> {
  return new Promise((resolve, reject) => {
    execFile(
      seaPath,
      ['server', 'run', '--log-level', 'error', '--lock-name', 'desktop'],
      { timeout: RUN_TIMEOUT_MS },
      (error, stdout, stderr) => {
        if (error) {
          reject(new Error(`mirri server run failed: ${error.message}\n${stderr}`.trim()));
          return;
        }
        const info = parseDaemonInfo(stdout);
        if (info === null) {
          reject(
            new Error(
              `mirri server run did not emit origin JSON on stdout.\n${stderr}`.trim(),
            ),
          );
          return;
        }
        resolve(info);
      },
    );
  });
}

export interface EnsureServerResult {
  origin: string;
  /** PID of the Desktop daemon, for clean shutdown on quit. */
  pid: number;
  /** Path of the daemon's log file, surfaced in error UI. */
  logPath: string;
}

/**
 * Ensure the Desktop daemon is running and return its origin + pid.
 *
 * The Desktop app runs its own daemon instance (separate lock file, separate
 * log, separate port range) so it does not compete with the CLI's shared
 * daemon. Configuration (providers, MCP, skills, sessions) is still shared via
 * `MIRRICODE_HOME`.
 */
export async function ensureServer(seaPath: string): Promise<EnsureServerResult> {
  const info = await runServerRun(seaPath);

  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (await isHealthy(info.origin, 500)) {
      return { origin: info.origin, pid: info.pid, logPath: serverLogPath() };
    }
    await new Promise((resolve) => {
      setTimeout(resolve, HEALTH_POLL_MS);
    });
  }
  throw new Error(
    `Mirri server at ${info.origin} did not become healthy within ${HEALTH_TIMEOUT_MS}ms.`,
  );
}

/**
 * Kill a stale Desktop daemon left behind by a crashed previous session.
 *
 * **PID-reuse safety**: before sending SIGTERM, we verify the PID is still
 * our daemon by probing the daemon's recorded port with a Mirri healthz
 * request. Only when `GET /api/v1/healthz` returns `code: 0` do we confirm
 * ownership and kill. If the health check fails (PID was recycled, port was
 * released, or another server is on the port), we do NOT kill — the daemon's
 * idle-shutdown (60s) acts as the fallback.
 */
export async function killStaleDaemon(): Promise<void> {
  const lockFile = desktopLockPath();
  if (!existsSync(lockFile)) return;

  const lock = readLock(lockFile);
  if (lock === null) {
    // Corrupt / unreadable lock — clean it up.
    try {
      unlinkSync(lockFile);
    } catch {
      // best-effort
    }
    return;
  }

  // 1. PID alive check — ESRCH means the process is gone.
  let alive: boolean;
  try {
    process.kill(lock.pid, 0);
    alive = true;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === 'ESRCH') {
      // Process is dead — clean up the stale lock.
      process.stdout.write(
        `[mirri-desktop] stale daemon (pid=${lock.pid}, port=${lock.port}) is dead; ` +
          `removing lock file ${lockFile}\n`,
      );
      try {
        unlinkSync(lockFile);
      } catch {
        // best-effort
      }
      return;
    }
    // EPERM: process exists but owned by another user — can't kill, can't
    // verify. Leave it to idle shutdown.
    alive = true;
  }

  if (!alive) return;

  // 2. lock_name verification — confirm this lock file was written by a
  // Desktop daemon (not a CLI daemon or a foreign process). The lock file
  // path (`server/desktop`) already provides isolation, but `lock_name` is an
  // additional integrity check against file corruption or tampering.
  if (lock.lock_name !== DESKTOP_LOCK_NAME) {
    process.stderr.write(
      `[mirri-desktop] stale lock at ${lockFile} has lock_name=${String(lock.lock_name)} ` +
        `(expected "${DESKTOP_LOCK_NAME}"); not killing. Relying on idle shutdown.\n`,
    );
    return;
  }

  // 3. Port health check — verify this PID is actually our Mirri daemon.
  const host = lock.host !== undefined && lock.host !== '0.0.0.0' ? lock.host : '127.0.0.1';
  const origin = `http://${host}:${lock.port}`;
  const healthy = await isHealthy(origin, STALE_HEALTH_TIMEOUT_MS);
  if (!healthy) {
    // The PID is alive but the port is not responding as our daemon — it may
    // have been recycled. Do NOT kill; the idle-shutdown (60s) will clean up
    // the real daemon if it is still running somewhere.
    process.stderr.write(
      `[mirri-desktop] stale daemon (pid=${lock.pid}, port=${lock.port}) is alive but ` +
        `healthz at ${origin} failed; not killing (possible PID reuse). ` +
        `Lock: ${lockFile}. Relying on idle shutdown.\n`,
    );
    return;
  }

  // 4. Confirmed our daemon — SIGTERM it.
  try {
    process.kill(lock.pid, 'SIGTERM');
    process.stdout.write(
      `[mirri-desktop] killed stale daemon (pid=${lock.pid}, port=${lock.port}, ` +
        `origin=${origin}, lock=${lockFile})\n`,
    );
  } catch (error) {
    process.stderr.write(
      `[mirri-desktop] failed to kill stale daemon pid ${lock.pid}: ${error instanceof Error ? error.message : String(error)}\n`,
    );
  }

  // 5. Clean up the lock file (the daemon's release may not have fired).
  try {
    unlinkSync(lockFile);
  } catch {
    // best-effort
  }
}
