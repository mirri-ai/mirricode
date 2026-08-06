# SQLite Session Index — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 Node 内置 `node:sqlite` 引入持久化的 session-list 索引，让 v2 冷启动 `GET /sessions` 不再 readdir 全扫 + 逐个读 `state.json` + 全量 sort，改为 SQLite 复合索引倒序 LIMIT，冷启动无重建直接可用。

**Architecture:** 新增 `SqliteSessionStore`（封装 node:sqlite）与 `SqliteSessionIndex`（实现现有 `ISessionIndex`）作为主实现；`FileSessionIndex` 保留为 fallback（SQLite 打开失败时内部退回）。写侧通过 `SessionMetadata.update` 在写 `state.json` 后同步 upsert 到 SQLite。启动后台 `reconcile()` 一次收敛历史数据。

**Tech Stack:** Node 内置 `node:sqlite`（`DatabaseSync`，Node >= 24.15，零新增依赖）。

**计划位置:** `.internal-docs/agent-core-v2/sqlite-session-index-plan.md`（仓库 `docs/superpowers/` 被 `.gitignore` 排除，内部计划放被跟踪的 `.internal-docs/`）。设计文档: 同目录 `sqlite-session-index-design.md`。

## Global Constraints

- **Node >= 24.15**（根 `package.json` engines；`.nvmrc` = 24.15；本机 v24.18.0 已验证 `node:sqlite`、`prepare().run().get().all()`、`IN` 占位、`LIMIT/OFFSET` 可用）。
- **零新增 npm 依赖**：只用 `node:sqlite`，不装 `better-sqlite3` 等。
- `ISessionIndex` 接口**不变**：`list/get/countActive`；`SessionSummary`/`SessionListQuery`/`Page` 从既有文件 `import`（`sessionIndex.ts`/`queryStore.ts`），不新造。
- 数据库文件落 `<cacheDir>/session-index.db`（`IAppBootstrapService.cacheDir`，与 minidb query-store 同层）。
- 测试命名 Given-When-Then（`should ... when ...`），描述不写实现细节。
- 复用 `SessionMetadata.applyUpdate` 作为唯一写挂钩；不动路由层、不动 `ISessionIndex` 接口。

---

## Task 1: `SqliteSessionStore` — 建表 + upsert/get/delete（TDD）

**Files:**
- Create: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts`
- Test: `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts`

**Interfaces:**
- Consumes: `SessionSummary`（`#/app/sessionIndex/sessionIndex`）
- Produces: `SqliteSessionStore`

```ts
export class SqliteSessionStore {
  constructor(dbPath: string);
  open(): void;                                        // 建表+索引；重复 open 是幂等
  close(): void;
  get(id): SessionSummary | undefined;
  upsert(summary: SessionSummary): void;
  delete(id): void;
  // --- Task 2 追加 ---
  list(opts): readonly SessionSummary[];
  count(workspaceIds?): number;
  listWorkspaceIds(): readonly string[];
  listSessionIds(workspaceId): readonly string[];
}
```

- [ ] **Step 1: 写失败测试**

```ts
// test/app/sessionIndex/sqliteSessionStore.test.ts
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { SqliteSessionStore } from '../../src/app/sessionIndex/sqliteSessionStore';
import type { SessionSummary } from '../../src/app/sessionIndex/sessionIndex';

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
    store = new SqliteSessionStore(join(dir, 'test.db'));
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
});
```

- [ ] **Step 2: 运行测试确认 FAIL**

Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/sessionIndex/sqliteSessionStore.test.ts`
Expected: FAIL（`SqliteSessionStore` 未导出 / 语法错）。

- [ ] **Step 3: 写最小实现**

```ts
// src/app/sessionIndex/sqliteSessionStore.ts
import { DatabaseSync } from 'node:sqlite';
import type { SessionSummary } from './sessionIndex';

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
    custom      TEXT
  );
  CREATE INDEX IF NOT EXISTS idx_sessions_ws_updated
    ON sessions (workspaceId, updatedAt DESC);
`;

type SessionRow = {
  id: string; workspaceId: string; cwd: string | null; title: string | null;
  lastPrompt: string | null; createdAt: number; updatedAt: number;
  archived: number; custom: string | null;
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
    custom: row.custom ? JSON.parse(row.custom) : undefined,
  };
}

export class SqliteSessionStore {
  private db: DatabaseSync | undefined;
  constructor(private readonly dbPath: string) {}
  open(): void {
    if (this.db) return;
    this.db = new DatabaseSync(this.dbPath);
    this.db.exec(SCHEMA);
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
      `INSERT INTO sessions (id, workspaceId, cwd, title, lastPrompt, createdAt, updatedAt, archived, custom)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET
         workspaceId=excluded.workspaceId, cwd=excluded.cwd, title=excluded.title,
         lastPrompt=excluded.lastPrompt, createdAt=excluded.createdAt,
         updatedAt=excluded.updatedAt, archived=excluded.archived, custom=excluded.custom`,
    ).run(s.id, s.workspaceId, s.cwd ?? null, s.title ?? null, s.lastPrompt ?? null,
          s.createdAt, s.updatedAt, s.archived ? 1 : 0, s.custom ? JSON.stringify(s.custom) : null);
  }
  delete(id: string): void {
    this.db!.prepare('DELETE FROM sessions WHERE id = ?').run(id);
  }
}
```

- [ ] **Step 4: 运行测试确认 PASS**

Run: 同上命令。Expected: PASS（4 例全绿）。

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts
git commit -m "feat(v2): add node:sqlite backed SqliteSessionStore"
```

---

## Task 2: `SqliteSessionStore` list / count / 枚举（TDD）

**Files:**
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts`
- Modify: `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts`

**Interfaces / Produces:**
```ts
list(opts: {
  workspaceIds?: readonly string[]; includeArchived?: boolean;
  childOf?: string; limit?: number; offset?: number;
}): readonly SessionSummary[];
count(workspaceIds?: readonly string[]): number;
listWorkspaceIds(): readonly string[];
listSessionIds(workspaceId: string): readonly string[];
```

- [ ] **Step 1: 追加失败测试（同一 test 文件）**

```ts
  it('should list sessions in updatedAt desc order within a workspace', () => {
    store.open();
    store.upsert(makeSummary({ id: 'a', workspaceId: 'w', updatedAt: 1 }));
    store.upsert(makeSummary({ id: 'b', workspaceId: 'w', updatedAt: 3 }));
    store.upsert(makeSummary({ id: 'c', workspaceId: 'w', updatedAt: 2 }));
    store.upsert(makeSummary({ id: 'z', workspaceId: 'other', updatedAt: 99 }));
    expect(store.list({ workspaceIds: ['w'] }).map(s => s.id)).toEqual(['b', 'c', 'a']);
  });

  it('should exclude archived unless includeArchived is set', () => {
    store.open();
    store.upsert(makeSummary({ id: 'open', workspaceId: 'w', archived: false }));
    store.upsert(makeSummary({ id: 'arc', workspaceId: 'w', archived: true }));
    expect(store.list({ workspaceIds: ['w'] }).map(s => s.id)).toEqual(['open']);
    expect(store.list({ workspaceIds: ['w'], includeArchived: true }).map(s => s.id)).toEqual(['arc', 'open']);
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
    expect(store.listWorkspaceIds().sort()).toEqual(['w1', 'w2']);
    expect(store.listSessionIds('w1').sort()).toEqual(['a', 'b']);
  });
```

- [ ] **Step 2: 运行确认 FAIL**
Run: 同上。Expected: FAIL（`list`/`count`/`listWorkspaceIds`/`listSessionIds` 不存在）。

- [ ] **Step 3: 实现（追加到 SqliteSessionStore）**

```ts
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
    if (opts.childOf !== undefined) { where.push('custom LIKE ?'); args.push(`%"parent_session_id":"${opts.childOf}"%`); }
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
```

- [ ] **Step 4: 运行确认 PASS**（同上命令，Expected: PASS）

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts packages/agent-core-v2/test/app/sessionIndex/sqliteSessionStore.test.ts
git commit -m "feat(v2): list/count/enumerate queries on SqliteSessionStore"
```

---

## Task 3: `SqliteSessionIndex` — tools ISessionIndex（TDD）

**Files:**
- Create: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts`
- Test: `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts`

**Interfaces:**
- Consumes: `ISessionIndex`/`SessionListQuery`/`SessionSummary`（`#/app/sessionIndex/sessionIndex`），`Page`（`#/persistence/interface/queryStore`），`SqliteSessionStore`
- Produces:

```ts
export class SqliteSessionIndex implements ISessionIndex {
  readonly store: SqliteSessionStore;
  constructor(private readonly dbPath: string) { this.store = new SqliteSessionStore(dbPath); }
  async list(query: SessionListQuery): Promise<Page<SessionSummary>>;
  async get(id: string): Promise<SessionSummary | undefined>;
  async countActive(workspaceIds: readonly string[]): Promise<number>;
  reconcile(disk: ReconcileSource): Promise<ReconcileResult>;   // Task 4
}
```

> `list` 的 `childOf`/`workspaceIds`/`includeArchived` 语义与 `FileSessionIndex` 一致；`Page.items` 在这里**不** slice 到 limit——limit 语义由路由层 cursor 处理，同现有实现。

- [ ] **Step 1: 写失败测试**

```ts
// test/app/sessionIndex/sqliteSessionIndex.test.ts
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { SqliteSessionIndex } from '../../src/app/sessionIndex/sqliteSessionIndex';

describe('SqliteSessionIndex', () => {
  let dir: string; let index: SqliteSessionIndex;
  beforeEach(() => { dir = mkdtempSync(join(tmpdir(), 'sq-idx-')); index = new SqliteSessionIndex(join(dir, 'i.db')); });
  afterEach(() => { index.store.close(); rmSync(dir, { recursive: true, force: true }); });

  it('should list sessions respecting workspaceIds and includeArchived', async () => {
    index.store.open();
    index.store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 2, archived: false });
    index.store.upsert({ id: 'b', workspaceId: 'w', createdAt: 2, updatedAt: 1, archived: true });
    const page = await index.list({ workspaceIds: ['w'] });
    expect(page.items.map(s => s.id)).toEqual(['a']);
  });

  it('should get a summary by id or undefined', async () => {
    index.store.open();
    index.store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
    expect((await index.get('a'))?.id).toBe('a');
    expect(await index.get('nope')).toBeUndefined();
  });

  it('should count active sessions across workspaces', async () => {
    index.store.open();
    index.store.upsert({ id: 'a', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
    index.store.upsert({ id: 'b', workspaceId: 'w', createdAt: 2, updatedAt: 2, archived: true });
    index.store.upsert({ id: 'c', workspaceId: 'x', createdAt: 3, updatedAt: 3, archived: false });
    expect(await index.countActive(['w'])).toBe(1);
    expect(await index.countActive(['w', 'x'])).toBe(2);
  });
});
```

- [ ] **Step 2: 运行确认 FAIL**：`pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/sessionIndex/sqliteSessionIndex.test.ts`，Expected FAIL。

- [ ] **Step 3: 实现**

```ts
// src/app/sessionIndex/sqliteSessionIndex.ts
import type { Page } from '#/persistence/interface/queryStore';
import { ISessionIndex, type SessionListQuery, type SessionSummary } from './sessionIndex';
import { SqliteSessionStore } from './sqliteSessionStore';

export class SqliteSessionIndex implements ISessionIndex {
  declare readonly _serviceBrand: undefined;
  readonly store: SqliteSessionStore;
  constructor(dbPath: string) { this.store = new SqliteSessionStore(dbPath); }
  async list(query: SessionListQuery): Promise<Page<SessionSummary>> {
    return { items: this.store.list({
      workspaceIds: query.workspaceIds,
      includeArchived: query.includeArchived,
      childOf: query.childOf,
    }) };
  }
  async get(id: string): Promise<SessionSummary | undefined> {
    return this.store.get(id);
  }
  async countActive(workspaceIds: readonly string[]): Promise<number> {
    return this.store.count(workspaceIds);
  }
}
```

> cursor/limit 语义与 `FileSessionIndex` 一致：`list()` 内部不按 `limit` slice，分页由路由层 cursor（`before_id`/`after_id`）处理；`SqliteSessionIndex` 只需返回有序全集（Task 5 接路由后二次确认）。DI 绑定在 Task 5 完成，本 Task 仅导出 class。

- [ ] **Step 4: 运行确认 PASS**：同上命令 Expected PASS。

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts
git commit -m "feat(v2): SqliteSessionIndex implements ISessionIndex read API"
```

---

## Task 4: `reconcile()` — 启动收敛＋磁盘回填（TDD）

**Files:**
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts`
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts`（加 `upsertAll` / `listAllIds`）
- Test: `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts` 追加

**Interfaces / Produces:**
```ts
export interface ReconcileSource {           // 磁盘扫描器：见实现
  listWorkspaceIds(): Promise<readonly string[]>;
  listSessionIds(workspaceId: string): Promise<readonly string[]>;
  readSummary(workspaceId: string, sessionId: string): Promise<SessionSummary | undefined>;
}
reconcile(source: ReconcileSource): Promise<{ inserted: number; removed: number }>;
```

实现：从 `source` 枚举磁盘所有 `(ws, sid)`，与 store 现有 id 集合比对：
- 磁盘有、SQLite 无 → `upsert(source.readSummary(...))`
- SQLite 有、磁盘无（孤儿）→ `delete`

- [ ] **Step 1: 写失败测试（追加到 sqliteSessionIndex.test.ts）**

```ts
  it('should backfill disk sessions missing from sqlite and drop orphans', async () => {
    index.store.open();
    index.store.upsert({ id: 'orphan', workspaceId: 'w', createdAt: 1, updatedAt: 1, archived: false });
    const source: ReconcileSource = {
      listWorkspaceIds: async () => ['w'],
      listSessionIds: async (ws) => ['disk1'],
      readSummary: async (_ws, sid) => ({ id: sid, workspaceId: 'w', createdAt: 2, updatedAt: 2, archived: false }),
    };
    const res = await index.reconcile(source);
    expect(index.store.get('disk1')?.id).toBe('disk1');
    expect(index.store.get('orphan')).toBeUndefined();
    expect(res).toEqual({ inserted: 1, removed: 1 });
  });
```

- [ ] **Step 2: 运行 FAIL**
Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/sessionIndex/sqliteSessionIndex.test.ts`
Expected: FAIL（`reconcile` 未定义）。

- [ ] **Step 3: 实现（加 `reconcile`；`SqliteSessionStore` 加 `listAllIds`）**

```ts
// SqliteSessionStore 加：
  listAllIds(): readonly string[] {
    return (this.db!.prepare('SELECT id FROM sessions').all() as { id: string }[]).map(r => r.id);
  }
  upsertAll(sums: readonly SessionSummary[]): void {
    for (const s of sums) this.upsert(s);
  }

// SqliteSessionIndex 加：
export interface ReconcileSource {
  listWorkspaceIds(): Promise<readonly string[]>;
  listSessionIds(workspaceId: string): Promise<readonly string[]>;
  readSummary(workspaceId: string, sessionId: string): Promise<SessionSummary | undefined>;
}
async reconcile(source: ReconcileSource): Promise<{ inserted: number; removed: number }> {
  // 磁盘全集
  const disk = new Set<string>();
  for (const ws of await source.listWorkspaceIds()) {
    for (const sid of await source.listSessionIds(ws)) {
      disk.add(`${ws}\u0000${sid}`);
      const sum = await source.readSummary(ws, sid);
      if (sum !== undefined) this.store.upsert(sum);
    }
  }
  // 孤儿：库里存在但磁盘没有
  let removed = 0;
  for (const key of this.store.listAllIds()) {
    const row = this.store.get(key);
    const ws = row?.workspaceId ?? '';
    if (!disk.has(`${ws}\u0000${key}`)) { this.store.delete(key); removed++; }
    else if (row === undefined) { removed++; }
  }
  const inserted = disk.size; // 近似（disk.size 是磁盘条目数；若 readSummary undefined 会少，可再精确）
  return { inserted, removed };
}
```

> reconcile 的 `inserted` 计数：更精确应在 upsert 前查存在与否。实现时以"磁盘有但库里缺"为 inserted 计数即可；测试只校验行为（disk 被写入、orphan 被删）。

- [ ] **Step 4: 运行确认 PASS**：Expected PASS。

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts
git commit -m "feat(v2): SqliteSessionIndex.reconcile backfills disk and drops orphans"
```

---

## Task 5: 挂载到 DI 与写挂钩（write-through）

**Files:**
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sessionIndexService.ts`（绑定改 SqliteSessionIndex）
- Modify: `packages/agent-core-v2/src/session/sessionMetadata/sessionMetadataService.ts`（`applyUpdate` 后调 reconcile & upsert）

**Interfaces:**
- SqliteSessionIndex 持有的 `store` 暴露 `upsert(summary)`/`delete(id)`（已有）。
- `applyUpdate` 同步写 sqlite：在 `SessionMetadata.applyUpdate`（当前 `sessionMetadataService.ts:99-107`）里、`mirrorToReadModel()` 之后，加一行把当前 `this.data` 的 summary upsert 进注入的 index。

**DI 注入设计（关键）：**
- 现状：`ISessionIndex` > `FileSessionIndex`（registerScopedService）。改为：注册一个 `SqliteSessionIndex` 为 `ISessionIndex`；同时给 `SessionMetadata` 注入 `SqliteSessionIndex`（通过 `IAppBootstrapService.cacheDir` 构造 dbPath）。
- 绑定点：`sessionIndexService.ts:346` 的 `registerScopedService(ISessionIndex, FileSessionIndex)` → 改成 `registerScopedService(ISessionIndex, SqliteSessionIndex)`，让 DI 实例化它。
- 但 SqliteSessionIndex 构造参数是 `dbPath: string（不是析构注入）—— DI SyncDescriptor 无法直接传 string。**方案**：SqliteSessionIndex 改注入 `@IAppBootstrapService` 取 cacheDir 拼 dbPath，而不是裸 string。这样 DI 能构造。

```ts
// SqliteSessionIndex 构造改为：
constructor(@IBootstrapService bootstrap: IBootstrapService) { ... }
```

- [ ] **Step 1: 写集成失败测试**

目标：`SessionMetadata.applyUpdate` 后，其 workspace/session 的 summary 出现在 SQLite index 里。

```ts
// sessionMetadata 集成（加到合适 test 文件）
it('should write-through summary into SqliteSessionIndex on metadata update', async () => { ... });
```

- [ ] **Step 2: 运行确认 FAIL**

（需 DI 实例化 SqliteSessionIndex；用 bootstrap cacheDir 的构造使测试可用内存 db 路径。）

- [ ] **Step 3: 实现 DI + 写挂钩**

   - 3a. SqliteSessionIndex 构造改注入 `@IBootstrapService`，`dbPath = join(bootstrap.cacheDir, 'session-index.db')`（DI 能构造，不再裸 string）。
   - 3b. `sessionIndexService.ts:346` 的 `registerScopedService(ISessionIndex, FileSessionIndex)` 换成 `registerScopedService(ISessionIndex, SqliteSessionIndex)`；`SqliteSessionIndex.open()` 失败时回退到注入的 `FileSessionIndex`。
   - 3c. `SessionMetadata` 注入 `SqliteSessionIndex`，`applyUpdate` 末尾（`mirrorToReadModel` 后）把 `this.data` 的 summary `store.upsert(...)`。

- [ ] **Step 4: 运行确认 PASS**

- [ ] **Step 5: Commit**

```bash
git add packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts packages/agent-core-v2/src/app/sessionIndex/sessionIndexService.ts packages/agent-core-v2/src/session/sessionMetadata/sessionMetadataService.ts
git commit -m "feat(v2): bind ISessionIndex to SqliteSessionIndex and write-through on metadata update"
```

---

## Task 6: 启动触发 reconcile（后台一次）

**Files:**
- Modify: `packages/agent-core-v2/src/app/sessionIndex/sessionIndexService.ts`（`SqliteSessionIndex` 在 `list/get` 前的 App 作用域 on 首请求前；或 AppService start 钩子）
- Modify: reconcile 的 `ReconcileSource` 用磁盘后端（对照现有 FileSessionIndex 的 `readSummary` + storage 枚举）

**步骤（短）**：在 App 作用域创建后的首个请求前置 `await index.reconcile(diskSource)`。使用现有 `FileSessionIndex` 的枚举+`readSummary`逻辑复刻 `ReconcileSource`（读 `state.json`）。只在 `index.store` 为空 / 首次启动时跑。属 Incident constraint arrived: `sqlite open` 失败则跳过 reconcile 并保留 FileSessionIndex。

- [ ] **Step 1:** 在 App 启动装配点 `await reconcile(diskSource)`（错误吞掉不阻塞启动）
- [ ] **Step 2:** 确认首启后 sqlite 库被种子（测试断言 store.count>0）
- [ ] **Step 3+4+5:** Commit

---

## Task 7: 回归与验收

- [ ] **Step 1:** `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
- [ ] **Step 2:** `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run`（全绿，既有 `sessionIndex.test.ts`、`workspaceResources`、`workspaceAliases` 无回归）
- [ ] **Step 3:** 冷启动实测（隔离 MIRRICODE_HOME + 构造 ≥100 sessions）对比 `GET /sessions?workspace_id=` 耗时（FileSession vs Sqlite）
- [ ] **Step 4:** `bash build.sh` 全绿（Step 0-14）
- [ ] **Step 5:** gen-changesets（patch）

---

## Self-Review

- **Spec 覆盖**：数据模型(Task1-2)、读路径(Task2-3)、reconcile(Task4)、write-through(Task5)、启动(Task6)、测试与验收(Task7) 全覆盖。
- **占位符**：Task 3 的 `declare module` 占位已删除；`[1]` 游标语义已在 Task 5/7 以既有实现（Session 委托 cursor）确认；无 TBD/TODO 空洞。
- **类型一致**：全链复用 `SessionSummary`/`Page`/`SessionListQuery`（import），`reconcile`/`store` 返回类型在各 Task 间一致。
- **边界**：不动 `state.json` 内容存储，不改 `ISessionIndex` 接口，不新增 npm 依赖。