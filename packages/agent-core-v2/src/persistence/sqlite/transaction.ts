import type { SqliteDriver } from '#/persistence/sqlite/driver';

/**
 * 在单个顶层事务中执行 fn：fn 正常返回则 COMMIT，抛错则 ROLLBACK 并重抛原错误。
 * 嵌套使用不在本封装语义内（需要方自行用 SAVEPOINT）。
 */
export function withTransaction<T>(db: SqliteDriver, fn: () => T): T {
  db.exec('BEGIN');
  try {
    const result = fn();
    db.exec('COMMIT');
    return result;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}
