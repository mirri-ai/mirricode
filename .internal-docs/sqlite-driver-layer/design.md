# SQLite 驱动抽象层（sqlite-driver-layer）— Internal Design

> **Status**: 待评审（brainstorming 产出，尚未实现）
> **Date**: 2026-08-17
> **范围**: `packages/agent-core-v2`
> **依赖**: 零新增（`node:sqlite` / `bun:sqlite` 均为运行时内置）

---

## 1. 背景与目标

仓库目前只在 session index 一处使用了 SQLite（通过运行时内置 `node:sqlite`）。
未来会有更多功能使用 SQLite 做本地持久化，用户希望加一层抽象接口层，
让所有用 SQLite 的地方都通过这一层适配，未来**切换运行时（Node → Bun）时
只需要修改这一层**就能完成适配，收缩影响范围。

设计目标：

1. **运行时无感**：业务代码只依赖本层接口，不直接 import `node:sqlite` / `bun:sqlite`。
2. **切换成本收敛**：未来切 Bun 只新增一个 adapter 文件 + 工厂分支，其余零改动。
3. **面向未来功能的公共能力**：不止"能跑"，还提供迁移、事务这类每个 SQLite
   功能都会用到的样板能力，避免未来各功能各写各的。
4. **零新增依赖**：延续现有 "只用运行时内置 `node:sqlite`" 的原则。

## 2. 现状调查（全部 SQLite 触点）

| 文件 | 角色 | 是否走抽象层 |
|------|------|-------------|
| `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts` | **唯一生产触点**：直接 `new DatabaseSync(...)`，用 `exec` / `prepare().get/all/run` 维护 session 索引 | 改造 |
| `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts` | 测试 fixture 里直接 `new DatabaseSync(...)` 建库/校验 | 改为经抽象层 |

**间接消费者（不接触驱动、无需改动）**：
`SqliteSessionIndex`（经 DI `ISqliteSessionStore` 消费 store 方法）、
`sessionIndexService.ts`（`ISessionIndex` 绑定 `SqliteSessionIndex`）、
以及 `ISessionIndex` 的下游（`sessionLifecycleService` / `workspaceSessionsService` /
`messageLegacyService` / `workspaceService` / `sessionExportService` /
`sessionLookup` 等）。

**非驱动引用（不在本层范围）**：
- `file-type.ts` 系列只是把 `.sqlite/.db/.db3` 当文件扩展名识别；
- `packages/minidb` 是自研 KV，只是"类 SQLite WAL"的类比；
- 各处注释/文档中的 "sqlite" 字样。

→ 结论：**真正要抽象的驱动面只有 1 个生产文件 + 1 个测试文件**。

## 3. 市场调研结论（为什么自研而不是引入现成产品）

- **没有现成产品把 `node:sqlite` 与 `bun:sqlite` 两个内置模块桥接**：两者是各运行时
  内置模块，互不互通，也没有第三方包装它们俩。
- 接近的方向有二，但均不直接命中：
  - **查询构建器多驱动（Drizzle）**：官方 SQLite 驱动清单为
    `@libsql/client` / `node:sqlite` / `better-sqlite3`，无独立 `bun:sqlite`
    官方驱动；其 "Bun SQL" 文档实为 Bun **PostgreSQL** 绑定。它是"写法层"
    抽象，不是驱动层抽象，且引入第三方依赖 + `drizzle-kit` 迁移工具链。
  - **跨平台客户端（libSQL）**：`@libsql/client` 以 `node/web/http/ws/wasm`
    变体服务多运行时，但 API 形状（异步 `execute`）与 `bun:sqlite` / `node:sqlite`
    的同步 `prepare/get/all/run` 完全不同，属于换引擎而非换驱动。
- **关键事实**：`node:sqlite`、`bun:sqlite`、`better-sqlite3` 的 API 同源于
  better-sqlite3 风格（同步、`prepare → get/all/run` + `exec`），适配一个薄接口
  成本极低（几十行）。

→ 结论：自研薄驱动接口 + adapter 最便宜；能力层（迁移、事务）借鉴市场产品的
定位自行实现。

## 4. 架构

分两层，全部收在 `packages/agent-core-v2/src/persistence/sqlite/` 下：

```
                        业务功能（当前: SqliteSessionStore）
                                     │
            ┌────────────────────────┴────────────────────────┐
            │  能力层（运行时无关，纯 JS）                       │
            │  migrate.ts     版本化 schema 迁移（PRAGMA user_version）│
            │  transaction.ts withTransaction(db, fn)         │
            └────────────────────────┬────────────────────────┘
            ┌────────────────────────┴────────────────────────┐
            │  驱动层（运行时相关点）                            │
            │  driver.ts    SqliteDriver 接口 + 类型            │
            │  factory.ts   openSqliteDatabase()：运行时探测+选择 │
            │  backends/    node.ts（实现）/ bun.ts（预留）      │
            └──────────────────────────────────────────────────┘
                                     │ 切换运行时只改这里
                                      ▼
                          node:sqlite  /  bun:sqlite
```

**数据流**：所有 SQLite 读取/写入必须经 `SqliteSessionStore` → `SqliteDriver`
接口 → adapter → 引擎。禁止业务模块直接 import 驱动产物。

## 5. 文件规划

```
src/persistence/sqlite/
  driver.ts          # 运行时无关的驱动接口 + 类型
  backends/node.ts   # node:sqlite 适配器（工厂函数 createNodeSqliteDriver）
  backends/bun.ts    # 预留文件（本阶段不实现，仅留 import 位 + 注释）
  factory.ts         # openSqliteDatabase(path)：探测运行时 + 唯一切换点
  migrate.ts         # migrate(db, migrations)：版本化迁移
  transaction.ts     # withTransaction(db, fn)：BEGIN/COMMIT/ROLLBACK 封装
```

### 5.1 驱动接口（driver.ts）

```ts
export type SqlValue = null | number | bigint | string | Uint8Array;

export interface SqlRunResult {
  readonly changes: number | bigint;
  readonly lastInsertRowid: number | bigint;
}

/** 预编译语句句柄 — 仅暴露位置参数绑定（当前代码只用到位置参数）。 */
export interface SqlStatement {
  run(...params: SqlValue[]): SqlRunResult;
  get(...params: SqlValue[]): unknown | undefined;
  all(...params: SqlValue[]): unknown[];
}

/** 数据库连接的最小可移植面。 */
export interface SqliteDriver {
  exec(sql: string): void;
  prepare(sql: string): SqlStatement;
  close(): void;
}
```

设计取舍：
- **同步 API**：`node:sqlite` / `bun:sqlite` 均为同步风格，接口保持同步。
- **最小面**：只暴露当前（含可预见的未来）真正需要的
  `exec / prepare / get / all / run / close`，不做额外包装。
  `PRAGMA`（WAL、busy_timeout 等）通过 `exec` 表达，参数与配置留给调用方。
- **行数据保持 `unknown`**：类型化行读取属于各功能自己的映射层，不塞进驱动。
- **只支持位置参数**：两个引擎都支持；命名参数支持的差异留到有需要时再演进。

### 5.2 Node 适配器（backends/node.ts）

`createNodeSqliteDriver(path: string): SqliteDriver` — 薄封装 `node:sqlite` 的
`DatabaseSync`：

- `exec(sql)` → `db.exec(sql)`
- `prepare(sql)` → 返回包装 `StatementSync` 的对象（`run/get/all` 透传）
- `close()` → `db.close()`，必须幂等（重复 close 无副作用，不抛错）

**错误语义**：打开/执行失败直接抛引擎原始错误，不吞、不重包
（`SqliteSessionIndex` 依赖 open 抛错触发 fallback，见 §6）。

### 5.3 工厂（factory.ts）— 唯一切换点

```ts
function detectRuntime(): 'node' | 'bun' {
  if (typeof process !== 'undefined' && (process.versions as { bun?: string }).bun) {
    return 'bun';
  }
  return 'node';
}

export function openSqliteDatabase(path: string): SqliteDriver {
  switch (detectRuntime()) {
    case 'node':
      return createNodeSqliteDatabase(path);
    case 'bun':
      // 预留分支：未来在此换成动态 import('bun:sqlite') + bun adapter。
      // 注意：node 构建下静态 import('bun:sqlite') 会解析失败，届时需要
      // 用动态 import + 构建配置（或按运行时拆产物）确保构建不破。
      throw new Error('bun sqlite driver is not implemented yet');
  }
}
```

未来切 Bun 时只需要：新增 `backends/bun.ts` + 替代上抛的 case 分支。
仓库内其余任何位置都不改。

### 5.4 能力层

**migrate.ts** — 版本化迁移，基于 `PRAGMA user_version`：

```ts
export interface SqlMigration {
  /** 迁移后应达到的 user_version。按升序排列。 */
  version: number;
  /** 从上一版本升级到本版本要执行的 SQL（允许多条）。 */
  statements: string;
}

/** 把 db 从当前 user_version 逐级升到最大定义版本；未达最末版本才执行，幂等。 */
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
  const v = row?.user_version;
  return typeof v === 'number' && Number.isInteger(v) ? v : 0;
}
```

**transaction.ts** — 同步事务封装：

```ts
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

语义约束：
- 仅支持顶层事务；**嵌套 `withTransaction` 不在本阶段支持**（SQLite 本身也不支持
  shareable nested tx；需要者自行 `SAVEPOINT`），接口文档注明即可。
- 任意语句抛错即回滚并重抛原错。

### 5.5 `SqliteSessionStore` 改造（生产侧唯一改动）

`packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts` 三处局部修改：

1. 删 `import { DatabaseSync } from 'node:sqlite';`，改为
   `import { openSqliteDatabase, type SqliteDriver } from '#/persistence/sqlite/factory';`
2. `private db: SqliteDriver | undefined;`
3. `open()` 中 `this.db = new DatabaseSync(this.dbPath);`
   → `this.db = openSqliteDatabase(this.dbPath);`

其余不动：
- SCHEMA 常量、`stateMtime` 迁移判断（现有 `hasStateMtimeColumn`）保持现状，
  不强行改造成 `migrate()`（本次最小改动原则；未来可把 store 迁移收敛进
  `migrate.ts` 作为示范，见 §10 演进出口）。
- WAL / busy_timeout / wal_checkpoint 等 PRAGMA 语义保留在 store（多进程行为
  有专门注释，属于领域调参，不进通用层）。
- `hasStateMtimeColumn(db)` 参数类型改 `SqliteDriver`；测试工厂
  `createForPath` 不变。

## 6. 错误处理与生命周期

| 场景 | 行为 |
|------|------|
| open 失败（权限/损坏/目录不存在） | 引擎错误原样上抛 → `SqliteSessionIndex.tryOpen` 捕获 → 永久 fallback 到 `FileSessionIndex`（现有逻辑，不变） |
| exec/prepare 执行期间的 SQL 错误 | 原样上抛，由调用方处理 |
| `close()` | 幂等；store 自身已做 `this.db === undefined` 判空，adapter 层再保证一次 |
| 事务失败 | `withTransaction` 回滚并重抛 |

## 7. 测试计划（Given-When-Then 风格）

新增 1 个测试文件：`packages/agent-core-v2/test/persistence/sqlite/sqliteDriver.test.ts`
（覆盖驱动 + 能力层，均为 Node 运行时真实执行）：

**驱动（factory → node adapter）**
- given 一个 file 路径，when open 并插入数据，then 文件真实创建且可查询
- given 多段 SQL，when exec，then 全部生效
- given 位置参数（string/number/null），when prepare().get/all/run，then 绑定正确
- given 查询无命中行，when get，then undefined（all → 空数组）
- given 一条 INSERT，when run，then 返回正确的 lastInsertRowid 与 changes
- given 已 close 的连接，when 再次 close，then 不抛错（幂等）

**migrate**
- given user_version=0 的库，when migrate 到版本 2，then 表结构存在且 user_version=2
- given 已迁移到 v2 的库，when 再次 migrate 同一组迁移，then 幂等、无副作用
- given statements 中途报错，when migrate，then 回滚且 user_version 保持不变

**withTransaction**
- given 事务内 fn 抛错，when withTransaction，then 已写数据回滚且异常被抛出
- given 事务内 fn 成功，when withTransaction，then 数据提交

**存量测试调整**
- `test/app/sessionIndex/sqliteSessionStore.test.ts`：两处裸
  `new DatabaseSync(...)` 改为 `openSqliteDatabase(...)`，使除 adapter 文件外
  仓库零 `node:sqlite` 直用。

**验证命令**

```bash
pnpm --filter @mirri-ai/agent-core-v2 run typecheck
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run \
  test/persistence/sqlite/sqliteDriver.test.ts \
  test/app/sessionIndex/sqliteSessionStore.test.ts
```

## 8. 范围边界（明确不做）

- ❌ 不实现 Bun adapter（本期只预留接口与工厂分支）。
- ❌ 不引入第三方 SQLite 依赖（drizzle / libsql / better-sqlite3 均不加）。
- ❌ 不改 `ISessionIndex` 接口、不改 store 的 SQL/索引/fallback 逻辑。
- ❌ 不做 ORM / 查询构建器 / async 化。
- ❌ 不做嵌套事务 / SAVEPOINT 封装。
- ❌ 不改 `persistence/interface/` 下既有存储接口布局（sqlite 层独立成域）。

## 9. 验收标准

1. 仓库内 `node:sqlite` 仅存在于 `persistence/sqlite/backends/node.ts`（src 与
   test 逐文件验证）。
2. agent-core-v2 typecheck 通过；驱动/能力层新测试与全部 session index 存量
   测试全绿。
3. 无新增 npm 依赖。
4. `SqliteSessionStore` 行为（含 fallback 语义）无回归。

## 10. 演进出口（未来方向的落点）

| 未来需求 | 落点 | 备注 |
|---------|------|------|
| 切 Bun 运行时 | `backends/bun.ts` + factory 分支 | 动态 import 需处理构建解析（§5） |
| 更多功能用 SQLite | 各自独立 `.db` 文件 + `migrate()` + `withTransaction()` | 每库 per-feature 的故障域与 fallback 语义清晰 |
| 引入查询构建器（如 Drizzle） | 在本层之上加适配 | 不排除、不预支 |
| 类型化行读取 | 功能侧自行映射；需要时给接口加泛型 | 演进项 |
| store 迁移工程化 | 把 SCHEMA/stateMtime 迁移改为 `migrate()` | 演进项，不在本期 |
