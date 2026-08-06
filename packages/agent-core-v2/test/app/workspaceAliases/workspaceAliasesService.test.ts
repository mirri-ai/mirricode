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
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { JsonAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { FileStorageService } from '#/persistence/backends/node-fs/fileStorageService';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IWorkspaceService } from '#/app/workspace/workspace';
import { WorkspaceService } from '#/app/workspace/workspaceService';
import { FileWorkspacePersistence } from '#/app/workspace/fileWorkspacePersistence';
import {
  IWorkspacePersistence,
  type PersistedWorkspaceEntry,
} from '#/app/workspace/workspacePersistence';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import type { PersistenceScopeName } from '#/app/bootstrap/bootstrap';
import { IWorkspaceAliases } from '#/app/workspaceAliases/workspaceAliases';
import { WorkspaceAliasesService } from '#/app/workspaceAliases/workspaceAliasesService';

interface SessionIndexLine {
  readonly sessionId: string;
  readonly sessionDir: string;
  readonly workDir: string;
}

describe('WorkspaceAliasesService (file-backed)', () => {
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
    registerScopedService(
      LifecycleScope.App,
      IWorkspaceAliases,
      WorkspaceAliasesService,
      ScopeActivation.OnDemand,
      'workspaceAliases',
    );
    homeDir = await fsp.mkdtemp(join(os.tmpdir(), 'ws-aliases-'));
  });

  afterEach(async () => {
    currentHost?.dispose();
    currentHost = undefined;
    await fsp.rm(homeDir, { recursive: true, force: true });
  });

  function build(hostFs: IHostFileSystem = new HostFileSystem()): IWorkspaceAliases {
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
    } as IBootstrapService;
    const host = createScopedTestHost([
      stubPair(IFileSystemStorageService, fileStorage),
      stubPair(IAtomicDocumentStore, new JsonAtomicDocumentStore(fileStorage)),
      stubPair(IHostFileSystem, hostFs),
      stubPair(IBootstrapService, bootstrapStub),
    ]);
    currentHost = host;
    return host.app.accessor.get(IWorkspaceAliases);
  }

  async function seedSessionIndex(entries: SessionIndexLine[]): Promise<void> {
    const text = `${entries.map((e) => JSON.stringify(e)).join('\n')}\n`;
    await fsp.writeFile(join(homeDir, 'session_index.jsonl'), text, 'utf8');
  }

  async function writeWorkspacesJson(
    workspaces: Record<string, PersistedWorkspaceEntry>,
    extra?: { readonly deleted_workspace_ids?: unknown },
  ): Promise<void> {
    await fsp.writeFile(
      join(homeDir, 'workspaces.json'),
      JSON.stringify({ version: 1, workspaces, ...extra }),
      'utf8',
    );
  }

  it('resolveAliasIds returns every registered id for one physical directory', async () => {
    // A legacy catalog holds two entries whose roots differ only by casing —
    // one physical folder, two bucket ids (this is what `dedupeByRoot` merges
    // for listing; the alias set exposes both for multi-bucket reads).
    const lowerRoot = 'c:\\users\\foo\\proj';
    const typedRoot = 'C:\\Users\\Foo\\Proj';
    const legacyId = 'wd_proj_deadbeef0002';
    const canonicalId = encodeWorkDirKey(lowerRoot);
    const entry = (root: string): PersistedWorkspaceEntry => ({
      root,
      name: 'proj',
      created_at: '2026-01-01T00:00:00.000Z',
      last_opened_at: '2026-01-01T00:00:00.000Z',
    });
    await writeWorkspacesJson({
      [legacyId]: entry(typedRoot),
      [canonicalId]: entry(lowerRoot),
    });

    const aliases = build();
    for (const id of [legacyId, canonicalId]) {
      expect((await aliases.resolveAliasIds(id)).toSorted()).toEqual(
        [legacyId, canonicalId].toSorted(),
      );
    }
  });

  it('resolveAliasIds folds in session-index-only spellings of the same root', async () => {
    // The sibling bucket's spelling was never registered: only the legacy
    // session index remembers it. Malformed index lines are skipped, never
    // thrown.
    const typedRoot = 'C:\\Users\\Foo\\Proj';
    const typedId = encodeWorkDirKey(typedRoot);
    const indexOnlyId = encodeWorkDirKey('c:\\Users\\Foo\\Proj');
    await writeWorkspacesJson({
      [typedId]: {
        root: typedRoot,
        name: 'proj',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
    });
    await seedSessionIndex([
      { sessionId: 's1', sessionDir: 'sessions/a/s1', workDir: typedRoot },
      { sessionId: 's2', sessionDir: 'sessions/b/s2', workDir: 'c:\\Users\\Foo\\Proj' },
      { sessionId: 's3', sessionDir: 'sessions/c/s3', workDir: join(homeDir, 'unrelated') },
    ]);
    await fsp.appendFile(join(homeDir, 'session_index.jsonl'), 'not-json\n{}\n', 'utf8');

    const aliases = build();
    expect((await aliases.resolveAliasIds(typedId)).toSorted()).toEqual(
      [typedId, indexOnlyId].toSorted(),
    );
  });

  it('resolveAliasIds keeps unknown ids and POSIX roots singleton', async () => {
    const root = join(homeDir, 'posix');
    const id = encodeWorkDirKey(root);
    await writeWorkspacesJson({
      [id]: {
        root,
        name: 'posix',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
    });

    const aliases = build();
    // Unknown id: callers keep their existing not-found semantics.
    expect(await aliases.resolveAliasIds('wd_missing_000000000000')).toEqual([
      'wd_missing_000000000000',
    ]);
    // POSIX roots never fold, so the alias set is just the id itself.
    expect(await aliases.resolveAliasIds(id)).toEqual([id]);
  });

  it('re-resolves a new catalog entry after clearCaches()', async () => {
    const root = join(homeDir, 'proj');
    const id = encodeWorkDirKey(root);
    await writeWorkspacesJson({
      [id]: {
        root,
        name: 'proj',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
    });

    const aliases = build();
    expect(await aliases.resolveAliasIds(id)).toEqual([id]);

    // A new workspace registers after the first resolve. The cached session
    // index view is append-only (grows on session create/fork); workspace
    // registration is reflected through IWorkspaceService, so the catalog part
    // is already fresh. `clearCaches()` (manual refresh path) re-reads the
    // session index on demand — the new id must resolve after it.
    const root2 = join(homeDir, 'proj2');
    const id2 = encodeWorkDirKey(root2);
    await writeWorkspacesJson({
      [id]: {
        root,
        name: 'proj',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
      [id2]: {
        root: root2,
        name: 'proj2',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
    });
    aliases.clearCaches();
    expect(await aliases.resolveAliasIds(id2)).toEqual([id2]);
  });

  it('clearCaches() forces a re-read of a changed session index', async () => {
    const typedRoot = 'C:\\Users\\Foo\\Idx';
    const id = encodeWorkDirKey(typedRoot);
    await writeWorkspacesJson({
      [id]: {
        root: typedRoot,
        name: 'idx',
        created_at: '2026-01-01T00:00:00.000Z',
        last_opened_at: '2026-01-01T00:00:00.000Z',
      },
    });
    await seedSessionIndex([
      { sessionId: 's1', sessionDir: 'sessions/a/s1', workDir: typedRoot },
    ]);

    const aliases = build();
    expect(await aliases.resolveAliasIds(id)).toEqual([id]);

    // A sibling-bucket session appears in the index (a legacy split spelling
    // of the same physical folder written by an external v1 process). The
    // append-only view is cached, so a plain re-resolve must NOT see it;
    // after clearCaches() the alias expansion folds the sibling bucket id in.
    const siblingRoot = 'c:\\users\\Foo\\Idx';
    const siblingId = encodeWorkDirKey(siblingRoot);
    expect(siblingId).not.toEqual(id);
    await seedSessionIndex([
      { sessionId: 's1', sessionDir: 'sessions/a/s1', workDir: typedRoot },
      { sessionId: 's2', sessionDir: 'sessions/b/s2', workDir: siblingRoot },
    ]);
    expect(await aliases.resolveAliasIds(id)).toEqual([id]);
    aliases.clearCaches();
    expect((await aliases.resolveAliasIds(id)).toSorted()).toEqual([id, siblingId].toSorted());
  });
});
