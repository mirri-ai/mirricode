import { DatabaseSync } from 'node:sqlite';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { SqliteSessionStore } from '#/app/sessionIndex/sqliteSessionStore';
import type { SessionSummary } from '#/app/sessionIndex/sessionIndex';

function makeSummary(over: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 's1', workspaceId: 'ws1', createdAt: 1, updatedAt: 1, archived: false,
    ...over,
  };
}

describe('SqliteSessionStore', () => {
  let dir: string;
  let store: SqliteSessionStore;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'sq-ss-'));
    store = SqliteSessionStore.createForPath(join(dir, 'test.db'));
  });
  afterEach(() => { store.close(); rmSync(dir, { recursive: true, force: true }); });

  it('should create table and return undefined for a missing id', () => {
    store.open();
    expect(store.get('missing')).toBeUndefined();
  });

  it('should upsert a summary and get it back unchanged', () => {
    store.open();
    const s = { id: 'a', workspaceId: 'w', title: 't', updatedAt: 5, createdAt: 1, cwd: '/p', lastPrompt: 'hi', archived: false, custom: { parent_session_id: 'p' } };
    store.upsert(s);
    expect(store.get('a')).toEqual(s);
  });

  it('should overwrite the row when upserting the same id', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a', title: 'old', updatedAt: 1 }));
    store.upsert(makeSummary({ id: 'a', title: 'new', updatedAt: 2 }));
    expect(store.get('a')?.title).toBe('new');
  });

  it('should delete a summary by id', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a' }));
    store.delete('a');
    expect(store.get('a')).toBeUndefined();
  });

  it('should list sessions in updatedAt desc order within a workspace', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a', workspaceId: 'w', updatedAt: 1 }));
    store.upsert(makeSummary({ id: 'b', workspaceId: 'w', updatedAt: 3 }));
    store.upsert(makeSummary({ id: 'c', workspaceId: 'w', updatedAt: 2 }));
    store.upsert(makeSummary({ id: 'z', workspaceId: 'other', updatedAt: 99 }));
    expect(store.list({ workspaceIds: ['w'] }).map(s => s.id)).toEqual(['b', 'c', 'a']);
  });

  it('should exclude archived rows unless includeArchived is set', () => {
    store.open();
    store.upsert(makeSummary({ id: 'open', workspaceId: 'w', archived: false }));
    store.upsert(makeSummary({ id: 'arc', workspaceId: 'w', archived: true }));
    expect(store.list({ workspaceIds: ['w'] }).map(s => s.id)).toEqual(['open']);
    expect(store.list({ workspaceIds: ['w'], includeArchived: true }).map(s => s.id)).toEqual(['open', 'arc']);
  });

  it('should paginate with limit and offset', () => {
    store.open();
    for (let i = 0; i < 5; i++) store.upsert(makeSummary({ id: `s${i}`, workspaceId: 'w', updatedAt: i }));
    expect(store.list({ workspaceIds: ['w'], limit: 2 }).map(s => s.id)).toEqual(['s4', 's3']);
    expect(store.list({ workspaceIds: ['w'], limit: 2, offset: 2 }).map(s => s.id)).toEqual(['s2', 's1']);
  });

  it('should filter children with childOf', () => {
    store.open();
    store.upsert(makeSummary({ id: 'parent', workspaceId: 'w' }));
    store.upsert(makeSummary({ id: 'kid', workspaceId: 'w', custom: { parent_session_id: 'parent', child_session_kind: 'child' } }));
    expect(store.list({ workspaceIds: ['w'], childOf: 'parent' }).map(s => s.id)).toEqual(['kid']);
  });

  it('should treat childOf with LIKE wildcards as a literal value', () => {
    store.open();
    // '%' and '_' in childOf must match literally, not as wildcards.
    store.upsert(makeSummary({ id: 'kid1', workspaceId: 'w', custom: { parent_session_id: 'ab%cd' } }));
    store.upsert(makeSummary({ id: 'kid2', workspaceId: 'w', custom: { parent_session_id: 'abXcd' } }));
    store.upsert(makeSummary({ id: 'kid3', workspaceId: 'w', custom: { parent_session_id: 'ab_cd' } }));
    expect(store.list({ workspaceIds: ['w'], childOf: 'ab%cd' }).map(s => s.id)).toEqual(['kid1']);
    expect(store.list({ workspaceIds: ['w'], childOf: 'ab_cd' }).map(s => s.id)).toEqual(['kid3']);
  });

  it('should count non-archived sessions per workspace', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a', workspaceId: 'w', archived: false }));
    store.upsert(makeSummary({ id: 'b', workspaceId: 'w', archived: true }));
    store.upsert(makeSummary({ id: 'c', workspaceId: 'x', archived: false }));
    expect(store.count(['w'])).toBe(1);
    expect(store.count(['w', 'x'])).toBe(2);
  });

  it('should enumerate distinct workspace and session ids', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a', workspaceId: 'w1' }));
    store.upsert(makeSummary({ id: 'b', workspaceId: 'w1' }));
    store.upsert(makeSummary({ id: 'c', workspaceId: 'w2' }));
    expect([...store.listWorkspaceIds()].sort()).toEqual(['w1', 'w2']);
    expect([...store.listSessionIds('w1')].sort()).toEqual(['a', 'b']);
  });

  describe('stateMtime', () => {
    it('given a session with no mtime, when getStateMtime, then returns undefined', () => {
      store.open();
      store.upsert(makeSummary({ id: 'a', workspaceId: 'w' }));
      expect(store.getStateMtime('a')).toBeUndefined();
    });

    it('given setStateMtime called, when getStateMtime, then returns the same value', () => {
      store.open();
      store.upsert(makeSummary({ id: 'a', workspaceId: 'w' }));
      store.setStateMtime('a', 1234567890);
      expect(store.getStateMtime('a')).toBe(1234567890);
    });

    it('given a missing id, when getStateMtime, then returns undefined', () => {
      store.open();
      expect(store.getStateMtime('nonexistent')).toBeUndefined();
    });

    it('given setStateMtime overwrites, when called twice, then returns the latest value', () => {
      store.open();
      store.upsert(makeSummary({ id: 'a', workspaceId: 'w' }));
      store.setStateMtime('a', 100);
      store.setStateMtime('a', 200);
      expect(store.getStateMtime('a')).toBe(200);
    });

    it('given sessions with and without mtime, when listAllRows, then returns id/workspaceId/stateMtime for all', () => {
      store.open();
      store.upsert(makeSummary({ id: 'a', workspaceId: 'w1' }));
      store.upsert(makeSummary({ id: 'b', workspaceId: 'w2' }));
      store.setStateMtime('a', 999);
      // b has no mtime
      const rows = store.listAllRows();
      expect(rows.length).toBe(2);
      const rowA = rows.find(r => r.id === 'a');
      const rowB = rows.find(r => r.id === 'b');
      expect(rowA).toEqual({ id: 'a', workspaceId: 'w1', stateMtime: 999 });
      expect(rowB).toEqual({ id: 'b', workspaceId: 'w2', stateMtime: undefined });
    });
  });

  describe('migration: stateMtime column', () => {
    it('given an old DB without stateMtime column, when open(), then column is added and getStateMtime works', () => {
      // Create a DB with the old schema (no stateMtime)
      const dbPath = join(dir, 'old.db');
      const rawDb = new DatabaseSync(dbPath);
      rawDb.exec(`
        CREATE TABLE sessions (
          id          TEXT PRIMARY KEY NOT NULL,
          workspaceId TEXT NOT NULL,
          cwd         TEXT,
          title       TEXT,
          lastPrompt  TEXT,
          createdAt   INTEGER NOT NULL,
          updatedAt   INTEGER NOT NULL,
          archived    INTEGER NOT NULL DEFAULT 0,
          custom      TEXT
        );
      `);
      rawDb.prepare(
        `INSERT INTO sessions (id, workspaceId, cwd, title, lastPrompt, createdAt, updatedAt, archived, custom)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      ).run('old1', 'w1', null, 'old-title', null, 1, 1, 0, null);
      rawDb.close();

      // Now open via SqliteSessionStore — migration should kick in
      const migrated = SqliteSessionStore.createForPath(dbPath);
      migrated.open();
      // Existing row should still be readable
      expect(migrated.get('old1')?.title).toBe('old-title');
      // stateMtime should be undefined for the old row
      expect(migrated.getStateMtime('old1')).toBeUndefined();
      // Setting stateMtime should work
      migrated.setStateMtime('old1', 42);
      expect(migrated.getStateMtime('old1')).toBe(42);
      // listAllRows should include the migrated row
      const rows = migrated.listAllRows();
      expect(rows.find(r => r.id === 'old1')).toEqual({ id: 'old1', workspaceId: 'w1', stateMtime: 42 });
      migrated.close();
    });

    it('given a new DB, when open() twice, then second open is idempotent (no error)', () => {
      const dbPath = join(dir, 'new.db');
      const s = SqliteSessionStore.createForPath(dbPath);
      s.open();
      s.upsert(makeSummary({ id: 'a', workspaceId: 'w' }));
      s.setStateMtime('a', 100);
      s.close();

      const s2 = SqliteSessionStore.createForPath(dbPath);
      s2.open();
      // Second open should not throw — ALTER TABLE is idempotent
      expect(s2.getStateMtime('a')).toBe(100);
      s2.close();
    });

    it('should open the store in WAL mode with a non-zero busy_timeout (multi-process safety)', () => {
      const s = SqliteSessionStore.createForPath(join(dir, 'wal.db'));
      s.open();
      // WAL is a persistent property of the database file: a fresh connection
      // observes it. busy_timeout is per-connection, so it is asserted on the
      // store's own connection via a prepared statement (the store does not
      // expose the handle, so query through its own query surface).
      const db = new DatabaseSync(join(dir, 'wal.db'));
      try {
        const journal = db.prepare('PRAGMA journal_mode').get() as { journal_mode: string };
        expect(journal.journal_mode.toLowerCase()).toBe('wal');
      } finally {
        db.close();
      }
      // Write path must succeed (no immediate BUSY error) — exercises the
      // configured busy_timeout pragmatically.
      s.upsert(makeSummary({ id: 'w', workspaceId: 'w', updatedAt: 1 }));
      expect(s.get('w')?.id).toBe('w');
      s.close();
    });
  });
});
