import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { openSqliteDatabase } from '#/persistence/sqlite/factory';
import type { SqliteDriver } from '#/persistence/sqlite/factory';
import { migrate, type SqlMigration } from '#/persistence/sqlite/migrate';
import { withTransaction } from '#/persistence/sqlite/transaction';

function makeDb(prefix: string): { dir: string; dbPath: string; db: SqliteDriver } {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  const dbPath = join(dir, 'test.db');
  return { dir, dbPath, db: openSqliteDatabase(dbPath) };
}

function closeAndClean(dir: string, db: SqliteDriver): void {
  db.close();
  rmSync(dir, { recursive: true, force: true });
}

describe('sqlite driver layer', () => {
  let ctx: { dir: string; dbPath: string; db: SqliteDriver };
  beforeEach(() => { ctx = makeDb('sq-drv-'); });
  afterEach(() => { closeAndClean(ctx.dir, ctx.db); });

  describe('driver (node backend via openSqliteDatabase)', () => {
    it('should create a db file on disk when opening and executing DDL', () => {
      ctx.db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)');
      expect(existsSync(ctx.dbPath)).toBe(true);
    });

    it('should apply all statements when exec receives multiple SQL statements', () => {
      ctx.db.exec('CREATE TABLE a (x INTEGER); CREATE TABLE b (y TEXT);');
      ctx.db.exec("INSERT INTO b (y) VALUES ('hi');");
      expect((ctx.db.prepare('SELECT y FROM b LIMIT 1').get() as { y: string }).y).toBe('hi');
    });

    it('should bind positional string/number/null params correctly with get/all/run', () => {
      ctx.db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, n INTEGER, u TEXT)');
      ctx.db.prepare('INSERT INTO t (s, n, u) VALUES (?, ?, ?)').run('str', 42, null);
      const row = ctx.db.prepare('SELECT s, n, u FROM t WHERE n = ?').get(42) as { s: string; n: number; u: null };
      expect(row).toEqual({ s: 'str', n: 42, u: null });
      expect(ctx.db.prepare('SELECT s FROM t').all()).toEqual([{ s: 'str' }]);
    });

    it('should return undefined from get and an empty array from all when no row matches', () => {
      ctx.db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY)');
      expect(ctx.db.prepare('SELECT * FROM t WHERE id = 1').get()).toBeUndefined();
      expect(ctx.db.prepare('SELECT * FROM t').all()).toEqual([]);
    });

    it('should report changes and lastInsertRowid when run executes an INSERT', () => {
      ctx.db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)');
      const first = ctx.db.prepare('INSERT INTO t (name) VALUES (?)').run('a');
      expect(Number(first.changes)).toBe(1);
      expect(Number(first.lastInsertRowid)).toBe(1);
      const second = ctx.db.prepare('INSERT INTO t (name) VALUES (?)').run('b');
      expect(Number(second.lastInsertRowid)).toBe(2);
    });

    it('should not throw when close is called on an already closed database', () => {
      ctx.db.close();
      expect(() => ctx.db.close()).not.toThrow();
    });
  });

  describe('migrate', () => {
    const MIGRATIONS: readonly SqlMigration[] = [
      { version: 1, statements: 'CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);' },
      { version: 2, statements: 'ALTER TABLE users ADD COLUMN email TEXT;' },
    ];

    const readUserVersion = (driver: SqliteDriver): number =>
      Number((driver.prepare('PRAGMA user_version').get() as { user_version: number }).user_version);

    it('should apply the final schema and set user_version when migrating a fresh db', () => {
      migrate(ctx.db, MIGRATIONS);
      expect(readUserVersion(ctx.db)).toBe(2);
      ctx.db.prepare('INSERT INTO users (name, email) VALUES (?, ?)').run('a', 'a@example.com');
      expect((ctx.db.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n).toBe(1);
    });

    it('should be a no-op when migrating an already-migrated db', () => {
      migrate(ctx.db, MIGRATIONS);
      expect(() => migrate(ctx.db, MIGRATIONS)).not.toThrow();
      expect(readUserVersion(ctx.db)).toBe(2);
    });

    it('should roll back the failing step and keep user_version unchanged when a statement fails midway', () => {
      const failing: readonly SqlMigration[] = [
        { version: 1, statements: 'CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT);' },
        { version: 2, statements: 'INSERT INTO t (id) VALUES (42); ALTER TABLE t ADD COLUMN x TEXT;' },
      ];
      expect(() => migrate(ctx.db, failing)).toThrow();
      expect(readUserVersion(ctx.db)).toBe(1);
      expect((ctx.db.prepare('SELECT COUNT(*) AS n FROM t').get() as { n: number }).n).toBe(0);
    });
  });

  describe('withTransaction', () => {
    beforeEach(() => {
      ctx.db.exec('CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)');
    });

    it('should roll back the writes and rethrow when the fn throws', () => {
      expect(() =>
        withTransaction(ctx.db, () => {
          ctx.db.prepare('INSERT INTO t (v) VALUES (?)').run('a');
          throw new Error('boom');
        }),
      ).toThrow('boom');
      expect((ctx.db.prepare('SELECT COUNT(*) AS n FROM t').get() as { n: number }).n).toBe(0);
    });

    it('should commit the writes when the fn succeeds', () => {
      withTransaction(ctx.db, () => {
        ctx.db.prepare('INSERT INTO t (v) VALUES (?)').run('kept');
      });
      expect((ctx.db.prepare('SELECT COUNT(*) AS n FROM t').get() as { n: number }).n).toBe(1);
    });
  });
});
