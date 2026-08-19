import type { SqliteDriver } from '#/persistence/sqlite/driver';

export interface SqlMigration {
  /** 迁移后应达到的 user_version。声明时按升序排列。 */
  version: number;
  /** 从上一版本升级到本版本要执行的 SQL（可多条语句）。 */
  statements: string;
}

/** 把 db 从当前 user_version 逐级升到最高已声明版本；已应用的版本跳过，幂等。 */
export function migrate(db: SqliteDriver, migrations: readonly SqlMigration[]): void {
  const current = readUserVersion(db);
  for (const m of [...migrations].sort((a, b) => a.version - b.version)) {
    if (m.version <= current) continue;
    db.exec('BEGIN');
    try {
      db.exec(m.statements);
      db.exec(`PRAGMA user_version = ${m.version}`);
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
  }
}

function readUserVersion(db: SqliteDriver): number {
  const row = db.prepare('PRAGMA user_version').get() as { user_version?: number } | undefined;
  const value = row?.user_version;
  return typeof value === 'number' && Number.isInteger(value) ? value : 0;
}
