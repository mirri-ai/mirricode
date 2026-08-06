/**
 * `workspaceAliases` domain — `IWorkspaceAliases` implementation.
 *
 * Resolves every id spelling of one physical directory by folding the
 * registered catalog (by `workspaceRootKey`) together with `workDir`
 * spellings recorded only in the legacy `session_index.jsonl`. The catalog
 * is reached through `IWorkspaceService.getSnapshot` — the raw (un-deduped)
 * catalog is required because `IWorkspaceService.list` collapses sibling
 * spellings to one representative, which would defeat the alias enumeration,
 * while `getSnapshot` serves every id from the service's own process-lifetime
 * cache. Read-only: no id or bucket is ever rewritten here. Bound at App scope.
 *
 * Read-path caching: `resolveAliasIds` runs once per sessions-list request;
 * it used to re-read `workspaces.json` (`IWorkspacePersistence.load`) and
 * `session_index.jsonl` from disk on every call — measured at ~200ms each of
 * async-fs latency under a busy event loop, i.e. ~500ms per list request
 * (phase-4 PROF). The catalog now comes from `IWorkspaceService`'s cache (no
 * disk), and the session-index view is cached for the process lifetime, so
 * steady-state resolutions are pure memory. Freshness: the session index is
 * append-only, grows only when a session is created or forked in-process —
 * `session/create|fork` routes invalidate the cached view explicitly — and
 * external writers (a v1 daemon sharing the HOME) are observed via the manual
 * `refresh` endpoint, which calls `clearCaches()` first.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { IWorkspaceService, type Workspace } from '#/app/workspace/workspace';
import {
  collectAliasIds,
  readSessionIndexEntries,
  type SessionIndexLine,
} from '#/app/workspace/workspaceAlias';
import { IFileSystemStorageService } from '#/persistence/interface/storage';

import { IWorkspaceAliases } from './workspaceAliases';

export class WorkspaceAliasesService implements IWorkspaceAliases {
  declare readonly _serviceBrand: undefined;

  private indexEntries: SessionIndexLine[] | undefined;
  private indexLoad: Promise<void> | undefined;

  constructor(
    @IWorkspaceService private readonly workspaces: IWorkspaceService,
    @IFileSystemStorageService private readonly storage: IFileSystemStorageService,
  ) {}

  async prewarm(): Promise<void> {
    await this.loadSessionIndexEntries();
  }

  clearCaches(): void {
    this.indexEntries = undefined;
  }

  async resolveAliasIds(id: string): Promise<readonly string[]> {
    const entry = await this.getWorkspaceEntry(id);
    if (entry === undefined) return [id];
    const { workspaces } = await this.workspaces.getSnapshot();
    const entries = await this.loadSessionIndexEntries();
    return collectAliasIds(workspaces, entries, entry.root);
  }

  /** Resolve one workspace by id from the service catalog; fall back on miss. */
  private async getWorkspaceEntry(id: string): Promise<Workspace | undefined> {
    const { byId } = await this.workspaces.getSnapshot();
    const fromCache = byId.get(id);
    if (fromCache !== undefined) return fromCache;
    // Catalog miss: fall through to the service, which triggers the
    // one-per-process session-index merge (ensureMerged) on first access and
    // folds workspaces the snapshot has not seen yet.
    return this.workspaces.get(id);
  }

  private async loadSessionIndexEntries(): Promise<readonly SessionIndexLine[]> {
    if (this.indexEntries !== undefined) return this.indexEntries;
    if (this.indexLoad !== undefined) {
      await this.indexLoad;
      return this.indexEntries!;
    }
    this.indexLoad = this.reloadSessionIndex().finally(() => {
      this.indexLoad = undefined;
    });
    await this.indexLoad;
    return this.indexEntries!;
  }

  private async reloadSessionIndex(): Promise<void> {
    this.indexEntries = await readSessionIndexEntries(this.storage);
  }
}

registerScopedService(
  LifecycleScope.App,
  IWorkspaceAliases,
  WorkspaceAliasesService,
  ScopeActivation.OnScopeCreated,
  'workspaceAliases',
);