# SQLite Session Index — Read-Path Convergence Design

> **Status**: Draft（待实现）
> **Date**: 2026-08-03
> **Related**:
> - `.internal-docs/agent-core-v2/sqlite-session-index-design.md`（索引整体设计）
> - `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts`（改动主文件）
> - 实测基线：warm 启动 reconcile `elapsedMs=12907` → 优化后 `4/8/16`；但**冷启动首屏
>   sessions API 仍约 10s+**（用户实测，见下）

> 本设计解决「SQLite 索引已落地、reconcile 已提速，但冷启动首屏 sessions 请求仍然慢」的
> 剩余根因：**每个读请求前都会触发一次阻塞事件循环的全量磁盘同步**。

---

## 1. 问题：我们优化了「启动时」，慢的是「每次请求前」

上一轮已把**启动时的 reconcile** 从 12.9s 压到 ms 级（增量 + 同步 fs + stateMtime 快照）。
但 desktop 冷启动体感并没有变快，因为实际卡住用户的是**服务就绪后的第一波
`GET /api/v1/sessions` 请求**，走的是完全不同的热路径。

### 代码事实（已核实）

`SqliteSessionIndex` 的三个读入口，**每一个都在真正读之前无条件调用 `syncFromDisk()`**：

```ts
// packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts
async list(query)        { await this.ensureOpen(); ...; await this.syncFromDisk(); ... } // :117
async get(id)            { await this.ensureOpen(); ...; await this.syncFromDisk(); ... } // :130
async countActive(ids)   { await this.ensureOpen(); ...; await this.syncFromDisk(); ... } // :144
```

`syncFromDisk()`（:99-112）的结构：

```
for ws of diskSource.listWorkspaceIds()        // readdirSync 全部 workspace 目录
  for sid of diskSource.listSessionIds(ws)     // readdirSync 该 workspace 全部 session 目录
    if (store.get(sid) !== undefined) continue // store 命中跳过
    … read state.json → upsert                 // 缺失的才回填
```

三个致命点：

1. **每次读都全量扫**：不区分请求的 workspace。真实环境 18 个 workspace 目录 → 203 个
   session 目录，每个请求都完整走一遍 `readdirSync` + 逐 session `store.get`（SQLite
   逐条查询）。
2. **同步 fs 阻塞事件循环**：`readdirSync` / `statSync`（`sqliteDiskSource.ts:70-103`）
   是主线程同步调用。单次几百 micro~ms 级，但**并发请求排队**时被放大。
3. **前端首屏是并发风暴**：`useWorkspaceState.ts:663` 对**每个 workspace 发一个
   `Promise.allSettled` 的并发请求**（各 workspace 再 5 条一页 + 12h 窗口翻页，
   `useWorkspaceState.ts:54,507`）。18 个 workspace × 每页一次 list → 每个 list 都做
   一遍全量 sync → 全部挤在同一个事件循环里串行排队。这就是 10s+ 的直接来源。

### 为什么「刷新快、重启慢」能自洽

- 重启后第一次请求：所有并发请求在同一瞬间撞上冷状态，各自先做一遍完整扫盘。
- 刷新时：store 已被首轮填满、目录已在 OS page cache，`syncFromDisk` 里「store.get
  命中即 continue」几乎每 session 都命中，只剩 `readdirSync` 的成本，一次请求一次扫描
  → 快。
- 也就是：**慢的不是 SQLite，而是每次读之前非要碰一遍磁盘的全部目录**。

## 2. 目标与不做什么

### 目标

1. **warm store 下读路径零磁盘扫描**：目录 mtime 没变 = 磁盘没有新增 session → 完全跳过
   `readdirSync` 枚举，`list/get/countActive` 变为纯 SQLite 查询。
2. **冷状态只扫一次**：并发请求合并成单次扫描；`list({workspaceIds})` 只扫涉及的
   workspace（而不是全部 18 个）。
3. **`get` 不扫盘**：store 命中直接返回；miss 时 fallback 已有的 `FileSessionIndex.get`
   单会话读磁盘（写穿 + reconcile 保证绝大多数场景 store 都是热的）。
4. **语义零改变**：v1/TUI 并发创建的新 session 仍在下一个带变化的请求里可见（目录
   mtime 变化触发回填）；孤儿/删除收敛仍归启动 reconcile；单测 seed store 的契约不受影响。

### 明确不做

- 不做前端并发请求的分批/懒加载（那是另一个独立的改进，可后置）。
- 不做 fs.watch / 后台 watcher（方案 C 已被排除）。
- 不删除 `syncFromDisk` 的 insert-only 语义（孤儿删除永远在 reconcile 里）。
- 不动 `ISessionIndex` 接口，不动路由层（`packages/kap-server/src/routes/sessions.ts`
  零感知）。

## 3. 设计：定向扫描 + 目录 mtime 快照 + 并发合并

改动收敛在 `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts` 内。

### 3.1 `syncFromDisk(scope)` 接受 workspace 过滤

```ts
private async syncFromDisk(scope?: { workspaceIds?: readonly string[] }): Promise<void>
```

- `list()` 传入 `query.workspaceIds`；undefined 表示全局（该扫的仍扫全部，但此时启用
  3.2/3.3 的合并与快照）。
- `countActive(workspaceIds)` 传入同样的目标集合。
- `get()` **不再调用 `syncFromDisk`**：直接 `store.get(id)`，miss 时走既有
  `legacy.get(id)`（`FileSessionIndex.get`，单会话半查 + 读 state.json，量级远小于
  全量枚举）。这样 `GET /sessions/{id}` 不再有任何全盘代价。

### 3.2 目录 mtime 快照（warm 零扫描的判据）

每个 workspace 目录记录一次 `statSync(sessionsDir/<ws>).mtimeMs`，存入
`syncState: Map<string, number>`。枚举前先 `statSync` 一次目录 mtime：

- **相等** → 磁盘没有可观察变化，整 bypass（不再 `readdirSync` 子目录）。
- **不等/首次** → 枚举该 workspace 的 session 目录，回填缺失的 summary，再更新快照。

目录 mtime 语义恰好匹配数据：**新增/删除 session 目录会改写父目录 mtime**（`stat`
对子目录，`readdir` 后写文件不改变父目录 mtime）。`syncFromDisk` 只关心「有没有新
session 目录」，不关心已有 session 内容变化（内容变化由 reconcile 的 stateMtime
处理）——所以目录粒度快照是**精确且足够**的。

注意：删除 session 目录也会改 mtime；`syncFromDisk` 是 insert-only 不删除 store 行，
孤儿收敛仍只属于 `reconcile`，不会破坏「单测直接 seed store 行（磁盘为空）」的既有
契约。

### 3.3 并发合并（风暴场景只扫一次）

```ts
private syncInFlight: Promise<void> | undefined;
private syncFromDisk(scope) {
  // 等待进行中的扫描（先前的失败被吞掉，避免毒化后续读），再收敛自己的
  // scope —— mtime 快照会跳过刚才已核验过的目录，非重叠 scope 只付一次
  // 目录 stat 的代价。
  const previous = this.syncInFlight;
  const run = (previous === undefined ? Promise.resolve() : previous.catch(() => undefined))
    .then(() => this.doSyncFromDisk(scope));
  // 守卫必须用 wrapped（`finally` 返回新 promise，若比较原始 `run` 恒为
  // false → syncInFlight 永不重置、一次失败永久毒化所有读）。
  const wrapped = run.finally(() => {
    if (this.syncInFlight === wrapped) this.syncInFlight = undefined;
  });
  this.syncInFlight = wrapped;
  return wrapped;
}
```

18 个并发 `list` 只有第一个真正 scan，其余 await 同一个 promise；一个线程完成
后其余直接从 store 读，加上 3.2 的快照，后续请求零扫。失败的扫描只让当次
调用者看到错误，短暂停后即自愈。

### 3.3 行为对照表

| 场景 | 现状 | 改后 |
| --- | --- | --- |
| store 热（sessions 已入库），目录 mtime 未变 | 每次读全量 readdir + 逐 session store.get | **纯 SQLite 读** |
| 首启并发 18 workspace 风暴 | 18 次全量扫 | 1 次全量扫（并发合并，若全量模式）或 18 各自只扫自己 |
| `list({workspaceIds:[w]})` | 扫全部 | 只扫 w |
| `get(id)` | 先全量扫再查询 | store 命中 = 0 磁盘；miss → 单会话 legacy 读 |
| v1/TUI 新建 session 后 cloud | 其他. 全量扫 catch 到 | 目录 mtime 变化 → 该 workspace 一轮增量回填 |
| 孤儿收敛 / 删除清除 | reconcile 负责 | 不变（仍属 reconcile） |
| fallback（SQLite open 失败） | list/get/countActive 走 legacy | 不变 |

## 4. 测试计划（Given-When-Then，TDD）

写好现有 80 用例（`test/app/sessionIndex/sqliteSessionIndex.test.ts`）全部保持绿。

新增用例（同文件追加，避免新增测试文件）：

- given 目录 mtime 未变且 store 已包含该 session，when list 同一个 workspace，then
  `readdirSync` 不触发（可用一个计数 observable 断言——在测试里构造真实临时目录，
  用 mock disk source 包一层计数，或直接用「目录添加新 session 文件后不改变 mtime
  → 结果不包含该 session」来行为断言）。
- given 目录内新增一个 session 子目录，when 再次 list，then 新 session 被回填且旧的
  保持。
- given 两个 workspace A/B 都含会话，when `list({workspaceIds:[A]})`，then B 的会话
  不被枚举（断言 B 目录从未被 readdir）。
- given 两个并发 list（同时发），when 都完成，then 磁盘枚举只发生一次（in-flight
  合并）。
- given store 命中，when `get`，then 不触发任何磁盘目录枚举。
- given store 未命中但磁盘存在（v1 外部创建），when `get`，then 通过 legacy 找到并
  返回。
- given 目录 mtime 未变，when `countActive`，then 只走 SQLite count 不接触磁盘目录。
- 回归：无 workspaceId 的全局 list 仍能枚举到全部 workspace 的新 session（变后一致）。

### E2E（真实 dev server 冷启动）

用 `MIRRICODE_HOME=<临时目录>` 起真实 v2 server（`apps/mirri-code` 的
`dev:kap-server:multi`），第一次启动后立即：

1. 顺序请求全部 workspace 的 `GET /api/v1/sessions?workspace_id=<w>&page_size=5`。
2. 记录每个请求 p50/p95/总耗时，与改动前同脚本基线对比（预期 p95 从秒级降至
   ms 级，且目录未变化时后续请求为纯 SQLite）。
3. 判断标准：warm 后再请求一次相同列表，`elapsed < 50ms`，磁盘枚举 0 次（可用
   `fs.realpath*/readdir` 计数或 server 端日志 + 记录）。

## 5. 验收标准

- 冷启动首屏（18 workspaces 并发 + 翻页）总耗时显著降低：p95 首跳应 ≤ 500ms
  （基线 10s+）。
- `GET /sessions/{id}`（store 热）dead 0 磁盘 I/O（无 readdirSync）。
- 现有 `agent-core-v2` 测试全绿；`packages/kap-server` 全绿；typecheck 过。
- `build.sh` Step 0–14 全走。
- 无新增依赖，不改路由，不改接口。

## 6. 范围边界

- 本设计只改 `sqliteSessionIndex.ts`（+ 必要的小量测试基建），`sqliteDiskSource.ts`
  如需暴露「目录 mtime」只加一个方法（现在 `statSessionStateMtime` 已有 pattern）。
- `apps/mirri-web` 的并发/懒加载改造——除非此轮做完后还有明显剩余，否则单独跟进，
  不混在本次。

## 7. Future

- 若首屏仍有明显感知，可做前端「最近活跃 workspace 优先渲染 + 其余懒加载」。
- 若未来出现大量跨进程并发写（v1 TUI + v2 server 同时），再考虑 watcher。