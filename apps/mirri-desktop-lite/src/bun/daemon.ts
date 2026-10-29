// apps/mirri-desktop-lite/src/bun/daemon.ts
//
// Daemon orchestration, ported from the Electron shell's
// apps/mirri-desktop/src/main/server/{daemon,state}.ts with the electron
// dependency stripped (isPackaged / webDir injected as parameters, ADR-003).
// R8 semantics: reuse a running daemon (whichever shell started it), spawn
// only under an atomic lock, and on quit clean up ONLY the instance this
// shell spawned (owner field, ADR-006).
//
// Port + state separation (user decision 2026-10-27): Lite runs its daemon
// on its OWN port with its OWN state dir, fully decoupled from the Electron
// shell's daemon (58827 + ~/.mirri-code/server/). The two shells never
// touch each other's daemon lifecycle — that is the whole point of the
// separation.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, closeSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { homedir } from "node:os";

export interface DaemonPaths {
  /** Candidate mirri binary paths, in priority order. */
  binaries: string[];
  /** Directory holding the state file + spawn lock (shared with the Electron shell). */
  stateDir: string;
  /** The port the daemon serves the web UI on. */
  port: number;
}

export interface DaemonConfig {
  isPackaged: boolean;
  /** Repo root in dev (used to locate the debug binary + web dist). */
  devRepoRoot: string | null;
}

interface DaemonState {
  pid: number;
  port: number;
  owner: string;
  startedAt: string;
}

const HEALTH_TIMEOUT_MS = 20_000;
const HEALTH_POLL_MS = 250;
export const DAEMON_OWNER = 'mirri-desktop-lite';

function stateFile(paths: DaemonPaths): string {
  return join(paths.stateDir, 'desktop-state.json');
}

function lockFile(paths: DaemonPaths): string {
  return join(paths.stateDir, 'daemon-spawn.lock');
}

/** The real liveness probe — same endpoint + `code` check as the Electron
 *  shell (the daemon exposes /api/v1/healthz, NOT /healthz; the SPA
 *  fallback made a naive /healthz probe accidentally "work"). */
async function isHealthy(port: number): Promise<boolean> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 2_000);
    try {
      const res = await fetch(`http://127.0.0.1:${port}/api/v1/healthz`, { signal: controller.signal });
      if (!res.ok) return false;
      const body = (await res.json()) as { code?: unknown };
      return body.code === 0;
    } finally {
      clearTimeout(timer);
    }
  } catch {
    return false;
  }
}

function readState(paths: DaemonPaths): DaemonState | null {
  try {
    return JSON.parse(readFileSync(stateFile(paths), 'utf8')) as DaemonState;
  } catch {
    return null;
  }
}

function writeState(paths: DaemonPaths, state: DaemonState): void {
  mkdirSync(paths.stateDir, { recursive: true });
  writeFileSync(stateFile(paths), JSON.stringify(state, null, 2));
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** Try to acquire the spawn lock atomically (O_EXCL). A stale lock whose
 *  holder pid is dead is taken over; a live holder means "they are
 *  spawning, wait for their daemon". Returns a release fn or null. */
function acquireSpawnLock(paths: DaemonPaths): (() => void) | null {
  mkdirSync(paths.stateDir, { recursive: true });
  const lock = lockFile(paths);
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const fd = openSync(lock, 'wx'); // O_EXCL
      closeSync(fd);
      writeFileSync(lock, String(process.pid));
      return () => {
        try { rmSync(lock); } catch { /* best effort */ }
      };
    } catch {
      // Lock exists: stale (holder dead) → take over; live → give up.
      try {
        const holderPid = Number.parseInt(readFileSync(lock, 'utf8').trim(), 10);
        if (Number.isFinite(holderPid) && !pidAlive(holderPid)) {
          rmSync(lock);
          continue; // retry the O_EXCL create
        }
      } catch {
        rmSync(lock); // unreadable lock is as good as dead
        continue;
      }
      return null;
    }
  }
  return null;
}

export interface StartedDaemon {
  origin: string;
  spawnedByUs: boolean;
  reusedFrom?: 'state' | 'probe';
}

/** Resolve the daemon's web dist directory, mirroring the Electron shell's
 *  probe (only pass --web-dir when the dist actually exists). */
function resolveWebDir(config: DaemonConfig): string | null {
  if (config.isPackaged || !config.devRepoRoot) return null;
  const dist = join(config.devRepoRoot, 'apps/mirri-web/dist/index.html');
  return existsSync(dist) ? join(config.devRepoRoot, 'apps/mirri-web/dist') : null;
}

/** Locate the repo root by walking up from `start` until the monorepo marker
 *  (pnpm-workspace.yaml) appears. The dev bundle's layout differs from the
 *  source tree (hutch stages it inside the .app), so a fixed number of
 *  ".." hops is not stable. */
export function findRepoRoot(start: string): string | null {
  let dir = start;
  for (let i = 0; i < 10; i++) {
    if (existsSync(join(dir, 'pnpm-workspace.yaml'))) return dir;
    const parent = dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
  return null;
}

/** The user's login shell environment (Finder-launched GUIs get a minimal
 *  PATH; the daemon's own children — npx etc. — need the real one). Ported
 *  behavior of the Electron shell's resolveShellEnv, simplified: inherit
 *  the login shell's PATH additions instead of running a full env dump. */
function daemonEnv(): NodeJS.ProcessEnv {
  const path = process.env.PATH ?? '';
  const extra = ['/usr/local/bin', '/opt/homebrew/bin'].filter((p) => !path.includes(p));
  return { ...process.env, PATH: extra.length > 0 ? `${path}:${extra.join(':')}` : path };
}

/**
 * Ensure a healthy daemon is serving on `port`. Reuse order:
 * 1. live probe on the port (fastest, covers a daemon the state file missed)
 * 2. state file pid + healthy check
 * 3. spawn under the atomic lock, wait for health
 * Returns null when the daemon could not be started (caller decides fallback).
 */
export async function ensureDaemon(paths: DaemonPaths, config: DaemonConfig): Promise<StartedDaemon | null> {
  const origin = `http://127.0.0.1:${paths.port}`;

  // 1. live probe
  if (await isHealthy(paths.port)) {
    console.log(`[lite] reusing live daemon on :${paths.port} (probe)`);
    return { origin, spawnedByUs: false, reusedFrom: 'probe' };
  }

  // 2. state file (a shell may have spawned it very recently)
  const state = readState(paths);
  if (state && state.port === paths.port && await isHealthy(paths.port)) {
    console.log(`[lite] reusing daemon from state (pid=${state.pid}, owner=${state.owner})`);
    return { origin, spawnedByUs: false, reusedFrom: 'state' };
  }

  // 3. spawn under the atomic lock
  const bin = paths.binaries.find((p) => existsSync(p));
  if (!bin) {
    console.error('[lite] no mirri binary found in candidates:', paths.binaries);
    return null;
  }

  const release = acquireSpawnLock(paths);
  if (!release) {
    // Another shell won the lock and is spawning. Wait for its daemon instead.
    console.log('[lite] spawn lock held by another shell — waiting for its daemon');
    const ok = await waitForHealth(paths.port, HEALTH_TIMEOUT_MS);
    return ok ? { origin, spawnedByUs: false, reusedFrom: 'probe' } : null;
  }

  try {
    // Re-check under the lock: the winner of a just-finished race may be up now.
    if (await isHealthy(paths.port)) {
      console.log(`[lite] reusing daemon that won the spawn race on :${paths.port}`);
      return { origin, spawnedByUs: false, reusedFrom: 'probe' };
    }

    const args = ['web', '--no-open', '--port', String(paths.port), '--log-level', 'error'];
    const webDir = resolveWebDir(config);
    if (webDir) args.push('--web-dir', webDir);

    console.log(`[lite] spawning daemon: ${bin} ${args.join(' ')}`);
    const child = spawn(bin, args, {
      stdio: ['ignore', 'pipe', 'pipe'],
      detached: false,
      env: daemonEnv(),
    });
    child.stdout?.on('data', (d: Buffer) => process.stdout.write(`[daemon] ${String(d)}`));
    child.stderr?.on('data', (d: Buffer) => process.stderr.write(`[daemon!] ${String(d)}`));

    // Record the spawn BEFORE waiting for health (crash-window recovery):
    // a dead lite leaves a killable pid behind, same as the Electron shell.
    writeState(paths, {
      pid: child.pid ?? -1,
      port: paths.port,
      owner: DAEMON_OWNER,
      startedAt: new Date().toISOString(),
    });

    const ok = await waitForHealth(paths.port, HEALTH_TIMEOUT_MS);
    if (!ok) {
      console.error('[lite] spawned daemon did not become healthy in time');
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
      return null;
    }

    console.log(`[lite] spawned daemon pid=${child.pid} owner=${DAEMON_OWNER}`);
    return { origin, spawnedByUs: true };
  } finally {
    release();
  }
}

async function waitForHealth(port: number, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await isHealthy(port)) return true;
    await new Promise((r) => setTimeout(r, HEALTH_POLL_MS));
  }
  return false;
}

/**
 * R8 cleanup: kill ONLY the daemon this shell spawned (owner match) and only
 * while it is still serving on our port (PID-reuse guard, same shape as the
 * Electron shell's killStale). A daemon owned by the Electron shell — or by
 * anyone else — stays alive.
 */
export function cleanupOwnedDaemon(paths: DaemonPaths): void {
  const state = readState(paths);
  if (!state || state.owner !== DAEMON_OWNER || state.port !== paths.port) return;
  // Only reap when the pid is alive AND it is actually our daemon — killing a
  // reused pid that now runs something else would be worse than leaking.
  void isHealthy(paths.port).then((healthy) => {
    if (!healthy) return;
    try {
      process.kill(state.pid, 'SIGTERM');
      console.log(`[lite] SIGTERM to owned daemon pid=${state.pid}`);
    } catch (error) {
      console.log(`[lite] owned daemon pid=${state.pid} already gone (${String(error)})`);
    }
    try { rmSync(stateFile(paths)); } catch { /* best effort */ }
  });
}

/** Default daemon paths for the lite shell (macOS). */
export function defaultDaemonPaths(): DaemonPaths {
  // Own state dir — never shared with the Electron shell's
  // ~/.mirri-code/server/ (which has no owner concept and would reap a
  // foreign daemon).
  const stateDir = join(homedir(), '.mirri-code/server-lite');
  const envBin = process.env.MIRRICODE_LITE_DAEMON_BIN;
  return {
    binaries: [
      ...(envBin ? [envBin] : []),
      '/Applications/Mirri Code Desktop.app/Contents/Resources/bin/darwin-arm64/mirri',
    ],
    stateDir,
        port: 55_828,
  };
}
