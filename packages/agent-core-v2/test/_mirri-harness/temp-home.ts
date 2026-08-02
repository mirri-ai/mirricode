/**
 * Temp-home helper — creates an isolated home directory for each test run
 * and cleans it up on `cleanup()`.
 *
 * Uses `node:fs` directly (not v2's `IHostFileSystem`) because this is
 * test-infrastructure, not business code. The directory is created under
 * `os.tmpdir()` with a predictable prefix for easy debugging.
 */

import { mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

let counter = 0;

/**
 * Create a temporary home directory for an in-process test.
 * Returns the absolute path. Call `cleanup()` to remove it.
 */
export function createTempHome(prefix = 'mirri-v2-test'): TempHome {
  const id = `${prefix}-${process.pid}-${Date.now()}-${(counter++).toString(36)}`;
  const homeDir = join(tmpdir(), id);
  mkdirSync(homeDir, { recursive: true, mode: 0o700 });
  let cleaned = false;
  return {
    homeDir,
    cleanup: () => {
      if (cleaned) return;
      cleaned = true;
      rmSync(homeDir, { recursive: true, force: true });
    },
  };
}

export interface TempHome {
  /** Absolute path to the temporary home directory. */
  readonly homeDir: string;
  /** Remove the temporary directory. Safe to call multiple times. */
  cleanup(): void;
}
