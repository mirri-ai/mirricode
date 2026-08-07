/**
 * `workspace` domain — `IWorkspaceService` implementation.
 *
 * Process-wide catalog of known workspaces, durable in
 * `<homeDir>/workspaces.json` (the v1-compatible file). The service keeps NO
 * in-memory write cache: every operation is a fresh read-modify-write
 * against the file, serialized through a promise-chain mutex. This is
 * required, not just tidy — the same file is written concurrently by other
 * processes, so a write-through cache would clobber external additions and
 * tombstones with stale state. Atomic renames at the persistence layer plus fresh
 * read-modify-write on both engines shrink the lost-update window to a
 * single read-modify-write, and the next session-index merge heals anything
 * still lost there.
 *
 * Once per process, the first operation triggers the startup sync with the
 * legacy `<homeDir>/session_index.jsonl`:
 *
 * 1. No usable catalog file → one-shot rebuild (one workspace per distinct
 *    absolute `workDir`), persisted.
 * 2. Catalog loaded → only workDirs the file does not know about yet are
 *    added (e.g. sessions created by the v1 TUI since the last sync),
 *    persisted if anything changed.
 *
 * Deletion is soft: `delete` drops the entry but records the id in
 * `deleted_workspace_ids`, and the merge never resurrects a tombstoned id.
 * An explicit `createOrTouch` clears the tombstone — the user opening the
 * folder again is a stronger signal than the historical index.
 *
 * `createOrTouch` is the single choke point every workspace/session creation
 * funnels through, so it owns the root-existence contract: the root must be
 * an existing directory on the host filesystem, otherwise it throws
 * `fs.path_not_found`. The directory probe follows symlinks
 * (`IHostFileSystem.stat` is lstat-based, so a symlink-form root is
 * re-checked through `realpath`), while the workspace identity stays lexical.
 * The rebuild and merge paths bypass the check on purpose — they catalog
 * where sessions *were*, not where new ones may open. Bound at App scope.
 *
 * One physical folder can arrive under several spellings — most visibly on
 * Windows, where drive-letter casing, slash direction, and typed-vs-realpath
 * casing all differ for one directory. Every "same directory?" judgment
 * (`createOrTouch` reuse, the session-index rebuild, and the `list` merge in
 * `dedupeByRoot`) therefore goes through the `workspaceRootKey` identity key
 * rather than the raw root string, while the minted `workspaceId` stays the
 * case-sensitive `encodeWorkDirKey` so already-persisted session buckets,
 * `workspaces.json` entries, and session metadata keep resolving with zero
 * data migration.
 *
 * Legacy data may still be split: two registry entries (or a registry entry
 * plus session-index-only spellings) for one physical folder, with sessions
 * bucketed per id. `delete` folds the same alias set inside the op mutex so a
 * sibling spelling cannot resurface as this directory's representative on the
 * next `list()`.
 */

import { basename, isAbsolute } from 'pathe';

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { encodeWorkDirKey, workspaceRootKey } from '#/_base/utils/workdir-slug';
import { ISessionIndex } from '#/app/sessionIndex/sessionIndex';
import { ErrorCodes, Error2, unwrapErrorCause } from '#/errors';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IFileSystemStorageService } from '#/persistence/interface/storage';

import { IWorkspaceService, type Workspace, type WorkspaceUpdate } from './workspace';
import {
  collectAliasIds,
  dedupeByRoot,
  readSessionIndexEntries,
  readSessionIndexWorkDirs,
} from './workspaceAlias';
import { IWorkspacePersistence, type WorkspaceCatalog } from './workspacePersistence';

import type { WorkspaceWithSessionCount } from './workspace';

export class WorkspaceService implements IWorkspaceService {
  declare readonly _serviceBrand: undefined;

  private merged = false;
  private opQueue: Promise<unknown> = Promise.resolve();
  private listCache:
    | { byId: Map<string, Workspace>; workspaces: readonly Workspace[] }
    | undefined;

  /**
   * The parsed catalog cache lives for the process lifetime: `workspaces.json`
   * only changes through this service's own write methods (`createOrTouch` /
   * `update` / `delete`), each of which invalidates the cache explicitly, so
   * reads never need a freshness re-check. An *external* writer (a v1 daemon
   * sharing the HOME) is not observed — the manual `refresh` endpoint is the
   * escape hatch that re-reads the catalog on demand, keeping the read side
   * free of per-request disk I/O (the cold-start sessions-list latency fix).
   */

  constructor(
    @IWorkspacePersistence private readonly store: IWorkspacePersistence,
    @IFileSystemStorageService private readonly storage: IFileSystemStorageService,
    @IHostFileSystem private readonly hostFs: IHostFileSystem,
    @ISessionIndex private readonly sessionIndex: ISessionIndex,
  ) {}

  private invalidateListCache(): void {
    this.listCache = undefined;
  }

  invalidateCache(): void {
    this.invalidateListCache();
  }

  private async loadListSnapshot(): Promise<{ deduped: readonly Workspace[]; byId: Map<string, Workspace> }> {
    if (this.listCache !== undefined) {
      return { deduped: this.listCache.workspaces, byId: this.listCache.byId };
    }
    const catalog = await this.loadCatalog();
    const byId = new Map(catalog.workspaces.map((ws) => [ws.id, ws]));
    const deduped = dedupeByRoot(byId);
    this.listCache = { workspaces: deduped, byId };
    return { deduped, byId };
  }

  async getSnapshot(): Promise<{
    readonly byId: ReadonlyMap<string, Workspace>;
    readonly workspaces: readonly Workspace[];
  }> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const { byId } = await this.loadListSnapshot();
      return { byId, workspaces: [...byId.values()] };
    });
  }

  list(): Promise<readonly Workspace[]> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const { deduped } = await this.loadListSnapshot();
      return deduped;
    });
  }

  listWithSessionCounts(): Promise<readonly WorkspaceWithSessionCount[]> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const catalog = await this.loadCatalog();
      const byId = new Map(catalog.workspaces.map((ws) => [ws.id, ws]));
      const deduped = dedupeByRoot(byId);

      // Build a root → alias-id set map from session_index.jsonl (read once).
      // Each workspace's representative id may not be the only bucket — legacy
      // split spellings create sibling directories under sessions/. We need
      // every alias id so we can readdir each bucket and sum the counts.
      const indexEntries = await readSessionIndexEntries(this.storage);
      const aliasIdsByRootKey = new Map<string, Set<string>>();
      for (const entry of indexEntries) {
        if (!isAbsolute(entry.workDir)) continue;
        const rootKey = workspaceRootKey(entry.workDir);
        let ids = aliasIdsByRootKey.get(rootKey);
        if (ids === undefined) {
          ids = new Set<string>();
          aliasIdsByRootKey.set(rootKey, ids);
        }
        ids.add(encodeWorkDirKey(entry.workDir));
      }
      // Also fold catalog entries that share a root key (registered aliases).
      for (const ws of catalog.workspaces) {
        const rootKey = workspaceRootKey(ws.root);
        let ids = aliasIdsByRootKey.get(rootKey);
        if (ids === undefined) {
          ids = new Set<string>();
          aliasIdsByRootKey.set(rootKey, ids);
        }
        ids.add(ws.id);
      }

      // Count non-archived sessions per workspace through the session index
      // (`ISessionIndex.countActive`, backed by the SQLite read model, with a
      // disk fallback). The alias set above is folded first so legacy split
      // buckets count once for the workspace. This replaces the old readdir +
      // directory-listing count, which included archived sessions and made the
      // sidebar's "load more N" number overshoot the pages actually remaining.
      const counts = await Promise.all(
        deduped.map(async (ws) => {
          const aliasIds = aliasIdsByRootKey.get(workspaceRootKey(ws.root));
          if (aliasIds === undefined || aliasIds.size === 0) return 0;
          return this.sessionIndex.countActive([...aliasIds]);
        }),
      );

      return deduped.map((ws, i) => ({
        ...ws,
        sessionCount: counts[i] ?? 0,
      }));
    });
  }

  get(id: string): Promise<Workspace | undefined> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const { byId } = await this.loadListSnapshot();
      return byId.get(id);
    });
  }

  createOrTouch(root: string, name?: string): Promise<Workspace> {
    return this.runExclusive(async () => {
      let stat;
      try {
        stat = await this.hostFs.stat(root);
      } catch (error) {
        const code = (unwrapErrorCause(error) as NodeJS.ErrnoException | undefined)?.code;
        if (code === 'ENOENT' || code === 'ENOTDIR') {
          throw new Error2(ErrorCodes.FS_PATH_NOT_FOUND, `workspace root ${root} does not exist`);
        }
        throw error;
      }
      if (!stat.isDirectory) {
        try {
          stat = await this.hostFs.stat(await this.hostFs.realpath(root));
        } catch {
        }
      }
      if (!stat.isDirectory) {
        throw new Error2(ErrorCodes.FS_PATH_NOT_FOUND, `workspace root ${root} is not a directory`);
      }
      await this.ensureMerged();
      const catalog = await this.loadCatalog();
      const byId = new Map(catalog.workspaces.map((ws) => [ws.id, ws]));
      const deletedIds = new Set(catalog.deletedIds);
      const id = encodeWorkDirKey(root);
      let existing = byId.get(id);
      if (existing === undefined) {
        const rootKey = workspaceRootKey(root);
        for (const entry of byId.values()) {
          if (workspaceRootKey(entry.root) === rootKey) {
            existing = entry;
            break;
          }
        }
      }
      const now = Date.now();
      const ws: Workspace =
        existing !== undefined
          ? { ...existing, lastOpenedAt: now }
          : {
              id,
              root,
              name: name ?? basename(root),
              createdAt: now,
              lastOpenedAt: now,
            };
      byId.set(ws.id, ws);
      deletedIds.delete(ws.id);
      await this.store.save({ workspaces: [...byId.values()], deletedIds: [...deletedIds] });
      this.invalidateListCache();
      return ws;
    });
  }

  update(id: string, patch: WorkspaceUpdate): Promise<Workspace | undefined> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const catalog = await this.loadCatalog();
      const existing = catalog.workspaces.find((ws) => ws.id === id);
      if (existing === undefined) return undefined;
      const updated: Workspace = {
        ...existing,
        ...(patch.name !== undefined ? { name: patch.name } : {}),
      };
      await this.store.save({
        workspaces: catalog.workspaces.map((ws) => (ws.id === id ? updated : ws)),
        deletedIds: catalog.deletedIds,
      });
      this.invalidateListCache();
      return updated;
    });
  }

  delete(id: string): Promise<void> {
    return this.runExclusive(async () => {
      await this.ensureMerged();
      const catalog = await this.loadCatalog();
      let root = catalog.workspaces.find((ws) => ws.id === id)?.root;
      if (root === undefined) {
        root = (await readSessionIndexEntries(this.storage)).find(
          (line) => encodeWorkDirKey(line.workDir) === id,
        )?.workDir;
      }
      if (root === undefined) {
        await this.store.save({
          workspaces: catalog.workspaces.filter((ws) => ws.id !== id),
          deletedIds: [...new Set([...catalog.deletedIds, id])],
        });
        this.invalidateListCache();
        return;
      }
      const rootKey = workspaceRootKey(root);
      const aliasIds = collectAliasIds(
        catalog.workspaces,
        await readSessionIndexEntries(this.storage),
        root,
      );
      await this.store.save({
        workspaces: catalog.workspaces.filter((ws) => workspaceRootKey(ws.root) !== rootKey),
        deletedIds: [...new Set([...catalog.deletedIds, ...aliasIds])],
      });
      this.invalidateListCache();
    });
  }

  private async ensureMerged(): Promise<void> {
    if (this.merged) return;
    const loaded = await this.store.load();
    if (loaded === undefined) {
      const rebuilt = await this.rebuildFromSessionIndex();
      await this.store.save({ workspaces: [...rebuilt.values()], deletedIds: [] });
      this.merged = true;
      return;
    }
    const byId = new Map(loaded.workspaces.map((ws) => [ws.id, ws]));
    const deletedIds = new Set(loaded.deletedIds);
    if (await this.mergeFromSessionIndex(byId, deletedIds)) {
      await this.store.save({ workspaces: [...byId.values()], deletedIds: [...deletedIds] });
    }
    this.merged = true;
  }

  private async loadCatalog(): Promise<WorkspaceCatalog> {
    return (await this.store.load()) ?? { workspaces: [], deletedIds: [] };
  }

  private async mergeFromSessionIndex(
    byId: Map<string, Workspace>,
    deletedIds: ReadonlySet<string>,
  ): Promise<boolean> {
    let changed = false;
    const now = Date.now();
    for (const workDir of await readSessionIndexWorkDirs(this.storage)) {
      const id = encodeWorkDirKey(workDir);
      if (byId.has(id) || deletedIds.has(id)) continue;
      byId.set(id, {
        id,
        root: workDir,
        name: basename(workDir),
        createdAt: now,
        lastOpenedAt: now,
      });
      changed = true;
    }
    return changed;
  }

  private async rebuildFromSessionIndex(): Promise<Map<string, Workspace>> {
    const result = new Map<string, Workspace>();
    const now = Date.now();
    const seenRootKeys = new Set<string>();
    for (const entry of await readSessionIndexEntries(this.storage)) {
      if (!isAbsolute(entry.workDir)) continue;
      const rootKey = workspaceRootKey(entry.workDir);
      if (seenRootKeys.has(rootKey)) continue;
      seenRootKeys.add(rootKey);
      const id = encodeWorkDirKey(entry.workDir);
      result.set(id, {
        id,
        root: entry.workDir,
        name: basename(entry.workDir),
        createdAt: now,
        lastOpenedAt: now,
      });
    }
    return result;
  }

  private runExclusive<T>(op: () => Promise<T>): Promise<T> {
    const next = this.opQueue.then(op, op);
    this.opQueue = next.then(
      () => {},
      () => {},
    );
    return next;
  }
}

registerScopedService(
  LifecycleScope.App,
  IWorkspaceService,
  WorkspaceService,
  ScopeActivation.OnScopeCreated,
  'workspace',
);
