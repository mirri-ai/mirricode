import { DatabaseSync } from 'node:sqlite';
import { dirname } from 'node:path';
import { mkdirSync } from 'node:fs';
import { join } from 'pathe';
import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import type { SessionSummary } from '#/app/sessionIndex/sessionIndex';

export const ISqliteSessionStore: ServiceIdentifier<SqliteSessionStore> =
  createDecorator<SqliteSessionStore>('sqliteSessionStore');

/** Wait this long for another process to release the write lock before failing. */
const BUSY_TIMEOUT_MS = 5_000;

const SCHEMA = `
  CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY NOT NULL,
    workspaceId TEXT NOT NULL,
    cwd         TEXT,
    title       TEXT,
    lastPrompt  TEXT,
    createdAt   INTEGER NOT NULL,
    updatedAt   INTEGER NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0,
    custom      TEXT,
    stateMtime  INTEGER
  );
  CREATE INDEX IF NOT EXISTS idx_sessions_ws_updated
    ON sessions (workspaceId, updatedAt DESC);
`;

type SessionRow = {
  id: string; workspaceId: string; cwd: string | null; title: string | null;
  lastPrompt: string | null; createdAt: number; updatedAt: number;
  archived: number; custom: string | null; stateMtime: number | null;
};

function rowToSummary(row: SessionRow): SessionSummary {
  return {
    id: row.id,
    workspaceId: row.workspaceId,
    cwd: row.cwd ?? undefined,
    title: row.title ?? undefined,
    lastPrompt: row.lastPrompt ?? undefined,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
    archived: row.archived === 1,
    custom: row.custom ? (() => { try { return JSON.parse(row.custom); } catch { return undefined; } })() : undefined,
  };
}

/** Escape SQLite LIKE wildcards so a literal value is matched exactly. */
function escapeLikePattern(value: string): string {
  return value.replace(/\\/g, '\\\\').replace(/%/g, '\\%').replace(/_/g, '\\_');
}

/**
 * Check whether the `stateMtime` column already exists in the sessions table.
 * Returns true if the column is present, false otherwise.
 */
function hasStateMtimeColumn(db: DatabaseSync): boolean {
  const cols = db.prepare('PRAGMA table_info(sessions)').all() as { name: string }[];
  return cols.some(c => c.name === 'stateMtime');
}

export class SqliteSessionStore {
  declare readonly _serviceBrand: undefined;
  private db: DatabaseSync | undefined;
  private dbPath: string;

  /**
   * DI constructor: resolves dbPath from `IBootstrapService.cacheDir`.
   * For tests that construct directly with a path, use the static
   * `SqliteSessionStore.createForPath(dbPath)` factory — it bypasses DI
   * injection and keeps the existing test surface intact.
   */
  constructor(@IBootstrapService bootstrap: IBootstrapService) {
    this.dbPath = join(bootstrap.cacheDir, 'session-index.db');
  }

  /** Test-only factory: create a store with an explicit dbPath, bypassing DI. */
  static createForPath(dbPath: string): SqliteSessionStore {
    const store = Object.create(SqliteSessionStore.prototype) as SqliteSessionStore;
    store.dbPath = dbPath;
    store.db = undefined;
    return store;
  }
  open(): void {
    if (this.db) return;
    // Ensure the parent cache directory exists: on a fresh homeDir the
    // directory may not be created yet, and DatabaseSync aborts with
    // "unable to open database file" otherwise — silently degrading to the
    // FileSessionIndex fallback for the whole process lifetime.
    mkdirSync(dirname(this.dbPath), { recursive: true });
    this.db = new DatabaseSync(this.dbPath);
    // WAL allows concurrent readers to proceed while a writer holds the
    // write lock across processes; with the default rollback journal, a
    // second process (e.g. the Desktop app and a dev server sharing one
    // homeDir) blocks the whole event loop waiting for the journal lock.
    // `busy_timeout` turns a transient lock into a short wait instead of an
    // immediate `SQLITE_BUSY` error. Harmless for the single-process case.
    this.db.exec('PRAGMA journal_mode = WAL;');
    this.db.exec(`PRAGMA busy_timeout = ${BUSY_TIMEOUT_MS};`);
    // Fold any leftover WAL from a previous process (crashed / force-killed
    // server) into the main database now, so reads on a cold cache do not
    // replay a multi-MB wal on the first request. This is the startup
    // checkpoint that graceful shutdown would otherwise do.
    try {
      this.db.exec('PRAGMA wal_checkpoint(TRUNCATE);');
    } catch {
      // checkpoint is best-effort; a busy db skips it and performs normally
    }
    this.db.exec(SCHEMA);
    // Migration: add stateMtime column to existing databases that lack it
    if (!hasStateMtimeColumn(this.db)) {
      this.db.exec('ALTER TABLE sessions ADD COLUMN stateMtime INTEGER');
    }
  }
  close(): void {
    if (this.db) { this.db.close(); this.db = undefined; }
  }
  get(id: string): SessionSummary | undefined {
    const row = this.db!.prepare('SELECT * FROM sessions WHERE id = ?').get(id) as SessionRow | undefined;
    return row ? rowToSummary(row) : undefined;
  }
  upsert(s: SessionSummary): void {
    this.db!.prepare(
      `INSERT INTO sessions (id, workspaceId, cwd, title, lastPrompt, createdAt, updatedAt, archived, custom, stateMtime)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, (SELECT stateMtime FROM sessions WHERE id = ?))
       ON CONFLICT(id) DO UPDATE SET
         workspaceId=excluded.workspaceId, cwd=excluded.cwd, title=excluded.title,
         lastPrompt=excluded.lastPrompt, createdAt=excluded.createdAt,
         updatedAt=excluded.updatedAt, archived=excluded.archived, custom=excluded.custom,
         stateMtime=excluded.stateMtime`,
    ).run(s.id, s.workspaceId, s.cwd ?? null, s.title ?? null, s.lastPrompt ?? null,
          s.createdAt, s.updatedAt, s.archived ? 1 : 0, s.custom ? JSON.stringify(s.custom) : null,
          s.id);
  }
  delete(id: string): void {
    this.db!.prepare('DELETE FROM sessions WHERE id = ?').run(id);
  }
  list(opts: {
    workspaceIds?: readonly string[]; includeArchived?: boolean;
    childOf?: string; limit?: number; offset?: number;
  }): readonly SessionSummary[] {
    const where: string[] = [];
    const args: (string | number)[] = [];
    if (opts.workspaceIds && opts.workspaceIds.length > 0) {
      where.push(`workspaceId IN (${opts.workspaceIds.map(() => '?').join(',')})`);
      args.push(...opts.workspaceIds);
    }
    if (opts.includeArchived !== true) where.push('archived = 0');
    if (opts.childOf !== undefined) { where.push(`custom LIKE ? ESCAPE '\\'`); args.push(`%"parent_session_id":"${escapeLikePattern(opts.childOf)}"%`); }
    let sql = 'SELECT * FROM sessions';
    if (where.length > 0) sql += ` WHERE ${where.join(' AND ')}`;
    sql += ' ORDER BY updatedAt DESC, id DESC';
    if (opts.limit !== undefined) { sql += ' LIMIT ?'; args.push(opts.limit); }
    if (opts.offset !== undefined) { sql += ' OFFSET ?'; args.push(opts.offset); }
    return (this.db!.prepare(sql).all(...args) as SessionRow[]).map(rowToSummary);
  }

  count(workspaceIds?: readonly string[]): number {
    const where: string[] = ['archived = 0'];
    const args: string[] = [];
    if (workspaceIds && workspaceIds.length > 0) {
      where.push(`workspaceId IN (${workspaceIds.map(() => '?').join(',')})`);
      args.push(...workspaceIds);
    }
    const row = this.db!.prepare(`SELECT COUNT(*) AS n FROM sessions WHERE ${where.join(' AND ')}`).get(...args) as { n: number };
    return Number(row.n);
  }

  listWorkspaceIds(): readonly string[] {
    return (this.db!.prepare('SELECT DISTINCT workspaceId AS w FROM sessions').all() as { w: string }[]).map(r => r.w);
  }

  listSessionIds(workspaceId: string): readonly string[] {
    return (this.db!.prepare('SELECT id FROM sessions WHERE workspaceId = ?').all(workspaceId) as { id: string }[]).map(r => r.id);
  }

  listAllIds(): readonly string[] {
    return (this.db!.prepare('SELECT id FROM sessions').all() as { id: string }[]).map(r => r.id);
  }

  upsertAll(sums: readonly SessionSummary[]): void {
    for (const s of sums) this.upsert(s);
  }

  /** Get the stored stateMtime for a session (undefined if row missing or mtime not set). */
  getStateMtime(id: string): number | undefined {
    const row = this.db!.prepare('SELECT stateMtime FROM sessions WHERE id = ?').get(id) as { stateMtime: number | null } | undefined;
    if (row === undefined) return undefined;
    return row.stateMtime ?? undefined;
  }

  /** Set the stateMtime for a session. No-op if the row does not exist. */
  setStateMtime(id: string, mtimeMs: number): void {
    this.db!.prepare('UPDATE sessions SET stateMtime = ? WHERE id = ?').run(mtimeMs, id);
  }

  /** List all rows with id, workspaceId, and stateMtime for incremental reconcile. */
  listAllRows(): readonly { id: string; workspaceId: string; stateMtime: number | undefined }[] {
    const rows = this.db!.prepare('SELECT id, workspaceId, stateMtime FROM sessions').all() as { id: string; workspaceId: string; stateMtime: number | null }[];
    return rows.map(r => ({ id: r.id, workspaceId: r.workspaceId, stateMtime: r.stateMtime ?? undefined }));
  }

  /** Rows belonging to one workspace — the scoped read for `refreshWorkspace`, so the converge and its orphan pass do not sweep unrelated workspaces' rows. */
  listRowsByWorkspace(workspaceId: string): readonly { id: string; stateMtime: number | undefined }[] {
    const rows = this.db!.prepare('SELECT id, stateMtime FROM sessions WHERE workspaceId = ?').all(workspaceId) as { id: string; stateMtime: number | null }[];
    return rows.map(r => ({ id: r.id, stateMtime: r.stateMtime ?? undefined }));
  }
}

registerScopedService(
  LifecycleScope.App,
  ISqliteSessionStore,
  SqliteSessionStore,
  ScopeActivation.OnScopeCreated,
  'sqliteSessionStore',
);
