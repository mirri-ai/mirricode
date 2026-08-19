/** 可绑定给 SQLite 语句的参数值。传 NULL 用 null，不要传 undefined。 */
export type SqlValue = null | number | bigint | string | Uint8Array;

export interface SqlRunResult {
  readonly changes: number | bigint;
  readonly lastInsertRowid: number | bigint;
}

/** 预编译语句句柄，仅暴露位置参数绑定。 */
export interface SqlStatement {
  run(...params: SqlValue[]): SqlRunResult;
  get(...params: SqlValue[]): unknown | undefined;
  all(...params: SqlValue[]): unknown[];
}

/** SQLite 连接的最小可移植面（同步）。业务代码只依赖本接口，不直接 import 引擎产物。 */
export interface SqliteDriver {
  exec(sql: string): void;
  prepare(sql: string): SqlStatement;
  close(): void;
}
