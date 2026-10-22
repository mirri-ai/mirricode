import {
  existsSync,
  mkdirSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';

/** Filename of the Desktop state file under `~/.mirri-code/server/`. */
const DESKTOP_STATE_FILENAME = 'desktop-state.json';

/** Legacy lock file name for v1 server migration (under `~/.mirri-code/server/`). */
export const DESKTOP_LOCK_NAME = 'desktop';

/** Shape of the Desktop state file (written by this module, not by the server). */
export interface DesktopState {
  pid: number;
  port: number;
  startedAt: number;
}

/** Subset of the v1 server lock JSON (kept for migration compatibility). */
export interface LockContents {
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
export function desktopLockPath(): string {
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

export function readLock(path: string): LockContents | null {
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

/** Remove the legacy v1 lock file if it exists. Best-effort. */
export function cleanupLegacyLock(): void {
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
