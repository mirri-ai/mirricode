import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { createDiskReconcileSource } from '#/app/sessionIndex/sqliteDiskSource';
import { SqliteSessionIndex, type ReconcileSource } from '#/app/sessionIndex/sqliteSessionIndex';
import { SqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';
import { stubLog } from '../../_base/log/stubs';
import { stubBootstrap } from '../../app/bootstrap/stubs';
import { stubFlag } from '../../app/flag/stubs';
import { stubQueryStore } from '../../persistence/interface/stubs';
import { FileStorageService } from '#/persistence/backends/node-fs/fileStorageService';
import { JsonAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { SyncDescriptor } from '#/_base/di/descriptors';
import { TestInstantiationService } from '#/_base/di/test';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { ILogService } from '#/_base/log/log';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IFlagService } from '#/app/flag/flag';
import { IQueryStore } from '#/persistence/interface/queryStore';
import { ISqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';

describe('createDiskReconcileSource', () => {
  let dir: string;
  let storage: FileStorageService;
  let docs: JsonAtomicDocumentStore;
  let source: ReconcileSource;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'disk-src-'));
    const bs = stubBootstrap(dir);
    storage = new FileStorageService(dir, 0o700, 0o600);
    docs = new JsonAtomicDocumentStore(storage);
    source = createDiskReconcileSource(bs, storage, docs);
  });

  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  it('listWorkspaceIds returns workspace directories under sessions scope', async () => {
    mkdirSync(join(dir, 'sessions', 'ws1'), { recursive: true });
    mkdirSync(join(dir, 'sessions', 'ws2'), { recursive: true });

    const ids = await source.listWorkspaceIds();
    expect([...ids].sort()).toEqual(['ws1', 'ws2']);
  });

  it('listWorkspaceIds returns empty array when sessions dir does not exist', async () => {
    const ids = await source.listWorkspaceIds();
    expect(ids).toEqual([]);
  });

  it('listSessionIds returns session directories under a workspace', async () => {
    mkdirSync(join(dir, 'sessions', 'ws1', 's1'), { recursive: true });
    mkdirSync(join(dir, 'sessions', 'ws1', 's2'), { recursive: true });

    const ids = await source.listSessionIds('ws1');
    expect([...ids].sort()).toEqual(['s1', 's2']);
  });

  it('listSessionIds returns empty array for nonexistent workspace', async () => {
    const ids = await source.listSessionIds('nope');
    expect(ids).toEqual([]);
  });

  it('readSummary reads state.json from session dir and returns SessionSummary', async () => {
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      version: 2,
      workspaceId: 'ws1',
      cwd: '/home/user/project',
      title: 'My Session',
      lastPrompt: 'hello',
      createdAt: 1000,
      updatedAt: 2000,
      archived: false,
      custom: { foo: 'bar' },
    }));

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeDefined();
    expect(summary!.id).toBe('s1');
    expect(summary!.workspaceId).toBe('ws1');
    expect(summary!.cwd).toBe('/home/user/project');
    expect(summary!.title).toBe('My Session');
    expect(summary!.lastPrompt).toBe('hello');
    expect(summary!.createdAt).toBe(1000);
    expect(summary!.updatedAt).toBe(2000);
    expect(summary!.archived).toBe(false);
    expect(summary!.custom).toEqual({ foo: 'bar' });
  });

  it('readSummary falls back to session-meta/state.json', async () => {
    const metaDir = join(dir, 'sessions', 'ws1', 's1', 'session-meta');
    mkdirSync(metaDir, { recursive: true });
    writeFileSync(join(metaDir, 'state.json'), JSON.stringify({
      id: 's1',
      version: 2,
      workspaceId: 'ws1',
      createdAt: 1000,
      updatedAt: 2000,
      archived: true,
    }));

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeDefined();
    expect(summary!.id).toBe('s1');
    expect(summary!.archived).toBe(true);
  });

  it('readSummary returns undefined when no state.json exists', async () => {
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeUndefined();
  });

  it('readSummary handles ISO string timestamps', async () => {
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      workspaceId: 'ws1',
      createdAt: '2024-01-15T10:30:00.000Z',
      updatedAt: '2024-01-16T12:00:00.000Z',
      archived: false,
    }));

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeDefined();
    expect(summary!.createdAt).toBe(Date.parse('2024-01-15T10:30:00.000Z'));
    expect(summary!.updatedAt).toBe(Date.parse('2024-01-16T12:00:00.000Z'));
  });

  it('readSummary recovers cwd from workDir fallback', async () => {
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      workspaceId: 'ws1',
      workDir: '/fallback/path',
      createdAt: 1000,
      updatedAt: 2000,
      archived: false,
    }));

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeDefined();
    expect(summary!.cwd).toBe('/fallback/path');
  });

  it('readSummary recovers cwd from custom.cwd', async () => {
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      workspaceId: 'ws1',
      createdAt: 1000,
      updatedAt: 2000,
      archived: false,
      custom: { cwd: '/custom/path' },
    }));

    const summary = await source.readSummary('ws1', 's1');
    expect(summary).toBeDefined();
    expect(summary!.cwd).toBe('/custom/path');
  });
});

describe('disk reconcile integration', () => {
  let dir: string;
  let store: SqliteSessionStore;
  let index: SqliteSessionIndex;
  let ix: TestInstantiationService;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'disk-recon-'));
    store = SqliteSessionStore.createForPath(join(dir, 'i.db'));

    const bs = stubBootstrap(dir);

    ix = new TestInstantiationService();
    ix.stub(ILogService, stubLog());
    ix.stub(IBootstrapService, bs);
    ix.set(
      IFileSystemStorageService,
      new SyncDescriptor(FileStorageService, [dir, 0o700, 0o600]),
    );
    ix.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
    ix.stub(IQueryStore, stubQueryStore());
    ix.stub(IFlagService, stubFlag(false));
    ix.stub(ISqliteSessionStore, store);

    index = ix.createInstance(SqliteSessionIndex);
  });

  afterEach(() => {
    store.close();
    ix.dispose();
    rmSync(dir, { recursive: true, force: true });
  });

  it('given disk sessions and empty store, when reconcile with disk source, then store is populated', async () => {
    // Create a session on disk
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      version: 2,
      workspaceId: 'ws1',
      cwd: '/home/user/project',
      title: 'Disk Session',
      lastPrompt: 'hello',
      createdAt: 1000,
      updatedAt: 2000,
      archived: false,
      custom: {},
    }));

    // Build the disk source the same way start.ts does
    const bs = ix.get(IBootstrapService);
    const storage = ix.get(IFileSystemStorageService);
    const docs = ix.get(IAtomicDocumentStore);
    const diskSource = createDiskReconcileSource(bs, storage, docs);

    const result = await index.reconcile(diskSource);

    expect(result.inserted).toBe(1);
    expect(result.removed).toBe(0);

    // Verify the store now contains the session
    const summary = await index.get('s1');
    expect(summary).toBeDefined();
    expect(summary!.id).toBe('s1');
    expect(summary!.title).toBe('Disk Session');
    expect(await index.countActive(['ws1'])).toBe(1);
  });

  it('given disk sessions and pre-existing store entries, when reconcile, then orphans are removed and new sessions backfilled', async () => {
    store.open();
    // Pre-seed an orphan
    store.upsert({
      id: 'orphan',
      workspaceId: 'ws1',
      createdAt: 1,
      updatedAt: 1,
      archived: false,
    });

    // Create a real session on disk
    const sessionDir = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(sessionDir, { recursive: true });
    writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
      id: 's1',
      workspaceId: 'ws1',
      createdAt: 1000,
      updatedAt: 2000,
      archived: false,
    }));

    const bs = ix.get(IBootstrapService);
    const storage = ix.get(IFileSystemStorageService);
    const docs = ix.get(IAtomicDocumentStore);
    const diskSource = createDiskReconcileSource(bs, storage, docs);

    const result = await index.reconcile(diskSource);

    expect(result.inserted).toBe(1); // s1 backfilled
    expect(result.removed).toBe(1); // orphan removed
    expect(store.get('orphan')).toBeUndefined();
    expect(store.get('s1')?.id).toBe('s1');
  });

  it('given multiple workspaces on disk, when reconcile, then all sessions are backfilled', async () => {
    // ws1/s1
    const d1 = join(dir, 'sessions', 'ws1', 's1');
    mkdirSync(d1, { recursive: true });
    writeFileSync(join(d1, 'state.json'), JSON.stringify({
      id: 's1', workspaceId: 'ws1', createdAt: 1000, updatedAt: 2000, archived: false,
    }));

    // ws2/s2
    const d2 = join(dir, 'sessions', 'ws2', 's2');
    mkdirSync(d2, { recursive: true });
    writeFileSync(join(d2, 'state.json'), JSON.stringify({
      id: 's2', workspaceId: 'ws2', createdAt: 3000, updatedAt: 4000, archived: false,
    }));

    const bs = ix.get(IBootstrapService);
    const storage = ix.get(IFileSystemStorageService);
    const docs = ix.get(IAtomicDocumentStore);
    const diskSource = createDiskReconcileSource(bs, storage, docs);

    const result = await index.reconcile(diskSource);

    expect(result.inserted).toBe(2);
    expect(store.get('s1')?.id).toBe('s1');
    expect(store.get('s2')?.id).toBe('s2');
  });
});
