import { execFile, spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from 'node:fs';
import { homedir, userInfo } from 'node:os';
import { dirname, join } from 'node:path';

/** Preferred port for the Desktop server (200 above the CLI default of 58627). */
const DESKTOP_PORT = 58_627 + 200; // 58827
/** How long to keep polling `/healthz` before declaring the server unhealthy. */
const HEALTH_TIMEOUT_MS = 20_000;
const HEALTH_POLL_MS = 200;
/** Timeout for the stale-server health check before deciding not to kill. */
const STALE_HEALTH_TIMEOUT_MS = 2_000;
/** Timeout for the login-shell environment probe. */
const SHELL_ENV_TIMEOUT_MS = 5_000;

/** Filename of the Desktop state file under `~/.mirri-code/server/`. */
const DESKTOP_STATE_FILENAME = 'desktop-state.json';

/** Legacy lock file name for v1 server migration (under `~/.mirri-code/server/`). */
const DESKTOP_LOCK_NAME = 'desktop';

/** Shape of the Desktop state file (written by this module, not by the server). */
interface DesktopState {
  pid: number;
  port: number;
  startedAt: number;
}

/** Subset of the v1 server lock JSON (kept for migration compatibility). */
interface LockContents {
  pid: number;
  host?: string;
  port: number;
  lock_name?: string;
}

/** `<MIRRICODE_HOME>` or `~/.mirri-code` — must match the server's `resolveMirriHome`. */
export function mirriHome(): string {
  const override = process.env['MIRRICODE_HOME'];
  if (override !== undefined && override.trim().length > 0) {
    return override;
  }
  return join(homedir(), '.mirri-code');
}

/** Path of the Desktop state file. */
export function desktopStatePath(): string {
  return join(mirriHome(), 'server', DESKTOP_STATE_FILENAME);
}

/** Path of the legacy v1 Desktop lock file (migration only). */
function desktopLockPath(): string {
  return join(mirriHome(), 'server', DESKTOP_LOCK_NAME);
}

/** Background server log — surfaced in the error screen / menu. */
export function serverLogPath(): string {
  return join(mirriHome(), 'server', 'server-desktop.log');
}

// ---------------------------------------------------------------------------
// Desktop state file read/write
// ---------------------------------------------------------------------------

/** Write the Desktop state file so `killStaleDaemon` can clean up on next launch. */
export function writeDesktopState(state: DesktopState): void {
  const path = desktopStatePath();
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(state));
}

/** Read the Desktop state file. Returns `undefined` when missing or corrupt. */
export function readDesktopState(): DesktopState | undefined {
  const path = desktopStatePath();
  if (!existsSync(path)) return undefined;
  try {
    const parsed = JSON.parse(readFileSync(path, 'utf-8')) as Partial<DesktopState>;
    if (typeof parsed.pid === 'number' && typeof parsed.port === 'number') {
      return {
        pid: parsed.pid,
        port: parsed.port,
        startedAt: typeof parsed.startedAt === 'number' ? parsed.startedAt : Date.now(),
      };
    }
    return undefined;
  } catch {
    return undefined;
  }
}

/** Remove the Desktop state file. Best-effort; errors are ignored. */
export function removeDesktopState(): void {
  try {
    unlinkSync(desktopStatePath());
  } catch {
    // best-effort
  }
}

// ---------------------------------------------------------------------------
// Legacy v1 lock file read (migration)
// ---------------------------------------------------------------------------

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
// Shell environment probe
// ---------------------------------------------------------------------------

/**
 * Resolve the user's full shell environment by running their login shell in
 * interactive mode (`$SHELL -lic /usr/bin/env`). GUI-launched apps inherit a
 * minimal `PATH` that misses nvm, Homebrew, and other user-installed tool
 * directories. The login-shell profile alone (`-l`) is not enough because many
 * PATH additions live in `.bashrc` / `.zshrc` which are only sourced for
 * interactive shells. The `-i` flag forces bash/zsh into interactive mode so
 * those files are read.
 *
 * Returns the merged environment, or `undefined` when the probe fails (no
 * shell, spawn error, timeout). The caller falls back to `process.env` in that
 * case.
 */
export function resolveShellEnv(): Promise<Record<string, string> | undefined> {
  const envShell = process.env['SHELL']?.trim();
  let shell: string | undefined;
  if (envShell !== undefined && envShell.length > 0) {
    shell = envShell;
  } else {
    try {
      const s = userInfo().shell;
      shell = s !== null && s.length > 0 ? s : undefined;
    } catch {
      shell = undefined;
    }
  }

  if (shell === undefined) return tryZshShellEnv();

  return new Promise((resolve) => {
    execFile(
      shell,
      ['-lic', '/usr/bin/env'],
      { encoding: 'utf8', timeout: SHELL_ENV_TIMEOUT_MS, windowsHide: true },
      (error, stdout) => {
        if (error !== null) {
          // Fall back to zsh — many users have $SHELL=bash but their PATH
          // config lives in .zshrc (common when using Warp terminal).
          void tryZshShellEnv().then(resolve);
          return;
        }
        const env = parseEnvOutput(stdout);
        if (env === undefined) {
          void tryZshShellEnv().then(resolve);
          return;
        }
        resolve(env);
      },
    );
  });
}

/**
 * Fallback: try zsh when the primary shell probe failed or returned an
 * incomplete PATH.
 */
function tryZshShellEnv(): Promise<Record<string, string> | undefined> {
  return new Promise((resolve) => {
    execFile(
      '/bin/zsh',
      ['-lic', '/usr/bin/env'],
      { encoding: 'utf8', timeout: SHELL_ENV_TIMEOUT_MS, windowsHide: true },
      (error, stdout) => {
        if (error !== null) {
          resolve(undefined);
          return;
        }
        resolve(parseEnvOutput(stdout));
      },
    );
  });
}

function parseEnvOutput(stdout: string): Record<string, string> | undefined {
  const env: Record<string, string> = {};
  for (const line of stdout.split('\n')) {
    const eq = line.indexOf('=');
    if (eq <= 0) continue;
    const key = line.slice(0, eq);
    const value = line.slice(eq + 1).trim();
    if (key.length > 0 && value.length > 0) env[key] = value;
  }
  if (env['PATH'] === undefined || env['PATH'].length === 0) {
    return undefined;
  }
  return env;
}

// ---------------------------------------------------------------------------
// V2 server spawn
// ---------------------------------------------------------------------------

/** Spawn `mirri web` as a detached child process and return its PID. */
export function startV2Server(
  seaPath: string,
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

  const child = spawn(seaPath, args, {
    detached: true,
    env,
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  // Redirect stdout + stderr to the Desktop log file.
  const logStream = createWriteStream(logPath, { flags: 'a' });
  child.stdout?.pipe(logStream);
  child.stderr?.pipe(logStream);

  child.unref();

  // Write Desktop state immediately so killStaleDaemon can find it even if
  // the Desktop process crashes before healthz passes.
  writeDesktopState({ pid: child.pid!, port: DESKTOP_PORT, startedAt: Date.now() });

  return { pid: child.pid!, logPath };
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
 * Uses `mirri web` (v2 engine / kap-server) instead of the legacy
 * `mirri server run` (v1 engine).
 *
 * Two-phase startup:
 *   Phase 1 — wait for the server to be *listening* (`code:0 | code:1`).
 *             Returns as soon as the port opens so the caller can `loadURL`.
 *   Phase 2 — poll for `code:0` in the background; `readyPromise` resolves
 *             when reconcile/prewarm finish (or after the timeout).
 */
export async function ensureServer(seaPath: string): Promise<EnsureServerResult> {
  const origin = `http://127.0.0.1:${DESKTOP_PORT}`;

  // 1. If a server is already listening on our port (any readiness state), reuse it.
  if (await isListening(origin, 500)) {
    // Try to recover the PID from the Desktop state file or the v1 lock file.
    const state = readDesktopState();
    const pid = state?.pid ?? recoverPidFromLegacyLock();
    const logPath = serverLogPath();
    return { origin, pid: pid ?? -1, logPath, readyPromise: waitUntilReady(origin) };
  }

  // 2. Resolve the user's interactive shell environment before spawning the
  //    server. This ensures the server (and its MCP stdio child processes)
  //    can find user-installed tools like npx, uvx, node, etc. even when
  //    the Desktop app was launched from Finder with a minimal PATH.
  const shellEnv = await resolveShellEnv();

  const { pid, logPath } = startV2Server(seaPath, shellEnv);

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

/** Remove the legacy v1 lock file if it exists. Best-effort. */
function cleanupLegacyLock(): void {
  const lockFile = desktopLockPath();
  if (!existsSync(lockFile)) return;
  try {
    unlinkSync(lockFile);
    process.stdout.write(
      `[mirri-desktop] removed legacy v1 lock file ${lockFile}\n`,
    );
  } catch {
    // best-effort
  }
}
