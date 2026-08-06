import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { SqliteSessionIndex, type ReconcileSource } from '#/app/sessionIndex/sqliteSessionIndex';
import { SqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';
import { createDiskReconcileSource } from '#/app/sessionIndex/sqliteDiskSource';
import { stubLog } from '../../_base/log/stubs';
import { stubBootstrap } from '../../app/bootstrap/stubs';
import { stubFlag } from '../../app/flag/stubs';
import { stubQueryStore } from '../../persistence/interface/stubs';
import { InMemoryStorageService } from '#/persistence/backends/memory/inMemoryStorageService';
import { JsonAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { FileStorageService } from '#/persistence/backends/node-fs/fileStorageService';
import { SyncDescriptor } from '#/_base/di/descriptors';
import { ServiceCollection } from '#/_base/di/serviceCollection';
import { TestInstantiationService } from '#/_base/di/test';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { ILogService } from '#/_base/log/log';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IFlagService } from '#/app/flag/flag';
import { IQueryStore } from '#/persistence/interface/queryStore';
import { ISqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';

describe('SqliteSessionIndex', () => {
  let dir: string;
  let store: SqliteSessionStore;
  let index: SqliteSessionIndex;
  let ix: TestInstantiationService;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sq-idx-'));
    store = SqliteSessionStore.createForPath(join(dir, 'i.db'));

    ix = new TestInstantiationService();
    ix.stub(ILogService, stubLog());
    ix.stub(IBootstrapService, stubBootstrap());
    ix.set(IFileSystemStorageService, new SyncDescriptor(InMemoryStorageService));
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

  it('should list sessions respecting workspaceIds and includeArchived', async () => {
    store.open();
    store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 2, archived: false });
    store.upsert({ id: 'b', workspaceId: 'w', createdAt: 2, updatedAt: 1, archived: true });
    const page = await index.list({ workspaceIds: ['w'] });
    expect(page.items.map(s => s.id)).toEqual(['a']);
  });

  it('should include archived sessions when includeArchived is true', async () => {
    store.open();
    store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 2, archived: false });
    store.upsert({ id: 'b', workspaceId: 'w', createdAt: 2, updatedAt: 1, archived: true });
    const page = await index.list({ workspaceIds: ['w'], includeArchived: true });
    expect(page.items.map(s => s.id).sort()).toEqual(['a', 'b']);
  });

  it('should get a summary by id or undefined', async () => {
    store.open();
    store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
    expect((await index.get('a'))?.id).toBe('a');
    expect(await index.get('nope')).toBeUndefined();
  });

  it('should count active sessions across workspaces', async () => {
    store.open();
    store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
    store.upsert({ id: 'b', workspaceId: 'w', createdAt: 2, updatedAt: 2, archived: true });
    store.upsert({ id: 'c', workspaceId: 'x', createdAt: 3, updatedAt: 3, archived: false });
    expect(await index.countActive(['w'])).toBe(1);
    expect(await index.countActive(['w', 'x'])).toBe(2);
  });

  describe('reconcile', () => {
    it('given orphan in store and disk session missing from store, when reconcile, then orphan removed and disk session backfilled', async () => {
      store.open();
      // Pre-seed an orphan session that does NOT exist on disk
      store.upsert({ id: 'orphan', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['w'],
        listSessionIds: async (_ws: string) => ['disk1'],
        readSummary: async (_ws: string, sid: string) => ({
          id: sid, workspaceId: 'w', createdAt: 2, updatedAt: 2, archived: false,
        }),
        statStateMtime: async () => undefined,
      };

      const res = await index.reconcile(source);

      // Disk session was backfilled into the store
      expect(store.get('disk1')?.id).toBe('disk1');
      // Orphan was removed from the store
      expect(store.get('orphan')).toBeUndefined();
      // Precise counts: only disk1 was inserted (orphan existed already), orphan was removed
      expect(res).toEqual({ inserted: 1, removed: 1 });
    });

    it('given store session title changed on disk, when reconcile, then the title is updated from disk (mtime differs)', async () => {
      store.open();
      store.upsert({ id: 'renamed', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, title: 'Old title', archived: false });
      store.setStateMtime('renamed', 100);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['renamed'],
        readSummary: async () => ({
          id: 'renamed', workspaceId: 'ws1', createdAt: 1, updatedAt: 2, title: 'New title', archived: false,
        }),
        statStateMtime: async () => 200, // disk state.json rewritten → mtime changed
      };

      const res = await index.reconcile(source);

      expect(store.get('renamed')?.title).toBe('New title');
      expect(store.getStateMtime('renamed')).toBe(200);
      expect(res.inserted).toBe(1);
      expect(res.removed).toBe(0);
    });

    it('given store session archived on disk, when reconcile with archived true, then the row is marked archived', async () => {
      store.open();
      store.upsert({ id: 'archived-later', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('archived-later', 100);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['archived-later'],
        readSummary: async () => ({
          id: 'archived-later', workspaceId: 'ws1', createdAt: 1, updatedAt: 2, archived: true,
        }),
        statStateMtime: async () => 200,
      };

      const res = await index.reconcile(source);

      expect(store.get('archived-later')?.archived).toBe(true);
      expect(res.inserted).toBe(1);
    });

    it('given existing session also on disk with changed mtime, when reconcile, then it is updated (counted as inserted)', async () => {
      store.open();
      // Pre-seed a session that also exists on disk, with a known stateMtime
      store.upsert({ id: 'disk1', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('disk1', 100);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['w'],
        listSessionIds: async (_ws: string) => ['disk1'],
        readSummary: async (_ws: string, sid: string) => ({
          id: sid, workspaceId: 'w', createdAt: 2, updatedAt: 2, archived: false,
        }),
        statStateMtime: async () => 200, // mtime changed
      };

      const res = await index.reconcile(source);

      // Session was updated (updatedAt changed)
      expect(store.get('disk1')?.updatedAt).toBe(2);
      // Updated sessions are counted as inserted
      expect(res.inserted).toBe(1);
      expect(res.removed).toBe(0);
    });

    it('given empty source (no workspaces), when reconcile, then all store sessions are removed and inserted is zero', async () => {
      store.open();
      store.upsert({ id: 's1', workspaceId: 'w1', createdAt: 1, updatedAt: 1, archived: false });
      store.upsert({ id: 's2', workspaceId: 'w2', createdAt: 2, updatedAt: 2, archived: false });

      const source: ReconcileSource = {
        listWorkspaceIds: async () => [],
        listSessionIds: async (_ws: string) => [],
        readSummary: async () => undefined,
        statStateMtime: async () => undefined,
      };

      const res = await index.reconcile(source);

      expect(store.get('s1')).toBeUndefined();
      expect(store.get('s2')).toBeUndefined();
      expect(res).toEqual({ inserted: 0, removed: 2 });
    });

    it('given readSummary returns undefined for a disk session, when reconcile, then that id is not inserted and not deleted', async () => {
      store.open();
      // Pre-seed an orphan so the store is not empty
      store.upsert({ id: 'orphan', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['w'],
        listSessionIds: async (_ws: string) => ['ghost'],
        readSummary: async () => undefined,
        statStateMtime: async () => undefined,
      };

      const res = await index.reconcile(source);

      // ghost was never inserted (readSummary was undefined)
      expect(store.get('ghost')).toBeUndefined();
      // orphan was removed (not on disk)
      expect(store.get('orphan')).toBeUndefined();
      expect(res).toEqual({ inserted: 0, removed: 1 });
    });

    it('given multiple workspaces, when reconcile, then orphans and backfills are computed per workspace correctly', async () => {
      store.open();
      // w1: orphan only
      store.upsert({ id: 'w1-orphan', workspaceId: 'w1', createdAt: 1, updatedAt: 1, archived: false });
      // w2: session that also exists on disk (with mtime changed → will be updated)
      store.upsert({ id: 'w2-existing', workspaceId: 'w2', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('w2-existing', 100);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['w1', 'w2'],
        listSessionIds: async (ws: string) =>
          ws === 'w1' ? ['w1-disk'] : ['w2-existing', 'w2-new'],
        readSummary: async (ws: string, sid: string) => ({
          id: sid, workspaceId: ws, createdAt: 5, updatedAt: 5, archived: false,
        }),
        statStateMtime: async (_ws: string, sid: string) => {
          if (sid === 'w2-existing') return 200; // mtime changed → triggers update
          return 999; // new sessions get a mtime
        },
      };

      const res = await index.reconcile(source);

      // w1-orphan removed, w1-disk inserted
      expect(store.get('w1-orphan')).toBeUndefined();
      expect(store.get('w1-disk')?.id).toBe('w1-disk');
      // w2-existing updated (mtime changed)
      expect(store.get('w2-existing')?.updatedAt).toBe(5);
      // w2-new inserted
      expect(store.get('w2-new')?.id).toBe('w2-new');
      // inserted: w1-disk + w2-existing(update) + w2-new = 3; removed: w1-orphan = 1
      expect(res).toEqual({ inserted: 3, removed: 1 });
    });
  });

  describe('reconcile incremental (stateMtime)', () => {
    it('given disk new session (simulating v1 create), when reconcile, then upsert + inserted+1', async () => {
      store.open();
      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['new-sess'],
        readSummary: async (_ws: string, sid: string) => ({
          id: sid, workspaceId: 'ws1', createdAt: 10, updatedAt: 10, archived: false,
        }),
        statStateMtime: async () => 5000,
      };

      const res = await index.reconcile(source);

      expect(store.get('new-sess')?.id).toBe('new-sess');
      expect(res.inserted).toBe(1);
      expect(res.removed).toBe(0);
    });

    it('given store session with no disk counterpart (orphan), when reconcile, then delete + removed+1', async () => {
      store.open();
      store.upsert({ id: 'orphan-sess', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });

      const source: ReconcileSource = {
        listWorkspaceIds: async () => [],
        listSessionIds: async () => [],
        readSummary: async () => undefined,
        statStateMtime: async () => undefined,
      };

      const res = await index.reconcile(source);

      expect(store.get('orphan-sess')).toBeUndefined();
      expect(res).toEqual({ inserted: 0, removed: 1 });
    });

    it('given mtime same on both sides, when reconcile, then readSummary and upsert are NOT called (0-change fast path)', async () => {
      store.open();
      store.upsert({ id: 'unchanged', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('unchanged', 12345);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['unchanged'],
        // M-2: stub throws if called — fast path must NEVER reach readSummary
        readSummary: async () => { throw new Error('reconcile must not read unchanged sessions'); },
        statStateMtime: async () => 12345, // same mtime
      };

      const res = await index.reconcile(source);

      expect(res).toEqual({ inserted: 0, removed: 0 });
    });

    it('given both storeMtime and diskMtime undefined, when reconcile, then readSummary is attempted and session is left as-is if undefined returned', async () => {
      store.open();
      // Session exists in store but has no stateMtime (e.g. migrated from old DB)
      store.upsert({ id: 'pending', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      // stateMtime remains undefined (never set)

      let readSummaryCalls = 0;
      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['pending'],
        readSummary: async () => {
          readSummaryCalls++;
          return undefined; // state.json missing on disk
        },
        statStateMtime: async () => undefined, // disk has no state.json
      };

      const res = await index.reconcile(source);

      // readSummary was attempted (not skipped)
      expect(readSummaryCalls).toBe(1);
      // Session is not deleted — readSummary returned undefined
      expect(store.get('pending')?.id).toBe('pending');
      // Nothing inserted, nothing removed
      expect(res).toEqual({ inserted: 0, removed: 0 });
    });

    it('given mtime changed on disk, when reconcile, then re-read + upsert + setStateMtime updated', async () => {
      store.open();
      store.upsert({ id: 'changed', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('changed', 1000);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['changed'],
        readSummary: async (_ws: string, sid: string) => ({
          id: sid, workspaceId: 'ws1', createdAt: 1, updatedAt: 99, archived: false,
        }),
        statStateMtime: async () => 2000, // mtime changed
      };

      const res = await index.reconcile(source);

      // Session was updated
      expect(store.get('changed')?.updatedAt).toBe(99);
      // stateMtime was updated
      expect(store.getStateMtime('changed')).toBe(2000);
      // Counted as updated (in the inserted count for backward compat)
      expect(res.inserted).toBeGreaterThanOrEqual(1);
      expect(res.removed).toBe(0);
    });

    it('given session deleted from disk after restart, when reconcile, then removed from store', async () => {
      store.open();
      store.upsert({ id: 'deleted', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('deleted', 5000);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        // deleted is NOT listed in disk session ids
        listSessionIds: async () => [],
        readSummary: async () => undefined,
        statStateMtime: async () => undefined,
      };

      const res = await index.reconcile(source);

      expect(store.get('deleted')).toBeUndefined();
      expect(res).toEqual({ inserted: 0, removed: 1 });
    });

    it('given same session id moves to another workspaceId on disk, when reconcile, then workspaceId updates in place and the orphan pass does not delete it', async () => {
      store.open();
      // Old row: session id exists under workspace ws1 (e.g. workspace was renamed)
      store.upsert({ id: 'moved', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('moved', 100);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws2'],
        listSessionIds: async () => ['moved'],
        readSummary: async (_ws: string, sid: string) => ({
          id: sid, workspaceId: 'ws2', createdAt: 9, updatedAt: 9, archived: false,
        }),
        // mtime differs → re-read path, upsert carries the row to ws2
        statStateMtime: async () => 200,
      };

      const res = await index.reconcile(source);

      // Row survives with the *new* workspace — the orphan pass must not delete it
      expect(store.get('moved')?.workspaceId).toBe('ws2');
      expect(store.get('moved')?.updatedAt).toBe(9);
      // Moved session is not counted as removed (it still exists on disk)
      expect(res.removed).toBe(0);
      expect(res.inserted).toBe(1);
    });

    it('given mixed scenario (new + orphan + changed + unchanged), when reconcile, then correct counts and state', async () => {
      store.open();
      // new-on-disk: not in store
      // orphan: in store but not on disk
      store.upsert({ id: 'orphan', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      // changed: in both, mtime differs
      store.upsert({ id: 'changed', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('changed', 100);
      // unchanged: in both, mtime same
      store.upsert({ id: 'unchanged', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false });
      store.setStateMtime('unchanged', 200);

      let readSummaryCalls = 0;
      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['ws1'],
        listSessionIds: async () => ['changed', 'unchanged', 'new-sess'],
        readSummary: async (_ws: string, sid: string) => {
          readSummaryCalls++;
          return { id: sid, workspaceId: 'ws1', createdAt: 1, updatedAt: 50, archived: false };
        },
        statStateMtime: async (_ws: string, sid: string) => {
          if (sid === 'changed') return 999;   // mtime changed
          if (sid === 'unchanged') return 200;  // mtime same
          if (sid === 'new-sess') return 300;   // new session
          return undefined;
        },
      };

      const res = await index.reconcile(source);

      // orphan removed
      expect(store.get('orphan')).toBeUndefined();
      // new-sess inserted
      expect(store.get('new-sess')?.id).toBe('new-sess');
      // changed updated
      expect(store.get('changed')?.updatedAt).toBe(50);
      expect(store.getStateMtime('changed')).toBe(999);
      // unchanged not touched (readSummary should NOT be called for it)
      // readSummary called for: changed, new-sess (2 calls; NOT for unchanged)
      expect(readSummaryCalls).toBe(2);
      // inserted: new-sess (1) + changed updated (1) = 2; removed: orphan (1)
      expect(res.inserted).toBe(2);
      expect(res.removed).toBe(1);
    });
  });

  describe('fallback on open failure', () => {
    it('delegates to FileSessionIndex when store open fails', async () => {
      // Create a DI container with a real file-backed FileSessionIndex setup
      const fallbackDir = mkdtempSync(join(tmpdir(), 'sq-fb-'));
      try {
        const bs = stubBootstrap(fallbackDir);
        // sessionsScope = bootstrap.scope('sessions') = 'sessions' (from stub)
        // FileStorageService roots at homeDir, so the on-disk path is:
        //   <fallbackDir>/sessions/<workspaceId>/<sessionId>/session-meta/state.json
        const sessionDir = join(fallbackDir, 'sessions', 'ws1', 's1', 'session-meta');
        mkdirSync(sessionDir, { recursive: true });
        writeFileSync(join(sessionDir, 'state.json'), JSON.stringify({
          id: 's1',
          version: 2,
          workspaceId: 'ws1',
          cwd: '/tmp/work',
          title: 'legacy-session',
          createdAt: 100,
          updatedAt: 200,
          archived: false,
          agents: {},
          custom: {},
        }));

        // Create an index with a store pointing to an invalid path (will trigger fallback)
        const badStore = SqliteSessionStore.createForPath('/nonexistent/deep/path/db.db');
        const fallbackIx = new TestInstantiationService();
        fallbackIx.stub(ILogService, stubLog());
        fallbackIx.stub(IBootstrapService, bs);
        // FileStorageService rooted at fallbackDir so FileSessionIndex reads real files
        fallbackIx.set(IFileSystemStorageService, new SyncDescriptor(FileStorageService, [fallbackDir, 0o700, 0o600]));
        fallbackIx.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
        fallbackIx.stub(IQueryStore, stubQueryStore());
        fallbackIx.stub(IFlagService, stubFlag(false));
        fallbackIx.stub(ISqliteSessionStore, badStore);

        const badIndex = fallbackIx.createInstance(SqliteSessionIndex);

        // list should delegate to FileSessionIndex and find the on-disk session
        const page = await badIndex.list({ workspaceIds: ['ws1'] });
        expect(page.items.length).toBe(1);
        expect(page.items[0]!.id).toBe('s1');
        expect(page.items[0]!.title).toBe('legacy-session');

        // get should delegate to FileSessionIndex
        const summary = await badIndex.get('s1');
        expect(summary).toBeDefined();
        expect(summary!.id).toBe('s1');

        // countActive should delegate to FileSessionIndex
        const count = await badIndex.countActive(['ws1']);
        expect(count).toBe(1);

        fallbackIx.dispose();
      } finally {
        rmSync(fallbackDir, { recursive: true, force: true });
      }
    });

    it('reconcile returns zeroes when in fallback mode (no SQLite to reconcile)', async () => {
      const badStore = SqliteSessionStore.createForPath('/nonexistent/deep/path/db.db');
      const fallbackIx = new TestInstantiationService();
      fallbackIx.stub(ILogService, stubLog());
      fallbackIx.stub(IBootstrapService, stubBootstrap());
      fallbackIx.set(IFileSystemStorageService, new SyncDescriptor(InMemoryStorageService));
      fallbackIx.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
      fallbackIx.stub(IQueryStore, stubQueryStore());
      fallbackIx.stub(IFlagService, stubFlag(false));
      fallbackIx.stub(ISqliteSessionStore, badStore);

      const badIndex = fallbackIx.createInstance(SqliteSessionIndex);

      const source: ReconcileSource = {
        listWorkspaceIds: async () => ['w'],
        listSessionIds: async () => ['s1'],
        readSummary: async () => ({ id: 's1', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false }),
        statStateMtime: async () => undefined,
      };

      const res = await badIndex.reconcile(source);
      expect(res).toEqual({ inserted: 0, removed: 0 });

      fallbackIx.dispose();
    });
  });

  describe('authoritative read path (no disk sync on read)', () => {
    it('given a session written to disk without any reconcile, then list/countActive hide it until refreshWorkspace, while get falls back to the legacy file read', async () => {
      const syncDir = mkdtempSync(join(tmpdir(), 'sq-sync-'));
      try {
        const bs = stubBootstrap(syncDir);
        const storage = new FileStorageService(syncDir, 0o700, 0o600);
        const docs = new JsonAtomicDocumentStore(storage);
        const store = SqliteSessionStore.createForPath(join(syncDir, 'i.db'));
        store.open();
        try {
          // A session written straight to disk, exactly what the API create
          // path produces — no reconcile has run (yet).
          const sessionDir = join(syncDir, 'sessions', 'ws1', 's1');
          mkdirSync(sessionDir, { recursive: true });
          writeFileSync(
            join(sessionDir, 'state.json'),
            JSON.stringify({
              id: 's1', workspaceId: 'ws1', cwd: '/work', title: 'fresh',
              createdAt: 1, updatedAt: 2, archived: false, custom: {},
            }),
          );

          const ix = new TestInstantiationService();
          ix.stub(ILogService, stubLog());
          ix.stub(IBootstrapService, bs);
          ix.set(IFileSystemStorageService, new SyncDescriptor(FileStorageService, [syncDir, 0o700, 0o600]));
          ix.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
          ix.stub(IQueryStore, stubQueryStore());
          ix.stub(IFlagService, stubFlag(false));
          ix.stub(ISqliteSessionStore, store);
          const index = ix.createInstance(SqliteSessionIndex);

          // Authoritative read: no sqlite row, so list/count stay empty even
          // though the session exists on disk.
          const page = await index.list({ workspaceIds: ['ws1'] });
          expect(page.items.map((s) => s.id)).toEqual([]);
          expect(await index.countActive(['ws1'])).toBe(0);

          // get keeps its single-session legacy fallback (no directory sweep):
          // a deep-linked v1 session still resolves without a refresh.
          expect((await index.get('s1'))?.title).toBe('fresh');

          // The manual refresh is the only path that converges disk → sqlite.
          const result = await index.refreshWorkspace(['ws1']);
          expect(result.inserted).toBe(1);
          const fresh = await index.list({ workspaceIds: ['ws1'] });
          expect(fresh.items.map((s) => s.id)).toEqual(['s1']);
          expect(await index.countActive(['ws1'])).toBe(1);

          ix.dispose();
        } finally {
          store.close();
        }
      } finally {
        rmSync(syncDir, { recursive: true, force: true });
      }
    });
  });

  describe('reconcile performance (cold-start startup gate)', () => {
    it('should reconcile 200 unchanged sessions fast on a warm store (0-change fast path)', async () => {
      // Real on-disk home with 200 sessions across 10 workspaces (v1 layout:
      // <sessions>/<ws>/<sid>/session-meta/state.json).
      const perfDir = mkdtempSync(join(tmpdir(), 'sq-perf-'));
      try {
        const bs = stubBootstrap(perfDir);
        const storage = new FileStorageService(perfDir, 0o700, 0o600);
        const docs = new JsonAtomicDocumentStore(storage);
        const store = SqliteSessionStore.createForPath(join(perfDir, 'session-index.db'));
        store.open();
        try {
          const ix = new TestInstantiationService();
          ix.stub(ILogService, stubLog());
          ix.stub(IBootstrapService, bs);
          ix.set(IFileSystemStorageService, new SyncDescriptor(FileStorageService, [perfDir, 0o700, 0o600]));
          ix.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
          ix.stub(IQueryStore, stubQueryStore());
          ix.stub(IFlagService, stubFlag(false));
          ix.stub(ISqliteSessionStore, store);
          const index = ix.createInstance(SqliteSessionIndex);
          const source = createDiskReconcileSource(bs, storage, docs);

          const SESSIONS = 200;
          const WORKSPACES = 10;
          for (let w = 0; w < WORKSPACES; w++) {
            const wsId = `ws${w}`;
            for (let s = 0; s < SESSIONS / WORKSPACES; s++) {
              const sid = `session-${w}-${s}`;
              const metaDir = join(perfDir, 'sessions', wsId, sid, 'session-meta');
              mkdirSync(metaDir, { recursive: true });
              writeFileSync(
                join(metaDir, 'state.json'),
                JSON.stringify({
                  id: sid,
                  version: 2,
                  workspaceId: wsId,
                  cwd: `/work/${wsId}`,
                  title: `session ${s}`,
                  lastPrompt: 'hi',
                  createdAt: 100 + s,
                  updatedAt: 200 + s,
                  archived: false,
                  custom: {},
                }),
              );
            }
          }

          // First reconcile seeds the empty store from disk; warm the store.
          const first = await index.reconcile(source);
          expect(first.inserted).toBe(SESSIONS);
          expect(first.removed).toBe(0);

          // Second reconcile: no disk change — must be a 0-change fast path.
          const start = performance.now();
          const second = await index.reconcile(source);
          const elapsedMs = performance.now() - start;
          expect(second.inserted).toBe(0);
          expect(second.removed).toBe(0);
          // The 0-change fast path runs in ~10-80ms on a quiet machine; on a
          // loaded one (shared CI disk, parallel suite) it can spike well past
          // 100ms. The inserted===0 assertion above is the real correctness
          // gate, so this generous bound only needs to catch a regression back
          // to the full 200-session re-read path (which takes seconds).
          expect(elapsedMs).toBeLessThanOrEqual(1000);
          ix.dispose();
        } finally {
          store.close();
        }
      } finally {
        rmSync(perfDir, { recursive: true, force: true });
      }
    });
  });

  describe('read-path convergence (warm-store zero-scan)', () => {
    // Build a real on-disk home with one seeded session per workspace, plus a
    // counting wrapper around the disk source so tests can assert *how many*
    // enumerations actually happened.
    function buildWarmHarness(): {
      dir: string;
      index: SqliteSessionIndex;
      ix: TestInstantiationService;
      store: SqliteSessionStore;
      /** The counting disk source installed on the index — pass it to
       *  `reconcile()`/`refreshWorkspace()` so convergence itself is counted
       *  against the same counters as reads. */
      source: ReconcileSource;
      counts: { listWorkspaceIds: number; listSessionIds: number; statWorkspaceMtime: number };
    } {
      const dir = mkdtempSync(join(tmpdir(), 'sq-warm-'));
      const bs = stubBootstrap(dir);
      const storage = new FileStorageService(dir, 0o700, 0o600);
      const docs = new JsonAtomicDocumentStore(storage);
      const dbPath = join(dir, 'session-index.db');
      const store = SqliteSessionStore.createForPath(dbPath);
      store.open();
      const ix = new TestInstantiationService();
      ix.stub(ILogService, stubLog());
      ix.stub(IBootstrapService, bs);
      ix.set(IFileSystemStorageService, new SyncDescriptor(FileStorageService, [dir, 0o700, 0o600]));
      ix.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
      ix.stub(IQueryStore, stubQueryStore());
      ix.stub(IFlagService, stubFlag(false));
      ix.stub(ISqliteSessionStore, store);
      const index = ix.createInstance(SqliteSessionIndex);

      const realSource = createDiskReconcileSource(bs, storage, docs);
      const counts = { listWorkspaceIds: 0, listSessionIds: 0, statWorkspaceMtime: 0 };
      const countingSource: ReconcileSource = {
        listWorkspaceIds: async () => {
          counts.listWorkspaceIds++;
          return realSource.listWorkspaceIds();
        },
        listSessionIds: async (ws: string) => {
          counts.listSessionIds++;
          return realSource.listSessionIds(ws);
        },
        readSummary: (ws: string, sid: string) => realSource.readSummary(ws, sid),
        statStateMtime: (ws: string, sid: string) => realSource.statStateMtime(ws, sid),
        statWorkspaceMtime: async (ws: string) => {
          counts.statWorkspaceMtime++;
          return realSource.statWorkspaceMtime ? realSource.statWorkspaceMtime(ws) : undefined;
        },
      };
      index.setDiskSourceForTests(countingSource);
      const harness: {
        dir: string;
        ix: TestInstantiationService;
        source: ReconcileSource;
        counts: typeof counts;
      } = { dir, ix, source: countingSource, counts };
      return { ...harness, index, store };
    }

    function seedSession(
      dir: string,
      workspaceId: string,
      sessionId: string,
      title: string,
      updatedAt = 100,
    ): void {
      const sessionDir = join(dir, 'sessions', workspaceId, sessionId);
      mkdirSync(sessionDir, { recursive: true });
      writeFileSync(
        join(sessionDir, 'state.json'),
        JSON.stringify({
          id: sessionId,
          version: 2,
          workspaceId,
          cwd: `/work/${workspaceId}`,
          title,
          lastPrompt: 'hi',
          createdAt: 1,
          updatedAt,
          archived: false,
          custom: {},
        }),
      );
    }

    it('given the store converged by the startup reconcile, when list/countActive run, then no disk enumeration happens', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 't1');
        seedSession(dir, 'ws2', 's2', 't2');
        // Startup convergence primes the store (and re-reads on future
        // refreshes only). After that the read path must never touch disk.
        await index.reconcile(source);

        const baseline = { ...counts };
        const page = await index.list({ workspaceIds: ['ws1', 'ws2'] });
        expect(page.items.map((s) => s.id).sort()).toEqual(['s1', 's2']);
        const count = await index.countActive(['ws1', 'ws2']);
        expect(count).toBe(2);
        expect(counts.listWorkspaceIds).toBe(baseline.listWorkspaceIds);
        expect(counts.listSessionIds).toBe(baseline.listSessionIds);
        expect(counts.statWorkspaceMtime).toBe(baseline.statWorkspaceMtime);
      } finally {
        ix.dispose();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given concurrent list calls on a converged store, then neither read touches disk', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'a');
        await index.reconcile(source);
        const baseline = { ...counts };
        const [a, b] = await Promise.all([
          index.list({ workspaceIds: ['ws1'] }),
          index.list({ workspaceIds: ['ws1'] }),
        ]);
        expect(a.items.map((s) => s.id)).toEqual(['s1']);
        expect(b.items.map((s) => s.id)).toEqual(['s1']);
        expect(counts.listWorkspaceIds).toBe(baseline.listWorkspaceIds);
        expect(counts.listSessionIds).toBe(baseline.listSessionIds);
        expect(counts.statWorkspaceMtime).toBe(baseline.statWorkspaceMtime);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given a session already in the store, then get does not scan disk', async () => {
      const { dir, index, counts, ix, store } = buildWarmHarness();
      try {
        // Seed the store so the row exists without any disk session — the
        // warm path must resolve purely from SQLite.
        store.upsert({
          id: 'warm-row',
          workspaceId: 'ws1',
          cwd: '/work/ws1',
          title: 'warm',
          createdAt: 1,
          updatedAt: 2,
          archived: false,
        });
        const summary = await index.get('warm-row');
        expect(summary?.title).toBe('warm');
        expect(counts.listWorkspaceIds).toBe(0);
        expect(counts.listSessionIds).toBe(0);
        expect(counts.statWorkspaceMtime).toBe(0);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given a disk-only session (v1 wrote it), then get falls back to the legacy file read', async () => {
      const { dir, index, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'legacy-title');
        // Store is empty for this id: get must find it on disk via the
        // legacy file index (no directory sweep of unrelated workspaces).
        const summary = await index.get('s1');
        expect(summary?.title).toBe('legacy-title');
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given a v1 process writes a new session to disk while the server runs, then list stays authoritative and refreshWorkspace brings it in', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'before');
        await index.reconcile(source);
        const baseline = { ...counts };

        // External v1 process drops a new session directory during runtime.
        await new Promise((resolve) => setTimeout(resolve, 20)); // ensure mtime changes
        seedSession(dir, 'ws1', 's2', 'external');

        // Authoritative read: sqlite does not auto-backfill — still s1 only.
        const stale = await index.list({ workspaceIds: ['ws1'] });
        expect(stale.items.map((s) => s.id)).toEqual(['s1']);
        expect(counts.listSessionIds).toBe(baseline.listSessionIds);

        // Manual refresh converges the workspace from disk.
        const refreshed = await index.refreshWorkspace(['ws1']);
        expect(refreshed.inserted).toBeGreaterThanOrEqual(1);
        const fresh = await index.list({ workspaceIds: ['ws1'] });
        expect(fresh.items.map((s) => s.id)).toEqual(['s2', 's1']);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given a session removed from disk while the server runs, then list keeps serving its row until refreshWorkspace removes it', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'a');
        await index.reconcile(source);
        const baseline = { ...counts };

        // External removal: the session directory disappears from disk.
        rmSync(join(dir, 'sessions', 'ws1', 's1'), { recursive: true, force: true });

        // Authoritative read: sqlite still serves the previously converged row.
        const stale = await index.list({ workspaceIds: ['ws1'] });
        expect(stale.items.map((s) => s.id)).toEqual(['s1']);
        expect(counts.listSessionIds).toBe(baseline.listSessionIds);

        // Manual refresh converges: the deleted session is dropped.
        const refreshed = await index.refreshWorkspace(['ws1']);
        expect(refreshed.removed).toBeGreaterThanOrEqual(1);
        const fresh = await index.list({ workspaceIds: ['ws1'] });
        expect(fresh.items).toEqual([]);
        expect(await index.countActive(['ws1'])).toBe(0);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given refreshWorkspace targets one workspace, then only that workspace directory is enumerated', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'a');
        seedSession(dir, 'ws2', 's2', 'b');
        await index.reconcile(source);
        const baseline = { ...counts };

        await index.refreshWorkspace(['ws1']);

        // ws1 was enumerated exactly once by the refresh; ws2 was never touched.
        expect(counts.listSessionIds - baseline.listSessionIds).toBe(1);
        expect(counts.statWorkspaceMtime).toBe(baseline.statWorkspaceMtime);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });

    it('given refreshWorkspace runs concurrently twice, then refresh convergences coalesce into one enumeration', async () => {
      const { dir, index, counts, source, ix, store } = buildWarmHarness();
      try {
        seedSession(dir, 'ws1', 's1', 'a');
        await index.reconcile(source);
        const baseline = { ...counts };

        const [a, b] = await Promise.all([
          index.refreshWorkspace(['ws1']),
          index.refreshWorkspace(['ws1']),
        ]);
        expect(a.inserted + a.removed).toBe(0);
        expect(b.inserted + b.removed).toBe(0);
        expect(counts.listSessionIds - baseline.listSessionIds).toBeLessThanOrEqual(1);
      } finally {
        ix.dispose();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      }
    });
  });
});
