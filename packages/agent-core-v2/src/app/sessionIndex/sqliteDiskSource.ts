/**
 * Disk-backed `ReconcileSource` — enumerates on-disk sessions through the
 * `IFileSystemStorageService` / `IAtomicDocumentStore` access-pattern stores
 * and produces `SessionSummary` values for `SqliteSessionIndex.reconcile()`.
 *
 * The logic mirrors `FileSessionIndex`'s private `listWorkspaceIds`,
 * `listSessionIds`, `readSummary`, and `readMeta` methods so the disk source
 * sees exactly the same data the legacy index would.
 *
 * Enumeration and mtime probing deliberately use **synchronous** fs calls
 * (readdirSync / statSync) on the absolute `bootstrap.sessionsDir` instead of
 * `node:fs/promises`. At startup the libuv worker pool is shared with other
 * bootstrapping workloads; queueing a per-call op there added ~60ms per
 * session — 202 sessions turned a warm 0-change reconcile into a 15s stall
 * even though every session was skipped. Sync fs sidesteps the pool entirely:
 * a full 200-session enumeration + mtime sweep costs a few milliseconds of
 * main-thread time. That is acceptable at startup and on the index read
 * path (`syncFromDisk`), where the same sweep is amortized per request and
 * stays a few milliseconds below the legacy file-index read of every
 * state.json; those short, bounded blocks only happen while the session
 * index is being read.
 */

import { readdirSync, readFileSync, statSync } from 'node:fs';
import type { Dirent } from 'node:fs';
import { join } from 'pathe';
import type { IBootstrapService } from '#/app/bootstrap/bootstrap';
import type { IFileSystemStorageService } from '#/persistence/interface/storage';
import type { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import type { SessionSummary } from '#/app/sessionIndex/sessionIndex';
import type { ReconcileSource } from '#/app/sessionIndex/sqliteSessionIndex';

const META_SCOPE = 'session-meta';
const META_KEY = 'state.json';

function parseTime(value: unknown): number {
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string') {
    const parsed = Date.parse(value);
    if (!Number.isNaN(parsed)) return parsed;
  }
  return 0;
}

function recoverCwd(meta: Record<string, unknown>): string | undefined {
  if (typeof meta['cwd'] === 'string' && meta['cwd'].length > 0) return meta['cwd'];
  if (typeof meta['workDir'] === 'string' && meta['workDir'].length > 0) {
    return meta['workDir'];
  }
  const custom = meta['custom'];
  if (custom !== null && typeof custom === 'object' && !Array.isArray(custom)) {
    const fromCustom = (custom as Record<string, unknown>)['cwd'];
    if (typeof fromCustom === 'string' && fromCustom.length > 0) return fromCustom;
  }
  return undefined;
}

/** True when the entry is a directory, or a symlink resolving to one. */
function tracesToDirectory(dir: string, entry: Dirent): boolean {
  if (entry.isDirectory()) return true;
  if (!entry.isSymbolicLink()) return false;
  try {
    return statSync(join(dir, entry.name)).isDirectory();
  } catch {
    return false;
  }
}

/** List immediate subdirectories (following directory symlinks) of `dir` synchronously; [] when absent/unreadable. */
function listSyncDirs(dir: string): readonly string[] {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return [];
  }
  const out: string[] = [];
  for (const entry of entries) {
    if (tracesToDirectory(dir, entry)) out.push(entry.name);
  }
  return out;
}

/**
 * Stat a session's state file, returning the integer-ms mtime of the unified
 * `<sid>/state.json`, falling back to the legacy v1 `<sid>/session-meta/state.json`
 * layout. `undefined` when neither file exists.
 *
 * Rounds to integer ms because the SQLite `stateMtime` column is INTEGER: a
 * float `mtimeMs` would be truncated on write and never compare equal on the
 * next reconcile (breaking the 0-change fast path).
 */
function statSessionStateMtime(sessionDir: string): number | undefined {
  try {
    return Math.round(statSync(join(sessionDir, 'state.json')).mtimeMs);
  } catch {
    try {
      return Math.round(statSync(join(sessionDir, META_SCOPE, 'state.json')).mtimeMs);
    } catch {
      return undefined;
    }
  }
}

/**
 * Create a `ReconcileSource` that reads session metadata from the on-disk
 * storage layer. Uses the same enumeration and parsing logic as
 * `FileSessionIndex` so both see an identical view of disk-resident sessions.
 */
export function createDiskReconcileSource(
  bootstrap: IBootstrapService,
  _storage: IFileSystemStorageService,
  _docs: IAtomicDocumentStore,
): ReconcileSource {
  const sessionsDir = bootstrap.sessionsDir;

  return {
    listWorkspaceIds(): Promise<readonly string[]> {
      return Promise.resolve(listSyncDirs(sessionsDir));
    },

    listSessionIds(workspaceId: string): Promise<readonly string[]> {
      return Promise.resolve(listSyncDirs(join(sessionsDir, workspaceId)));
    },

    async readSummary(
      workspaceId: string,
      sessionId: string,
    ): Promise<SessionSummary | undefined> {
      const meta = await readMeta(join(sessionsDir, workspaceId, sessionId));
      if (meta === undefined) return undefined;

      const rawCustom = meta['custom'];
      const custom =
        rawCustom !== null && typeof rawCustom === 'object' && !Array.isArray(rawCustom)
          ? (rawCustom as Record<string, unknown>)
          : undefined;

      return {
        id: sessionId,
        workspaceId,
        cwd: recoverCwd(meta),
        title: typeof meta['title'] === 'string' ? meta['title'] : undefined,
        lastPrompt:
          typeof meta['lastPrompt'] === 'string' ? meta['lastPrompt'] : undefined,
        createdAt: parseTime(meta['createdAt']),
        updatedAt: parseTime(meta['updatedAt']),
        archived: meta['archived'] === true,
        custom,
      };
    },

    statStateMtime(workspaceId: string, sessionId: string): Promise<number | undefined> {
      return Promise.resolve(statSessionStateMtime(join(sessionsDir, workspaceId, sessionId)));
    },

    /**
     * Return the workspace directory's own mtimeMs. `undefined` when the
     * directory is absent (or unreadable). `syncFromDisk` caches this and
     * skips the sub-directory enumeration when it is unchanged: creating or
     * removing a session directory updates the parent directory's mtime.
     */
    async statWorkspaceMtime(workspaceId: string): Promise<number | undefined> {
      try {
        return statSync(join(sessionsDir, workspaceId)).mtimeMs;
      } catch {
        return undefined;
      }
    },
  };
}

async function readMeta(sessionDir: string): Promise<Record<string, unknown> | undefined> {
  // Read the unified `<sid>/state.json` first, then the legacy v1 layout
  // `<sid>/session-meta/state.json`. The store-scoped access-pattern reads
  // (`docs.get`) are deliberately reimplemented as plain file reads here so
  // the summary comes from the same bytes statStateMtime probed.
  const unified = readJsonIfExists(join(sessionDir, 'state.json'));
  if (unified !== undefined) return unified;
  return readJsonIfExists(join(sessionDir, META_SCOPE, 'state.json'));
}

function readJsonIfExists(filePath: string): Record<string, unknown> | undefined {
  let raw: string;
  try {
    raw = readFileSync(filePath, 'utf-8');
  } catch {
    return undefined;
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    // Only a JSON *object* is a valid session meta. A top-level array/scalar
    // (or an unparseable file) is treated as absent: the old `docs.get` path
    // returned whatever it had and built an all-empty summary from it, which
    // only ever happened for hand-corrupted files — conservative here keeps
    // garbage out of the SQLite index.
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return parsed as Record<string, unknown>;
    }
  } catch {
    // Unparseable state file — treat as absent (caller decides how to react).
  }
  return undefined;
}
