import { createNodeSqliteDriver } from '#/persistence/sqlite/backends/node';
import type { SqliteDriver } from '#/persistence/sqlite/driver';

export type { SqliteDriver } from '#/persistence/sqlite/driver';

/** 当前进程的 JS 运行时。Bun 在 process.versions 暴露 bun 版本号，Node 没有。 */
export function detectJsRuntime(): 'node' | 'bun' {
  if (typeof process !== 'undefined' && (process.versions as { bun?: string }).bun) {
    return 'bun';
  }
  return 'node';
}

/**
 * 按当前运行时打开一个 SQLite 数据库。
 * 未来切运行时（Node → Bun）只需改这里 + 新增 backends/bun.ts。
 */
export function openSqliteDatabase(path: string): SqliteDriver {
  switch (detectJsRuntime()) {
    case 'node':
      return createNodeSqliteDriver(path);
    case 'bun':
      throw new Error('bun sqlite driver is not implemented yet (reserved: src/persistence/sqlite/backends/bun.ts)');
  }
}
