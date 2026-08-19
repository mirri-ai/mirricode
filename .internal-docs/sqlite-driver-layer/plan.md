# SQLite 驱动抽象层 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `packages/agent-core-v2` 内落地一层运行时无关的 SQLite 驱动抽象（驱动层 + 能力层），并让唯一的 SQLite 生产消费点 `SqliteSessionStore` 改经该层访问，使未来 Node → Bun 切换只改 factory/adapter。

**Architecture:** 两层：驱动层（`SqliteDriver` 接口 + `node:sqlite` adapter + 运行时探测工厂）解决运行时可移植；能力层（版本化 `migrate()` + `withTransaction()`）收敛未来功能的迁移/事务样板。业务代码（当前仅 `SqliteSessionStore`）只 import 抽象层，仓库内除 adapter 文件外不再出现 `node:sqlite`。

**Tech Stack:** TypeScript + `node:sqlite`（Node 内置，零新增依赖）；vitest（单元测试）；pnpm workspace；`#/` 包内路径别名（`"#/*": "./src/*.ts"`）。

**设计文档:** `.internal-docs/sqlite-driver-layer/design.md`

## Global Constraints

- Node `>= 24.15.0`（`node:sqlite` 可用）；pnpm `10.33.0`。
- **零新增 npm 依赖**：只用运行时内置 `node:sqlite`；不引 drizzle / libsql / better-sqlite3。`package.json` 一律不动。
- 生产与测试代码中，`node:sqlite` 只允许出现在 `packages/agent-core-v2/src/persistence/sqlite/backends/node.ts`（验收时 grep 验证）。
- 测试命名用 Given-When-Then（`should ... when ...`），描述里不提类名/函数名/内部实现细节。
- 只新增 **1 个** 测试文件（`test/persistence/sqlite/sqliteDriver.test.ts`），其余存量文件只改不改加。
- import 一律用包内别名 `#/...`（不用相对路径链）。
- **每次 `git commit` 须先向用户确认**（仓库硬规则），计划里的 commit 步骤默认只 `git add` + 展示命令，执行时先问。
- 不动 `ISessionIndex`、不动 store 的 SQL/索引/fallback 逻辑；不改 `persistence/interface/` 既有布局。

## File Structure

**新建（src）**
| 文件 | 职责 |
|------|------|
| `src/persistence/sqlite/driver.ts` | 运行时无关接口与类型：`SqliteDriver` / `SqlStatement` / `SqlRunResult` / `SqlValue` |
| `src/persistence/sqlite/backends/node.ts` | `createNodeSqliteDriver(path)`：包装 `node:sqlite` 的 `DatabaseSync` |
| `src/persistence/sqlite/backends/bun.ts` | 预留文件（仅注释，本期不实现） |
| `src/persistence/sqlite/factory.ts` | `detectJsRuntime()` + `openSqliteDatabase(path)`（唯一切换点）+ 重导出 `SqliteDriver` 类型 |
| `src/persistence/sqlite/migrate.ts` | `SqlMigration` + `migrate(db, migrations)`（基于 `PRAGMA user_version`） |
| `src/persistence/sqlite/transaction.ts` | `withTransaction(db, fn)` |

**新建测试**
| 文件 | 覆盖 |
|------|------|
| `test/persistence/sqlite/sqliteDriver.test.ts` | 驱动（factory→node adapter）、`migrate`、`withTransaction` |

**修改**
| 文件 | 改动 |
|------|------|
| `src/app/sessionIndex/sqliteSessionStore.ts` | import 换成工厂；`db` 字段类型、`open()` 构造、`hasStateMtimeColumn` 参数类型改 `SqliteDriver`（4 处局部） |
| `test/app/sessionIndex/sqliteSessionStore.test.ts` | 删 `node:sqlite` import；两处 fixture 的 `new DatabaseSync(...)` 改 `openSqliteDatabase(...)` |

---

### Task 1: 驱动层（driver + node adapter + factory + bun 预留）

**Files:**
- Create: `packages/agent-core-v2/src/persistence/sqlite/driver.ts`
- Create: `packages/agent-core-v2/src/persistence/sqlite/backends/node.ts`
- Create: `packages/agent-core-v2/src/persistence/sqlite/backends/bun.ts`
- Create: `packages/agent-core-v2/src/persistence/sqlite/factory.ts`
- Test: `packages/agent-core-v2/test/persistence/sqlite/sqliteDriver.test.ts`（Task 1 只写驱动相关的 describe 部分）

**Interfaces:**
- Consumes: 无外部队列依赖（只用 `node:sqlite` 内置模块）。
- Produces（后续任务依赖的签名）:
  - `openSqliteDatabase(path: string): SqliteDriver`（`#/persistence/sqlite/factory`）
  - `detectJsRuntime(): 'node' | 'bun'`（同文件导出）
  - `type SqliteDriver { exec(sql: string): void; prepare(sql: string): SqlStatement; close(): void }`（`#/persistence/sqlite/factory` 重导出）
  - `type SqlValue = null | number | bigint | string | Uint8Array`
  - `interface SqlStatement { run(...params: SqlValue[]): SqlRunResult; get(...params: SqlValue[]): unknown | undefined; all(...params: SqlValue[]): unknown[] }`
  - `interface SqlRunResult { readonly changes: number | bigint; readonly lastInsertRowid: number | bigint }`

- [ ] **Step 1: 写驱动相关失败测试**

创建 `packages/agent-core-v2/test/persistence/sqlite/sqliteDriver.test.ts`（先只放驱动 describe；migrate/withTransaction 的 describe 由 Task 2 追加）：

```ts
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { openSqliteDatabase } from '#/persistence/sqlite/factory';
import type { SqliteDriver } from '#/persistence/sqlite/factory';

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
});
```

- [ ] **Step 2: 运行测试确认失败**

```
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/persistence/sqlite/sqliteDriver.test.ts
```

Expected: FAIL，报 `Failed to resolve import "#/persistence/sqlite/factory"`（模块还没建）。

- [ ] **Step 3: 实现驱动层 4 个文件**

`src/persistence/sqlite/driver.ts`：

```ts
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
```

`src/persistence/sqlite/backends/node.ts`：

```ts
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
```

`src/persistence/sqlite/backends/bun.ts`（预留，仅注释）：

```ts
/**
 * Bun 运行时预留 adapter。
 *
 * 未来切 Bun 时在此实现 `createBunSqliteDriver(path: string): SqliteDriver`：
 * 包装 `bun:sqlite` 的 `Database`（`exec` / `query(...).get|all|run` / `close`），
 * 并在 `#/persistence/sqlite/factory` 的 `case 'bun'` 分支返回它。
 *
 * 注意：Node 构建下直接 `import('bun:sqlite')` 会解析失败——adapter 需由
 * factory 按需动态加载或按运行时拆产物，见 design.md §5.3 的约束说明。
 */
```

`src/persistence/sqlite/factory.ts`：

```ts
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
```

- [ ] **Step 4: 运行测试确认通过**

```
cd ~/workspace/opensource/mirricode
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/persistence/sqlite/sqliteDriver.test.ts
```

Expected: 6 个测试全 PASS。

- [ ] **Step 5: typecheck 驱动层**

```
pnpm --filter @mirri-ai/agent-core-v2 run typecheck
```

Expected: `tsc` 无错误（含新 4 文件）。注意：如报 `node:sqlite` 类型缺失，说明本地 `@types/node` 版本偏旧，需先升级 `@types/node`（需在 Task 前与用户确认，因为它属于依赖变更）。

- [ ] **Step 6: Commit（需用户确认后执行）**

```bash
git add packages/agent-core-v2/src/persistence/sqlite packages/agent-core-v2/test/persistence/sqlite
git commit -m "feat(agent-core-v2): add runtime-neutral sqlite driver layer"
```

---

### Task 2: 能力层（migrate + transaction）

**Files:**
- Create: `packages/agent-core-v2/src/persistence/sqlite/migrate.ts`
- Create: `packages/agent-core-v2/src/persistence/sqlite/transaction.ts`
- Test: `packages/agent-core-v2/test/persistence/sqlite/sqliteDriver.test.ts`（追加两个 describe）

**Interfaces:**
- Consumes: `SqliteDriver`（`#/persistence/sqlite/factory` 重导出，Task 1 产出）；`openSqliteDatabase`（测试里建库用）。
- Produces:
  - `interface SqlMigration { version: number; statements: string }`（`#/persistence/sqlite/migrate`）
  - `migrate(db: SqliteDriver, migrations: readonly SqlMigration[]): void`
  - `withTransaction<T>(db: SqliteDriver, fn: () => T): T`（`#/persistence/sqlite/transaction`）

- [ ] **Step 1: 追加失败测试**

在 `test/persistence/sqlite/sqliteDriver.test.ts` 顶部 import 追加：

```ts
import { migrate, type SqlMigration } from '#/persistence/sqlite/migrate';
import { withTransaction } from '#/persistence/sqlite/transaction';
```

在第一个 describe 闭合（最后的 `});`）之后、文件末尾追加：

```ts
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
```

（上面两个 describe 与驱动 describe 共用外层 `ctx`。）

- [ ] **Step 2: 运行确认失败**

```
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/persistence/sqlite/sqliteDriver.test.ts
```

Expected: 报 `Failed to resolve import`（migrate/transaction 不存在）或 `Cannot find module` 于测试解析阶段。

- [ ] **Step 3: 实现 migrate.ts 与 transaction.ts**

`src/persistence/sqlite/migrate.ts`：

```ts
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
```

`src/persistence/sqlite/transaction.ts`：

```ts
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
```

- [ ] **Step 4: 运行测试确认通过**

```
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/persistence/sqlite/sqliteDriver.test.ts
```

Expected: 11 个测试（6 驱动 + 3 migrate + 2 transaction）全 PASS。

- [ ] **Step 5: typecheck**

```
pnpm --filter @mirri-ai/agent-core-v2 run typecheck
```

Expected: 无错误。

- [ ] **Step 6: Commit（需用户确认）**

```bash
git add packages/agent-core-v2/src/persistence/sqlite/migrate.ts packages/agent-core-v2/src/persistence/sqlite/transaction.ts packages/agent-core-v2/test/persistence/sqlite/sqliteDriver.test.ts
git commit -m "feat(agent-core-v2): add sqlite migrate and transaction helpers"
```

---

### Task 3: `SqliteSessionStore` 与存量测试接入抽象层

**Files:**
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts`
- Modify: `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts`
- Test（复用）: `test/persistence/sqlite/sqliteDriver.test.ts` 无改动

**Interfaces:**
- Consumes: `openSqliteDatabase(path: string): SqliteDriver`、`type SqliteDriver`（`#/persistence/sqlite/factory`）。
- Produces: `SqliteSessionStore` 对外签名不变（`ISqliteSessionStore` / `createForPath` / 全部方法不变）。

- [ ] **Step 1: 改 store（4 处）**

先用 `Read` 打开 `src/app/sessionIndex/sqliteSessionStore.ts` 确认行号，然后：

1. 顶部 import：
   ```ts
   -import { DatabaseSync } from 'node:sqlite';
   +import { openSqliteDatabase, type SqliteDriver } from '#/persistence/sqlite/factory';
   ```
2. 字段类型：`private db: DatabaseSync | undefined;` → `private db: SqliteDriver | undefined;`
3. `open()` 中构造：`this.db = new DatabaseSync(this.dbPath);` → `this.db = openSqliteDatabase(this.dbPath);`
4. `hasStateMtimeColumn` 签名：`function hasStateMtimeColumn(db: DatabaseSync): boolean {` → `function hasStateMtimeColumn(db: SqliteDriver): boolean {`

- [ ] **Step 2: 改测试 fixture（2 处 + import）**

`test/app/sessionIndex/sqliteSessionStore.test.ts`：

1. 第 1 行：删 `import { DatabaseSync } from 'node:sqlite';`，新增
   ```ts
   import { openSqliteDatabase } from '#/persistence/sqlite/factory';
   ```
2. "old DB without stateMtime" 用例：`const rawDb = new DatabaseSync(dbPath);` → `const rawDb = openSqliteDatabase(dbPath);`
3. "WAL mode" 用例：`const db = new DatabaseSync(join(dir, 'wal.db'));` → `const db = openSqliteDatabase(join(dir, 'wal.db'));`（该处下方 `db.close()` 不变）

- [ ] **Step 3: 跑 store 测试**

```
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/sessionIndex/sqliteSessionStore.test.ts
```

Expected: 全 PASS（行为不变，仅换实现来源）。

- [ ] **Step 4: 验证 `node:sqlite` 引用唯一性**

```
rg -l "node:sqlite" packages/agent-core-v2/src packages/agent-core-v2/test
```

Expected: 输出**只有**`packages/agent-core-v2/src/persistence/sqlite/backends/node.ts`。

- [ ] **Step 5: typecheck + import 边界检查**

```
pnpm --filter @mirri-ai/agent-core-v2 run typecheck
pnpm --filter @mirri-ai/agent-core-v2 run lint:imports
```

Expected: 均通过（boundary 规则只针对 kosong/v1，新目录不受影响）。

- [ ] **Step 6: Commit（需用户确认）**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts
git commit -m "refactor(agent-core-v2): route sqlite session store through driver layer"
```

---

### Task 4: 收尾验证与变更记录

**Files:** 无新增。

- [ ] **Step 1: 跑 session index 全量相关测试**

```
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/persistence/sqlite/sqliteDriver.test.ts test/app/sessionIndex
```

Expected: 全部 PASS（含 `sqliteSessionIndex.test.ts` / `sqliteDiskSource.test.ts` / `sqliteWriteThrough.test.ts` 等存量）。

- [ ] **Step 2: 最终 grep 复核**

```
rg -n "node:sqlite" packages/agent-core-v2/src packages/agent-core-v2/test
```

Expected: 唯一命中 `src/persistence/sqlite/backends/node.ts` 的 import/使用。

- [ ] **Step 3: 变更记录（changeset，按仓库 gen-changesets 规则）**

`agent-core-v2` 为 `private: true` 工作区包；若它不在发布序列则跳过 changeset，否则按 `.agents/skills/gen-changesets/SKILL.md` 生成。若产生 `major` 判断必须停下先与用户确认（本改动为内部重构、对外零行为变化，预期 `minor`/`patch`）。

- [ ] **Step 4: （可选）提交收尾**

若用户要求提交改动，汇总：
```bash
git add .changeset
git commit -m "chore: changeset for sqlite driver layer"
```
（同样需用户确认后执行。）

---

## Self-Review 记录（计划作者自查）

- **Spec 覆盖**：design.md §5 的 6 个新文件 → Task1/2 全覆盖；§5.5 store 改造 → Task3；§7 测试清单逐条落入 Task1/2/3 的测试代码；§9 验收 1/2/3 → Task3 Step4/4 与 Task3/Task4 验证；§10 演进出口不改动（Bun 仍抛错）。
- **占位符扫描**：无 TBD/TODO；每个实现步骤都含完整代码。
- **类型一致性**：`openSqliteDatabase` / `SqliteDriver` / `SqlStatement` / `SqlRunResult` / `SqlValue` / `migrate` / `SqlMigration` / `withTransaction` 在全部任务中签名一致；factory 重导出 `SqliteDriver` 供 store 与测试两类 import 都成立。
