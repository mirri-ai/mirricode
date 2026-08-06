import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { promises as fsp } from 'node:fs';
import os from 'node:os';
import { join } from 'node:path';

import {
  LifecycleScope,
  ScopeActivation,
  _clearScopedRegistryForTests,
  registerScopedService,
} from '#/_base/di/scope';
import { createScopedTestHost, stubPair } from '#/_base/di/test';
import { encodeWorkDirKey } from '#/_base/utils/workdir-slug';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import type { PersistenceScopeName } from '#/app/bootstrap/bootstrap';
import { WorkspaceService } from '#/app/workspace/workspaceService';
import {
  IWorkspacePersistence,
  type IWorkspacePersistence as IWorkspacePersistenceType,
  type PersistedWorkspaceEntry,
  type WorkspaceCatalog,
} from '#/app/workspace/workspacePersistence';
import { FileWorkspacePersistence } from '#/app/workspace/fileWorkspacePersistence';
import { IWorkspaceService } from '#/app/workspace/workspace';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { FileStorageService } from '#/persistence/backends/node-fs/fileStorageService';

/** In-memory catalog store that counts load() calls — the zero-re-read proof. */
class CountingWorkspacePersistence implements IWorkspacePersistenceType {
  declare readonly _serviceBrand: undefined;
  loads = 0;
  saves = 0;
  catalog: WorkspaceCatalog = { workspaces: [], deletedIds: [] };

  async load(): Promise<WorkspaceCatalog | undefined> {
    this.loads++;
    return this.catalog;
  }

  async save(catalog: WorkspaceCatalog): Promise<void> {
    this.saves++;
    this.catalog = catalog;
  }
}

describe('WorkspaceService read cache (process-lifetime)', () => {
  let homeDir: string;
  let currentHost: ReturnType<typeof createScopedTestHost> | undefined;

  beforeEach(async () => {
    _clearScopedRegistryForTests();
    registerScopedService(
      LifecycleScope.App,
      IWorkspacePersistence,
      FileWorkspacePersistence,
      ScopeActivation.OnDemand,
      'workspace',
    );
    registerScopedService(
      LifecycleScope.App,
      IWorkspaceService,
      WorkspaceService,
      ScopeActivation.OnDemand,
      'workspace',
    );
    homeDir = await fsp.mkdtemp(join(os.tmpdir(), 'ws-cache-'));
  });

  afterEach(async () => {
    currentHost?.dispose();
    currentHost = undefined;
    await fsp.rm(homeDir, { recursive: true, force: true });
  });

  /**
   * hostFs stub that stats every path as an existing directory, so
   * `createOrTouch`'s root-existence probe passes for synthetic roots.
   */
  function allDirsHostFs(): IHostFileSystem {
    return {
      stat: () => Promise.resolve({ isFile: false, isDirectory: true, size: 0 }),
    } as unknown as IHostFileSystem;
  }

  function build(persistence: CountingWorkspacePersistence): IWorkspaceService {
    const fileStorage = new FileStorageService(homeDir);
    const bootstrapStub: IBootstrapService = {
      _serviceBrand: undefined,
      platform: process.platform,
      arch: process.arch,
      cwd: process.cwd(),
      osHomeDir: os.homedir(),
      homeDir,
      configPath: join(homeDir, 'config.toml'),
      clientIdentity: { productName: 'test', version: '0.0.0', platform: process.platform } as never,
      args: {} as never,
      sessionsDir: join(homeDir, 'sessions'),
      blobsDir: join(homeDir, 'blobs'),
      storeDir: join(homeDir, 'store'),
      cacheDir: join(homeDir, 'cache'),
      logsDir: join(homeDir, 'logs'),
      configKey: 'config.toml',
      getEnv: () => undefined,
      scope: (name: PersistenceScopeName) => name,
    };
    const host = createScopedTestHost([
      stubPair(IFileSystemStorageService, fileStorage),
      stubPair(IHostFileSystem, allDirsHostFs()),
      stubPair(IBootstrapService, bootstrapStub),
      stubPair(IWorkspacePersistence, persistence),
    ]);
    currentHost = host;
    return host.app.accessor.get(IWorkspaceService);
  }

  function entry(root: string): PersistedWorkspaceEntry {
    return {
      root,
      name: 'proj',
      created_at: '2026-01-01T00:00:00.000Z',
      last_opened_at: '2026-01-01T00:00:00.000Z',
    };
  }

  it('loads the catalog once across repeated list() calls (no per-request re-read)', async () => {
    const persistence = new CountingWorkspacePersistence();
    persistence.catalog = {
      workspaces: [
        { id: encodeWorkDirKey('/tmp/a'), root: '/tmp/a', name: 'a', createdAt: 1, lastOpenedAt: 1 },
      ],
      deletedIds: [],
    };
    const svc = build(persistence);

    await svc.list();
    await svc.list();
    await svc.list();
    // ensureMerged's rebuild check may read once, but steady-state list() calls
    // after the first must be served from memory.
    expect(persistence.loads).toBeLessThanOrEqual(2);
  });

  it('getSnapshot serves the same cached catalog without re-reading', async () => {
    const persistence = new CountingWorkspacePersistence();
    persistence.catalog = {
      workspaces: [
        { id: encodeWorkDirKey('/proj'), root: '/proj', name: 'p', createdAt: 1, lastOpenedAt: 1 },
      ],
      deletedIds: [],
    };
    const svc = build(persistence);

    // Warm the cache (first access may need ensureMerged + its own reads).
    await svc.getSnapshot();
    const before = persistence.loads;
    const snap = await svc.getSnapshot();
    const after = persistence.loads;
    expect(snap.workspaces).toHaveLength(1);
    // A cached getSnapshot adds zero extra loads.
    expect(after - before).toBe(0);
  });

  it('invalidates the cache when a workspace is created, so the new entry is listed', async () => {
    const persistence = new CountingWorkspacePersistence();
    persistence.catalog = { workspaces: [], deletedIds: [] };
    const svc = build(persistence);

    await svc.createOrTouch('/tmp/new-root');
    const listed = await svc.list();
    expect(listed.map((w) => w.id)).toEqual([encodeWorkDirKey('/tmp/new-root')]);
  });
});