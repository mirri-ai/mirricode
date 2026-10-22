import { spawn } from 'node:child_process';
import {
  chmodSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  statSync,
  unlinkSync,
} from 'node:fs';
import { dirname, join } from 'node:path';

import { app } from 'electron';

import { devRepoRoot } from '../server-binary';
import { resolveShellEnv } from './shell-env';
import {
  cleanupLegacyLock,
  DESKTOP_LOCK_NAME,
  type DesktopState,
  desktopLockPath,
  type LockContents,
  mirriHome,
  readDesktopState,
  readLock,
  removeDesktopState,
  serverLogPath,
  writeDesktopState,
} from './state';

// Public surface of the server module folder (index.ts / menu.ts / screens.ts
// consume these through `./server/daemon`).
export { mirriHome, serverLogPath } from './state';

/** Preferred port for the Desktop server (200 above the CLI default of 58627). */
const DESKTOP_PORT = 58_627 + 200; // 58827
/** How long to keep polling `/healthz` before declaring the server unhealthy. */
const HEALTH_TIMEOUT_MS = 20_000;
const HEALTH_POLL_MS = 200;
/** Timeout for the stale-server health check before deciding not to kill. */
const STALE_HEALTH_TIMEOUT_MS = 2_000;

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

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
 * Returns true when the server is listening and accepting traffic — whether or
 * not background startup work (reconcile/prewarm) has finished. Accepts both
 * `code:0` (fully ready) and `code:1` (listening-but-not-ready). Used in
 * Phase 1 of `ensureServer` so `loadURL` can happen as soon as the port opens.
 */
async function isListening(origin: string, timeoutMs: number): Promise<boolean> {
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
    return body.code === 0 || body.code === 1;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// Mirri server spawn
// ---------------------------------------------------------------------------

/** Spawn `mirri web` (Rust binary) as a detached child process and return its PID. */
export function startMirriServer(
  serverBinaryPath: string,
  shellEnv: Record<string, string> | undefined,
): { pid: number; logPath: string } {
  const env = shellEnv ?? process.env;
  const logPath = serverLogPath();
  mkdirSync(dirname(logPath), { recursive: true });

  const args = [
    'web',
    '--no-open',
    '--port',
    String(DESKTOP_PORT),
    '--log-level',
    'error',
  ];

  // Dev: point the server at the locally built web SPA when one exists. The
  // packaged build never appends `--web-dir` — the SPA is compiled into the
  // binary at build time. When the SPA bundle is missing in dev, the server
  // falls back to its embedded/probe assets; warn so the developer knows why.
  if (!app.isPackaged) {
    const webDistIndex = join(devRepoRoot(), 'apps', 'mirri-web', 'dist', 'index.html');
    if (existsSync(webDistIndex)) {
      args.push('--web-dir', dirname(webDistIndex));
    } else {
      console.warn(
        `[mirri-desktop] SPA bundle not found at ${webDistIndex}; the server will serve ` +
          `its embedded/probe assets instead. Run \`pnpm -C apps/mirri-web run build\` first.`,
      );
    }
  }

  const child = spawn(serverBinaryPath, args, {
    detached: true,
    env,
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  // Redirect stdout + stderr to the Desktop log file. Best-effort: an error
  // on the log stream (e.g. unwritable directory) must never crash the main
  // process — the server itself keeps running.
  const logStream = createWriteStream(logPath, { flags: 'a' });
  logStream.on('error', (error) => {
    // Keep swallowing the error — it must never crash the main process — but
    // surface it so log-file problems are diagnosable from the console.
    console.warn(`[mirri-desktop] failed to write server log to ${logPath}:`, error);
  });
  child.stdout?.pipe(logStream);
  child.stderr?.pipe(logStream);

  child.unref();

  // Write Desktop state immediately so killStaleDaemon can find it even if
  // the Desktop process crashes before healthz passes.
  writeDesktopState({ pid: child.pid!, port: DESKTOP_PORT, startedAt: Date.now() });

  return { pid: child.pid!, logPath };
}

// ---------------------------------------------------------------------------
// Token permission pre-flight
// ---------------------------------------------------------------------------

/** On-disk filename of the server's persistent bearer token (under MIRRICODE_HOME). */
const SERVER_TOKEN_FILENAME = 'server.token';

/**
 * Ensure the server token file is not group/world readable before the server
 * is spawned. The Rust server refuses to start on a permissive token file
 * (fail-loud), so pre-flight-chmod it to `0o600` whenever it exists with
 * group/world bits set.
 *
 * Best-effort: chmod failures are logged with `console.warn` and ignored —
 * the Rust server keeps its own fail-loud path for cases we cannot fix.
 */
export function ensureServerTokenPrivate(): void {
  const tokenPath = join(mirriHome(), SERVER_TOKEN_FILENAME);
  let mode: number;
  try {
    mode = statSync(tokenPath).mode;
  } catch {
    return; // no token file yet — nothing to fix
  }
  if ((mode & 0o077) === 0) {
    return; // already private
  }
  try {
    chmodSync(tokenPath, 0o600);
  } catch (error) {
    console.warn(
      `[mirri-desktop] failed to chmod server token ${tokenPath} to 0600: ` +
        `${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

// ---------------------------------------------------------------------------
// ensureServer
// ---------------------------------------------------------------------------

export interface EnsureServerResult {
  origin: string;
  /** PID of the Desktop server, for clean shutdown on quit. */
  pid: number;
  /** Path of the server's log file, surfaced in error UI. */
  logPath: string;
  /**
   * Resolves when the server finishes background startup work (workspace
   * catalog reconcile, session index prewarm) and healthz switches to
   * `code:0`. The caller can `loadURL` immediately after `ensureServer`
   * returns, and optionally await this promise before performing
   * workspace-dependent operations.
   */
  readyPromise: Promise<void>;
}

/**
 * Ensure the Desktop server is running and return its origin + pid.
 *
 * The Desktop app runs its own server instance (separate port, separate state
 * file, separate log) so it does not compete with the CLI's shared daemon.
 * Configuration (providers, MCP, skills, sessions) is still shared via
 * `MIRRICODE_HOME`.
 *
 * Uses the Rust-compiled `mirri web` instead of the legacy TS SEA binary
 * (`mirri server run` / v1 engine).
 *
 * Two-phase startup:
 *   Phase 1 — wait for the server to be *listening* (`code:0 | code:1`).
 *             Returns as soon as the port opens so the caller can `loadURL`.
 *   Phase 2 — poll for `code:0` in the background; `readyPromise` resolves
 *             when reconcile/prewarm finish (or after the timeout).
 */
export async function ensureServer(serverBinaryPath: string): Promise<EnsureServerResult> {
  const origin = `http://127.0.0.1:${DESKTOP_PORT}`;

  // 1. If a server is already listening on our port (any readiness state), reuse it.
  if (await isListening(origin, 500)) {
    // Try to recover the PID from the Desktop state file or the v1 lock file.
    const state = readDesktopState();
    const pid = state?.pid ?? recoverPidFromLegacyLock();
    const logPath = serverLogPath();
    return { origin, pid: pid ?? -1, logPath, readyPromise: waitUntilReady(origin) };
  }

  // 1b. We are about to spawn our own server: pre-flight the token file so
  //     the Rust server does not fail-loud on permissive permissions.
  ensureServerTokenPrivate();

  // 2. Resolve the user's interactive shell environment before spawning the
  //    server. This ensures the server (and its MCP stdio child processes)
  //    can find user-installed tools like npx, uvx, node, etc. even when
  //    the Desktop app was launched from Finder with a minimal PATH.
  const shellEnv = await resolveShellEnv();

  const { pid, logPath } = startMirriServer(serverBinaryPath, shellEnv);

  // 3. Phase 1: wait for the server to start listening (code:0 or code:1).
  //    Return as soon as the port opens — loadURL can happen immediately.
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (await isListening(origin, 500)) {
      return { origin, pid, logPath, readyPromise: waitUntilReady(origin) };
    }
    await new Promise((resolve) => {
      setTimeout(resolve, HEALTH_POLL_MS);
    });
  }
  throw new Error(
    `Mirri server at ${origin} did not become healthy within ${HEALTH_TIMEOUT_MS}ms.`,
  );
}

/**
 * Phase 2: poll until the server reports `code:0` (fully ready) or the health
 * timeout expires. Resolves in either case so callers can always await it.
 */
async function waitUntilReady(origin: string): Promise<void> {
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (await isHealthy(origin, 500)) {
      return;
    }
    await new Promise((resolve) => {
      setTimeout(resolve, HEALTH_POLL_MS);
    });
  }
  // Timed out waiting for code:0 — resolve anyway so callers don't hang.
}

/** Try to recover a PID from the legacy v1 lock file. */
function recoverPidFromLegacyLock(): number | undefined {
  const lockFile = desktopLockPath();
  if (!existsSync(lockFile)) return undefined;
  const lock = readLock(lockFile);
  return lock?.pid;
}

// ---------------------------------------------------------------------------
// killStaleDaemon
// ---------------------------------------------------------------------------

/**
 * Kill a stale Desktop server left behind by a crashed previous session.
 *
 * Reads the new `desktop-state.json` first, then falls back to the legacy
 * v1 `desktop` lock file for migration compatibility.
 *
 * **PID-reuse safety**: before sending SIGTERM, we verify the PID is still
 * our server by probing the recorded port with a Mirri healthz request.
 * Only when `GET /api/v1/healthz` returns `code: 0` do we confirm ownership
 * and kill. If the health check fails (PID was recycled, port was released,
 * or another server is on the port), we do NOT kill.
 */
export async function killStaleDaemon(): Promise<void> {
  // Prefer the new Desktop state file.
  const state = readDesktopState();
  if (state !== undefined) {
    await killStaleFromState(state);
    // Also clean up any legacy lock file that may coexist.
    cleanupLegacyLock();
    return;
  }

  // Fall back to the legacy v1 lock file (migration).
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

  await killStaleFromLock(lock, lockFile);
}

/** Try to kill a stale server described by the new Desktop state file. */
async function killStaleFromState(state: DesktopState): Promise<void> {
  // 1. PID alive check.
  let alive: boolean;
  try {
    process.kill(state.pid, 0);
    alive = true;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === 'ESRCH') {
      // Process is dead — clean up the stale state file.
      process.stdout.write(
        `[mirri-desktop] stale server (pid=${state.pid}, port=${state.port}) is dead; ` +
          `removing state file\n`,
      );
      removeDesktopState();
      return;
    }
    // EPERM: process exists but owned by another user — can't kill.
    alive = true;
  }

  if (!alive) return;

  // 2. Port health check — verify this PID is actually our Mirri server.
  const origin = `http://127.0.0.1:${state.port}`;
  const healthy = await isHealthy(origin, STALE_HEALTH_TIMEOUT_MS);
  if (!healthy) {
    // The PID is alive but the port is not responding as our server — it may
    // have been recycled. Do NOT kill.
    process.stderr.write(
      `[mirri-desktop] stale server (pid=${state.pid}, port=${state.port}) is alive but ` +
        `healthz at ${origin} failed; not killing (possible PID reuse).\n`,
    );
    return;
  }

  // 3. Confirmed our server — SIGTERM it.
  try {
    process.kill(state.pid, 'SIGTERM');
    process.stdout.write(
      `[mirri-desktop] killed stale server (pid=${state.pid}, port=${state.port})\n`,
    );
  } catch (error) {
    process.stderr.write(
      `[mirri-desktop] failed to kill stale server pid ${state.pid}: ${error instanceof Error ? error.message : String(error)}\n`,
    );
  }

  // 4. Clean up the state file.
  removeDesktopState();
}

/** Try to kill a stale v1 daemon described by the legacy lock file. */
async function killStaleFromLock(lock: LockContents, lockFile: string): Promise<void> {
  // 1. PID alive check.
  let alive: boolean;
  try {
    process.kill(lock.pid, 0);
    alive = true;
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === 'ESRCH') {
      process.stdout.write(
        `[mirri-desktop] stale v1 daemon (pid=${lock.pid}, port=${lock.port}) is dead; ` +
          `removing lock file ${lockFile}\n`,
      );
      try {
        unlinkSync(lockFile);
      } catch {
        // best-effort
      }
      return;
    }
    alive = true;
  }

  if (!alive) return;

  // 2. lock_name verification (v1-specific integrity check).
  if (lock.lock_name !== DESKTOP_LOCK_NAME) {
    process.stderr.write(
      `[mirri-desktop] stale lock at ${lockFile} has lock_name=${String(lock.lock_name)} ` +
        `(expected "${DESKTOP_LOCK_NAME}"); not killing.\n`,
    );
    return;
  }

  // 3. Port health check.
  const host = lock.host !== undefined && lock.host !== '0.0.0.0' ? lock.host : '127.0.0.1';
  const origin = `http://${host}:${lock.port}`;
  const healthy = await isHealthy(origin, STALE_HEALTH_TIMEOUT_MS);
  if (!healthy) {
    process.stderr.write(
      `[mirri-desktop] stale v1 daemon (pid=${lock.pid}, port=${lock.port}) is alive but ` +
        `healthz at ${origin} failed; not killing (possible PID reuse).\n`,
    );
    return;
  }

  // 4. Confirmed our daemon — SIGTERM it.
  try {
    process.kill(lock.pid, 'SIGTERM');
    process.stdout.write(
      `[mirri-desktop] killed stale v1 daemon (pid=${lock.pid}, port=${lock.port})\n`,
    );
  } catch (error) {
    process.stderr.write(
      `[mirri-desktop] failed to kill stale v1 daemon pid ${lock.pid}: ${error instanceof Error ? error.message : String(error)}\n`,
    );
  }

  // 5. Clean up the lock file.
  try {
    unlinkSync(lockFile);
  } catch {
    // best-effort
  }
}
