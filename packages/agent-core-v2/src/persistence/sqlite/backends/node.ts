import { DatabaseSync } from 'node:sqlite';
import type { StatementSync } from 'node:sqlite';
import type { SqliteDriver, SqlStatement, SqlValue } from '#/persistence/sqlite/driver';

/** 包装 node:sqlite 的 DatabaseSync，并收敛到 SqliteDriver 接口（design.md §5.2）。 */
export function createNodeSqliteDriver(path: string): SqliteDriver {
  const db = new DatabaseSync(path);
  return {
    exec(sql: string): void {
      db.exec(sql);
    },
    prepare(sql: string): SqlStatement {
      const stmt: StatementSync = db.prepare(sql);
      return {
        run(...params: SqlValue[]) { return stmt.run(...params); },
        get(...params: SqlValue[]) { return stmt.get(...params); },
        all(...params: SqlValue[]) { return stmt.all(...params); },
      };
    },
    close(): void {
      // close 幂等：node:sqlite 对已关闭句柄再 close 会抛错，先判 isOpen。
      if (db.isOpen) db.close();
    },
  };
}
