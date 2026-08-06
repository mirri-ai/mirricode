import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore } from '#/_base/di/lifecycle';
import { ServiceCollection } from '#/_base/di/serviceCollection';
import { TestInstantiationService } from '#/_base/di/test';
import { IFlagService } from '#/app/flag/flag';
import { ILogService } from '#/_base/log/log';
import { ISqliteSessionStore, SqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';
import { ISessionContext, makeSessionContext } from '#/session/sessionContext/sessionContext';
import { ISessionMetadata } from '#/session/sessionMetadata/sessionMetadata';
import { SessionMetadata } from '#/session/sessionMetadata/sessionMetadataService';
import { ISessionStateService } from '#/session/state/sessionState';
import { SessionStateService } from '#/session/state/sessionStateService';
import { JsonAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { InMemoryStorageService } from '#/persistence/backends/memory/inMemoryStorageService';
import { IQueryStore } from '#/persistence/interface/queryStore';

import { stubFlag } from '../../app/flag/stubs';
import { stubLog } from '../../_base/log/stubs';
import { stubQueryStore } from '../../persistence/interface/stubs';

const META_SCOPE = 'sessions/wd_test/s1/session-meta';

function makeContext(): ISessionContext {
  return makeSessionContext({
    sessionId: 's1',
    workspaceId: 'wd_test',
    sessionDir: '/tmp/sessions/wd_test/s1',
    sessionScope: 'sessions/wd_test/s1',
    metaScope: META_SCOPE,
    cwd: '/tmp/work',
  });
}

describe('SessionMetadata write-through to SqliteSessionStore', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let dir: string;
  let sqliteStore: SqliteSessionStore;

  beforeEach(() => {
    disposables = new DisposableStore();
    ix = disposables.add(new TestInstantiationService());
    ix.stub(ILogService, stubLog());
    ix.stub(ISessionContext, makeContext());
    ix.stub(IQueryStore, stubQueryStore());
    ix.stub(IFlagService, stubFlag(false));
    ix.set(ISessionStateService, new SyncDescriptor(SessionStateService));
    ix.set(IFileSystemStorageService, new SyncDescriptor(InMemoryStorageService));
    ix.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
    ix.set(ISessionMetadata, new SyncDescriptor(SessionMetadata));

    // Create a real SqliteSessionStore for the write-through integration test
    dir = mkdtempSync(join(tmpdir(), 'sq-write-through-'));
    sqliteStore = SqliteSessionStore.createForPath(join(dir, 'test.db'));
    ix.stub(ISqliteSessionStore, sqliteStore);
  });

  afterEach(() => {
    disposables.dispose();
    sqliteStore.close();
    rmSync(dir, { recursive: true, force: true });
  });

  it('should write-through summary into SqliteSessionStore on metadata update', async () => {
    const meta = ix.get(ISessionMetadata);
    await meta.ready;

    // The store is not yet open — first update should open it and write
    await meta.update({ title: 'hello' });

    // Verify the summary appears in the shared SQLite store
    const summary = sqliteStore.get('s1');
    expect(summary).toBeDefined();
    expect(summary!.id).toBe('s1');
    expect(summary!.workspaceId).toBe('wd_test');
    expect(summary!.title).toBe('hello');
    expect(summary!.cwd).toBe('/tmp/work');
    expect(summary!.archived).toBe(false);
  });

  it('should update the SQLite store on subsequent updates', async () => {
    const meta = ix.get(ISessionMetadata);
    await meta.ready;

    await meta.update({ title: 'first' });
    expect(sqliteStore.get('s1')?.title).toBe('first');

    await meta.update({ title: 'second' });
    const summary = sqliteStore.get('s1');
    expect(summary?.title).toBe('second');
    // updatedAt should have advanced
    expect(summary!.updatedAt).toBeGreaterThan(0);
  });

  it('should mirror archived flag correctly', async () => {
    const meta = ix.get(ISessionMetadata);
    await meta.ready;

    await meta.setArchived(true);
    expect(sqliteStore.get('s1')?.archived).toBe(true);

    await meta.setArchived(false);
    expect(sqliteStore.get('s1')?.archived).toBe(false);
  });

  it('should mirror custom field', async () => {
    const meta = ix.get(ISessionMetadata);
    await meta.ready;

    await meta.update({ custom: { parent_session_id: 'p1', child_session_kind: 'child' } });
    const summary = sqliteStore.get('s1');
    expect(summary?.custom).toEqual({ parent_session_id: 'p1', child_session_kind: 'child' });
  });

  it('write-through failure: update still resolves and state.json is updated when SQLite open fails', async () => {
    // Create a store that will fail on open (invalid path) and close safely
    const badStore = SqliteSessionStore.createForPath('/nonexistent/deep/path/fail.db');
    ix.stub(ISqliteSessionStore, badStore);

    const warnSpy: Array<{ args: unknown[] }> = [];
    const capturingLog = {
      ...stubLog(),
      warn: (...args: unknown[]) => { warnSpy.push({ args }); },
    };
    ix.stub(ILogService, capturingLog);

    const meta = ix.get(ISessionMetadata);
    await meta.ready;

    // Update should resolve without throwing even though SQLite is unavailable
    await expect(meta.update({ title: 'still-works' })).resolves.toBeUndefined();

    // state.json should have been updated (read back from the document store)
    const data = await meta.read();
    expect(data.title).toBe('still-works');

    // A warning should have been logged for the SQLite mirror failure
    expect(warnSpy.length).toBeGreaterThanOrEqual(1);
    expect(warnSpy.some(w => String(w.args).includes('sqlite'))).toBe(true);

    // Clean up: badStore.close() is a no-op since db was never opened
    badStore.close();
  });

  it('shared store: two SessionMetadata instances write to the same SQLite', async () => {
    // First session
    const meta1 = ix.get(ISessionMetadata);
    await meta1.ready;
    await meta1.update({ title: 'session-one' });

    // Simulate a second session context
    const ctx2Override = makeSessionContext({
      sessionId: 's2',
      workspaceId: 'wd_test',
      sessionDir: '/tmp/sessions/wd_test/s2',
      sessionScope: 'sessions/wd_test/s2',
      metaScope: 'sessions/wd_test/s2/session-meta',
      cwd: '/tmp/work',
    });

    // Create a second SessionMetadata sharing the same ISqliteSessionStore
    const ix2 = disposables.add(new TestInstantiationService());
    ix2.stub(ILogService, stubLog());
    ix2.stub(ISessionContext, ctx2Override);
    ix2.stub(IQueryStore, stubQueryStore());
    ix2.stub(IFlagService, stubFlag(false));
    ix2.set(ISessionStateService, new SyncDescriptor(SessionStateService));
    ix2.set(IFileSystemStorageService, new SyncDescriptor(InMemoryStorageService));
    ix2.set(IAtomicDocumentStore, new SyncDescriptor(JsonAtomicDocumentStore));
    ix2.stub(ISqliteSessionStore, sqliteStore);
    ix2.set(ISessionMetadata, new SyncDescriptor(SessionMetadata));

    const meta2 = ix2.get(ISessionMetadata);
    await meta2.ready;
    await meta2.update({ title: 'session-two' });

    // Both sessions should be in the shared store
    expect(sqliteStore.get('s1')?.title).toBe('session-one');
    expect(sqliteStore.get('s2')?.title).toBe('session-two');

    // Listing should include both
    const allSessions = sqliteStore.list({ workspaceIds: ['wd_test'], includeArchived: true });
    expect(allSessions.map(s => s.id).sort()).toEqual(['s1', 's2']);
  });
});
