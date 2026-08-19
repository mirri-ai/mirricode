import type { Page } from '#/persistence/interface/queryStore';
import {
  ISessionIndex,
  type SessionListQuery,
  type SessionSummary,
} from '#/app/sessionIndex/sessionIndex';
import { ISqliteSessionStore, SqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';
import { ILogService } from '#/_base/log/log';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { IQueryStore } from '#/persistence/interface/queryStore';
import { IFlagService } from '#/app/flag/flag';
import { FileSessionIndex } from '#/app/sessionIndex/sessionIndexService';
import { createDiskReconcileSource } from '#/app/sessionIndex/sqliteDiskSource';

/**
 * Source of truth for disk-resident sessions, used by `reconcile()` to
 * back-fill missing entries and drop orphaned ones.
 */
export interface ReconcileSource {
  listWorkspaceIds(): Promise<readonly string[]>;
  listSessionIds(workspaceId: string): Promise<readonly string[]>;
  readSummary(workspaceId: string, sessionId: string): Promise<SessionSummary | undefined>;
  /** Return the mtimeMs of the session's state.json, or undefined if not found. */
  statStateMtime(workspaceId: string, sessionId: string): Promise<number | undefined>;
  /**
   * Return the mtimeMs of the workspace directory itself, or undefined when
   * the workspace does not exist. Currently unused by the index (reads are
   * pure SQLite after the authoritative-read change; convergence re-scans the
   * scoped workspace) — kept on the source for parity with the legacy index
   * and for tests that count disk traffic. Optional so in-memory test
   * sources can omit it.
   */
  statWorkspaceMtime?(workspaceId: string): Promise<number | undefined>;
}

/**
 * `SqliteSessionIndex` is an `ISessionIndex` implementation backed by
 * `SqliteSessionStore`. It is a read-model facade: `list`/`get` mirror the
 * store's filters and ordering, and `countActive` counts non-archived sessions
 * across the requested workspaces. Pagination (cursor/limit) is handled by the
 * routing layer, so `list` returns the filtered, ordered set without slicing.
 *
 * The store is opened lazily on first use. If opening the SQLite database
 * fails (e.g. permissions, corrupt file), the index permanently falls back
 * to a `FileSessionIndex` instance — delegating `list`/`get`/`countActive`/
 * `reconcile` to it instead of returning empty results. The fallback is
 * one-way: once SQLite open fails within a process, it never retries (the
 * underlying cause is typically not recoverable without a restart).
 */
export class SqliteSessionIndex implements ISessionIndex {
  declare readonly _serviceBrand: undefined;
  readonly store: SqliteSessionStore;

  private _fallback = false;
  private _openPromise: Promise<void> | undefined;
  private readonly legacy: FileSessionIndex;
  private disk: ReconcileSource;

  constructor(
    @ISqliteSessionStore store: SqliteSessionStore,
    @ILogService private readonly log: ILogService,
    @IBootstrapService bootstrap: IBootstrapService,
    @IFileSystemStorageService storage: IFileSystemStorageService,
    @IAtomicDocumentStore docs: IAtomicDocumentStore,
    @IQueryStore queryStore: IQueryStore,
    @IFlagService flags: IFlagService,
  ) {
    this.store = store;
    this.legacy = new FileSessionIndex(bootstrap, storage, docs, queryStore, flags, log);
    // Disk source used to converge the store: the startup `reconcile()` and
    // the manual `refreshWorkspace()` read through it. Reads never touch it.
    this.disk = createDiskReconcileSource(bootstrap, storage, docs);
  }

  /** Ensure the underlying store is open. Falls back on failure. */
  private ensureOpen(): Promise<void> {
    if (this._fallback) return Promise.resolve();
    if (!this._openPromise) {
      this._openPromise = this.tryOpen();
    }
    return this._openPromise;
  }

  private async tryOpen(): Promise<void> {
    try {
      this.store.open();
    } catch (error) {
      this._fallback = true;
      this._openPromise = undefined;
      this.log.warn('SqliteSessionIndex: failed to open store, falling back to FileSessionIndex', {
        error: String(error),
      });
    }
  }

  /**
   * Authoritative-read model: `list`/`countActive`/`get` serve the SQLite
   * store **without any disk probing**. Disk is the change source, not a
   * per-read check: in-process writes reach the store through the session
   * metadata write-through (`SessionMetadataService`), the startup
   * `reconcile()` converges legacy on-disk sessions once, and the manual
   * `refreshWorkspace()` re-converges a workspace on demand (the sidebar
   * refresh button). An external v1 process writing to the same HOME while
   * the server runs therefore only becomes visible after a manual refresh —
   * a deliberate trade-off that keeps reads pure SQLite.
   *
   * Convergence (both `reconcile` and `refreshWorkspace`) uses the
   * stateMtime of each session's `state.json` to skip unchanged sessions
   * (0-change fast path). Concurrent `refreshWorkspace` calls coalesce into
   * one in-flight convergence instead of re-scanning the same directories.
   */

  /**
   * Test-only seam: swap the disk source used by `reconcile`/`refreshWorkspace`
   * so tests can count enumeration calls. Production never calls this.
   */
  setDiskSourceForTests(disk: ReconcileSource): void {
    this.disk = disk;
  }

  /**
   * Re-converge the named workspaces from disk into the SQLite store: upsert
   * new/changed sessions and delete store rows whose session no longer exists
   * on disk. Unlike the old read-path sync this is the *only* place the disk
   * becomes visible again — it is what the sidebar refresh button triggers.
   * No-op in fallback mode.
   *
   * Concurrent calls coalesce: a workspace already covered by the in-flight
   * convergence is absorbed (its result shared), and only the not-yet-covered
   * remainder is converged once the in-flight run settles.
   */
  private refreshInFlight:
    | { ids: readonly string[]; promise: Promise<{ inserted: number; removed: number }> }
    | undefined;

  async refreshWorkspace(workspaceIds: readonly string[]): Promise<{ inserted: number; removed: number }> {
    await this.ensureOpen();
    if (this._fallback || workspaceIds.length === 0) return { inserted: 0, removed: 0 };

    const inFlight = this.refreshInFlight;
    if (inFlight !== undefined) {
      const uncovered = workspaceIds.filter((id) => !inFlight.ids.includes(id));
      return inFlight.promise
        .catch(() => ({ inserted: 0, removed: 0 }))
        .then(async (first) => {
          if (uncovered.length === 0) return first;
          const rest = await this.convergeWorkspaces(uncovered);
          return {
            inserted: first.inserted + rest.inserted,
            removed: first.removed + rest.removed,
          };
        });
    }

    const promise = this.convergeWorkspaces(workspaceIds);
    const wrapped = promise.finally(() => {
      if (this.refreshInFlight?.promise === wrapped) this.refreshInFlight = undefined;
    });
    this.refreshInFlight = { ids: workspaceIds, promise: wrapped };
    return wrapped;
  }

  private async convergeWorkspaces(workspaceIds: readonly string[]): Promise<{ inserted: number; removed: number }> {
    const source = this.disk;
    let inserted = 0;
    let removed = 0;
    for (const ws of workspaceIds) {
      const result = await this.convergeWorkspace(source, ws);
      inserted += result.inserted;
      removed += result.removed;
    }
    return { inserted, removed };
  }

  /** Converge a single workspace: insert/update by stateMtime, delete orphans. */
  private async convergeWorkspace(
    source: ReconcileSource,
    workspaceId: string,
  ): Promise<{ inserted: number; removed: number }> {
    const diskIds = await source.listSessionIds(workspaceId);
    const diskMtimeById = new Map<string, number | undefined>();
    for (const sid of diskIds) {
      diskMtimeById.set(sid, await source.statStateMtime(workspaceId, sid));
    }
    const storeRows = this.store.listRowsByWorkspace(workspaceId);
    const storeRowById = new Map(storeRows.map((row) => [row.id, row]));

    let inserted = 0;
    for (const sid of diskIds) {
      const storeRow = storeRowById.get(sid);
      const diskMtime = diskMtimeById.get(sid);
      // Skip only when BOTH sides have a concrete mtime AND they are equal.
      // If either side is undefined we cannot be certain the session hasn't
      // changed — better to re-read and converge (matches reconcile).
      const unchanged =
        storeRow?.stateMtime !== undefined &&
        diskMtime !== undefined &&
        storeRow.stateMtime === diskMtime;
      if (unchanged) continue;
      const sum = await source.readSummary(workspaceId, sid);
      if (sum === undefined) continue; // missing/corrupt state: leave as-is
      this.store.upsert(sum);
      if (diskMtime !== undefined) this.store.setStateMtime(sid, diskMtime);
      inserted++;
    }

    let removed = 0;
    for (const row of storeRows) {
      if (diskMtimeById.has(row.id)) continue;
      this.store.delete(row.id);
      removed++;
    }
    return { inserted, removed };
  }

  async list(query: SessionListQuery): Promise<Page<SessionSummary>> {
    await this.ensureOpen();
    if (this._fallback) return this.legacy.list(query);
    // Authoritative read: pure SQLite, no disk probing. The store is kept
    // current by the metadata write-through, the startup reconcile, and the
    // manual `refreshWorkspace`.
    return {
      items: this.store.list({
        workspaceIds: query.workspaceIds,
        includeArchived: query.includeArchived,
        childOf: query.childOf,
      }),
    };
  }

  async get(id: string): Promise<SessionSummary | undefined> {
    await this.ensureOpen();
    if (this._fallback) return this.legacy.get(id);
    // No disk sync on this path: a session created through the API is written
    // through to the store already, and the `legacy.get` fallback below does a
    // cheap single-session disk read (no directory sweep) for a session that
    // exists on disk but is missing from the index (e.g. v1 wrote it).
    const row = this.store.get(id);
    if (row !== undefined) return row;
    return this.legacy.get(id);
  }

  async countActive(workspaceIds: readonly string[]): Promise<number> {
    await this.ensureOpen();
    if (this._fallback) return this.legacy.countActive(workspaceIds);
    // Authoritative read: pure SQLite count (see `list`).
    return this.store.count(workspaceIds);
  }

  /**
   * Reconcile the SQLite index against the on-disk source of truth.
   *
   * **Incremental** (by stateMtime): only sessions that are new, changed,
   * or removed from disk incur a read or write. Unchanged sessions (same
   * stateMtime) are skipped entirely — the 0-change fast path.
   *
   * - Sessions present on disk but missing from the store are backfilled
   *   (counted as `inserted`).
   * - Sessions present in the store but absent from disk are deleted
   *   (counted as `removed`).
   * - Sessions present in both with a changed stateMtime are re-read and
   *   updated (counted as `inserted` for backward compatibility).
   * - Sessions present in both with the **same** stateMtime are skipped
   *   (no read, no write).
   *
   * When in fallback mode (SQLite unavailable), delegates to the legacy
   * `FileSessionIndex` which reads from disk directly — no attempt is
   * made to re-open SQLite.
   */
  async reconcile(source: ReconcileSource): Promise<{ inserted: number; removed: number }> {
    await this.ensureOpen();
    if (this._fallback) {
      // Delegate to legacy: FileSessionIndex enumerates disk directly,
      // so a full reconcile is a no-op against itself. Return zeroes to
      // indicate nothing was backfilled or removed through SQLite.
      return { inserted: 0, removed: 0 };
    }

    let inserted = 0;
    let removed = 0;

    // 1. Enumerate all disk sessions → diskKeys set
    const diskKeys = new Set<string>();
    const diskEntries: { ws: string; sid: string }[] = [];
    for (const ws of await source.listWorkspaceIds()) {
      for (const sid of await source.listSessionIds(ws)) {
        const key = `${ws}\u0000${sid}`;
        diskKeys.add(key);
        diskEntries.push({ ws, sid });
      }
    }

    // 2. Get all store rows (with stateMtime) → storeMap
    const storeRows = this.store.listAllRows();
    const storeMap = new Map<string, { id: string; workspaceId: string; stateMtime: number | undefined }>();
    for (const row of storeRows) {
      storeMap.set(row.id, row);
    }

    // 3. Process disk entries: new sessions + changed sessions.
    // `touchedIds` collects every session that step 3 upserted (including ones
    // whose `workspaceId` moved). The orphan pass below must skip them, because
    // their snapshot row still carries the *old* workspace key, which would
    // otherwise make the pass delete the row we just wrote.
    const touchedIds = new Set<string>();
    for (const { ws, sid } of diskEntries) {
      const storeRow = storeMap.get(sid);

      const diskMtime = await source.statStateMtime(ws, sid);

      if (storeRow === undefined) {
        // New session on disk (not in store)
        const sum = await source.readSummary(ws, sid);
        if (sum !== undefined) {
          this.store.upsert(sum);
          touchedIds.add(sid);
          if (diskMtime !== undefined) {
            this.store.setStateMtime(sid, diskMtime);
          }
          inserted++;
        }
        // If readSummary returns undefined, skip (can't insert without data)
      } else if (storeRow.workspaceId !== ws) {
        // Same session id now lives under a different workspaceId. The row key
        // is just `id`, so upserting carries it to the new workspace — no
        // delete: deleting here would race with the orphan pass (which would
        // delete the freshly written row again).
        const sum = await source.readSummary(ws, sid);
        if (sum !== undefined) {
          this.store.upsert(sum);
          touchedIds.add(sid);
          if (diskMtime !== undefined) {
            this.store.setStateMtime(sid, diskMtime);
          }
          inserted++;
        }
      } else {
        // Same session in both store and disk — check mtime
        // Only skip (0-change fast path) when BOTH sides have a concrete
        // mtime AND they are equal.  If either side is undefined we cannot
        // be certain the session hasn't changed — better to re-read and
        // converge than to leave the entry in a permanently unresolved
        // state (I-1: double-undefined used to compare equal → never
        // converged).
        const bothDefinedAndEqual =
          storeRow.stateMtime !== undefined &&
          diskMtime !== undefined &&
          storeRow.stateMtime === diskMtime;

        if (!bothDefinedAndEqual) {
          // Mtime changed, or at least one side is undefined — re-read
          const sum = await source.readSummary(ws, sid);
          if (sum !== undefined) {
            this.store.upsert(sum);
            touchedIds.add(sid);
            if (diskMtime !== undefined) {
              this.store.setStateMtime(sid, diskMtime);
            }
            inserted++;
          }
          // If readSummary returns undefined (e.g. state.json missing on
          // disk), leave the session as-is — don't upsert and don't
          // delete.  The session may simply have a temporarily absent
          // state file.
        }
        // else: both mtimes defined and equal — skip (0-change fast path)
      }
    }

    // 4. Remove orphans: sessions in the store that are absent from disk.
    for (const row of storeRows) {
      if (touchedIds.has(row.id)) continue;
      const key = `${row.workspaceId}\u0000${row.id}`;
      if (!diskKeys.has(key)) {
        this.store.delete(row.id);
        removed++;
      }
    }

    return { inserted, removed };
  }
}
