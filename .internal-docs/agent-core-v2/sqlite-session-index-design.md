# SQLite Session Index — Internal Design

> **Status**: Draft (brainstorming output, pending implementation)
> **Date**: 2026-08-03
> **Related**: `packages/agent-core-v2/src/app/sessionIndex/sessionIndexService.ts`（现状）、
> `packages/agent-core/src/session/store/session-store.ts`（v1 对标）

> 内部工程设计文档。讨论「v2 冷启动 session list 全扫慢」的根本解法：
> 用 Node 内置 `node:sqlite` 引入持久化的 session-list 索引，让读路径从
> 「多轮 readdir + 每个 session 读 state.json + 全量 sort」变为
> 「SQLite 复合索引倒序 LIMIT」，并在冷启动时**无重建直接可用**。

- Status: Draft（待实现）
- 范围: `packages/agent-core-v2`
- 依赖: 零新增（使用 Node 内置 `node:sqlite`，Node >= 24.15.0）

---

## 1. 背景与问题

v2 的 `FileSessionIndex`（`packages/agent-core-v2/src/app/sessionIndex/sessionIndexService.ts`）
是 `ISessionIndex` 的本地默认实现。当前 `GET /api/v1/sessions` 等读 session 列表走的是
`listLegacy`（read-model flag 默认关），其 I/O 结构为：

```
listWorkspaceIds()        → storage.list(sessions scope) 一次 readdir
  └ for each workspaceId:
      listSessionIds(ws)  → storage.list(...) 又一次 readdir（全量，无 limit）
      for each sessionId:
        readSummary()     → 逐个读 <sessionDir>/state.json + JSON.parse
items.sort(updatedAt desc)     → 全量排序
return items.slice(0, limit)   → 最后才截断
```

对比 v1（`packages/agent-core/src/session/store/session-store.ts`），v1 用 append-only 的
全局 `session_index.jsonl` 一次性 `readFile` 拿到全部 `sessionId→dir` 映射（O(1) enumerate），
随后仍需逐个读 `state.json`，但**枚举这一维的 I/O 远小于 v2 的多轮 readdir**。冷启动体感
v1 快于 v2 的核心差异就在这个「发现 session 的方式」。

### 根因归纳

- **必须全扫**：wire 语义是 `updatedAt` 降序 + 分页 + `countActive`，排序需全集。
- **而全扫时 I/O 昂贵**：v2 用 readdir 当索引 + 逐 session 读文件，未复用 v1 已有的一次性索引。
- **缓存方案治不了冷启动**：minidb read-model（`persistence_minidb_readmodel`）是「可丢弃的
  派生缓存」，冷启动时缓存为空，仍须先从 state.json 回源重建——治的是"第二次起"而非"第一次"。

### 设计目标

1. **消除冷启动回源**：session list 的读取路径在进程重启后**无需重建、直接可用**。
2. **消除全量 sort**：让排序下推到数据层，走复合索引倒序 + LIMIT。
3. **对齐 v1 读快、并超越它**：不仅缓解"发现"开销，还一并解决"排序全扫"。
4. **不侵入路由层**：`ISessionIndex` 接口不变，上层（路由、`ISessionLegacyService`）零感知。

## 2. 采纳的混一致性模型（与 state.json 的关系）

采纳 **write-through 读模型 + 启动 reconcile** 的混合模型：

- **`state.json` 仍是每个 session 的权威写存储**（存完整内容，可用于修复/兜底）。
- **SQLite 是 session list 的专属读路径**：只承载 summary 索引（id/workspaceId/cwd/title/
  lastPrompt/createdAt/updatedAt/archived/custom），不含每个 session 的内容正文。
- **写侧 write-through**：create / archive / rename / update 时，除写 `state.json` 外，同步
  `upsert`/`delete` 进 SQLite，两者顺序一致，写失败可回滚或留给 reconcile 兜底。
- **启动 reconcile**：开库后短暂扫一次目录树，把 SQLite 缺失的旧 session 补进来、孤儿移除，
  闭合历史数据与崩溃遗留。只跑一次，之后靠 write-through 维护。
- **读侧**：`list`/`get`/`countActive` 全部走 SQLite，`ORDER BY updated_at DESC LIMIT n` 走
  `(workspace_id, updated_at DESC)` 复合索引。

> 「SQLite 是 read model、state.json 是 authority」的混合语义需要接受：若用户在进程外部直接
> 改 state.json（非 mirri-code 写入），SQLite 不会自动感知，由下次 reconcile 收敛。对
> mirri-code 自身写入的场景足够。

## 3. 架构

```
                  ISessionIndex (接口不变，路由层不感知)
                  list / get / countActive
        ▲                                    ▲
        │ (绑定主读路径)                  │ (fallback)
┌───────┴───────────┐        ┌─────────────────────────┐
│ SqliteSessionIndex │        │ FileSessionIndex (保留)  │
│ = 主读路径         │        │ = 韧性底座                │
└─────────┬─────────┘        └────────────▲────────────┘
          │ node:sqlite DatabaseSync      │ legacy 读
          │ (sessions 表 + 复合索引)       │
          └────────────┬──────────────────┘
                       │ write-through (upsert/delete)
        ┌──────────────┴──────────────────────┐
        │ ISessionIndexWriter (新增轻量接口)    │
        │  由 ISessionMetadata.update、         │
        │  ISessionLifecycle.create/archive …   │
        │  在写 state.json 后同步调用            │
        └──────────────────────────────────────┘

- `ISessionIndex` 接口**保持不变**：`list/ get/ countActive`。`ISessionIndex` 的绑定
  （DI 注册）指向 `SqliteSessionIndex`；`FileSessionIndex` 保留为 fallback。
- 新增 `SqliteSessionIndex`（实现 `ISessionIndex`）+ `ISessionIndexWriter`（写入挂钩接口）
   + `reconcile()`。`FileSessionIndex` 仍存在，作为回退底座，不删。

## 4. 数据


### 表 & 索引

```sql
CREATE TABLE IF NOT EXISTS sessions (
  id           TEXT PRIMARY KEY,     -- sessionId（含 workspace 前缀）
  workspace_id TEXT NOT NULL,
  cwd          TEXT,
  title        TEXT,
  last_prompt  TEXT,
  created_at   INTEGER NOT NULL,    -- epoch ms
  updated_at   INTEGER NOT NULL,    -- epoch ms
  archived     INTEGER NOT NULL DEFAULT 0,
  custom       TEXT                 -- JSON.stringify(custom)，读回 JSON.parse
);
CREATE INDEX IF NOT EXISTS idx_sessions_ws_updated
  ON sessions (workspace_id, updated_at DESC);
```

> `custom` 存 JSON 字符串：`state.json` 的 `custom` 可能是对象，SQLite 单列存 TEXT，
> 读回 `JSON.parse` 还原 `SessionSummary.custom`（保持 wire 形状不变）。

### Summary 映射

`SqliteSessionIndex` 从 `state.json`（`readMeta` 同一路径）构造 `SessionSummary` 的字段
（`cwd/title/lastPrompt/createdAt/updatedAt/archived/custom`）—— 与现有
`FileSessionIndex.readSummary` 逻辑一致。只是**来源不同**：write-through 时才补读 state.json；
写入成功后持久化到 SQLite；list 时从 SQLite 读无需碰 state.json。

## 5. 读路径

### list

- `workspaceIds` 给出时：`WHERE workspace_id IN (...)`；缺省时 `workspace_id` 不受限。
- `archived`：默认为 `archived = 0`；`includeArchived = true` 时去掉该过滤（枚举要全量）。
- `childOf`：`custom LIKE '%' || ? || '%'` where ? = 形如 `"parent_session_id":"<id>"` 的
  子串（先按 JSON 文本匹配；若会话内再升级为独立列）。
- 排序：`ORDER BY updated_at DESC, id DESC`（id 做 tie-break，游标稳定）。
- 分页：`LIMIT ?`（额外取 N+1 判断 hasMore）；`before_id`/`after_id` 用「(updated_at, id)」
  复合游标定位。

```sql
-- 示例（workspace 过滤 + 未归档 + 倒序 + 分页）
SELECT * FROM sessions
WHERE workspace_id = ? AND archived = 0
ORDER BY updated_at DESC, id DESC
LIMIT ? ;
```

### countActive

```sql
SELECT COUNT(*) FROM sessions
WHERE workspace_id IN (...) AND archived = 0;
```

聚合下推 SQLite，不再遍历扫描。

### 冷启动行为

SQLite 文件已持久落盘，开库即 `LIST`/`GET`，`ORDER BY` 直接走索引。**无需回源、无需重建**。

## 6. reconcile（启动收敛） 与回退

### reconcile()

- 调用时机：`AppService` 作用域初始化后、首个请求前，后台运行一次。
- 行为：扫描 `<sessions>/<workspaceId>/<sessionId>/` 目录树，与 SQLite 比对：
  - 目录有 session 而 SQLite 无 → 读其 state.json → `INSERT OR IGNORE` 补入 summary。
  - SQLite 有 session 而其目录不存在（孤儿）→ 删除/标记（避免列出已删 session）。
  - 已存在且一致的 → 不动（避免无谓写）。
- 只跑一次，之后靠 write-through。

### 回退（fallback）

- SQLite `open` 失败或损坏时，`SqliteSessionIndex` 无法提供服务，`ISessionIndex` 则回退到
  `FileSessionIndex`（legacy，不变）。
- 这是纯守 韧性：信任单进程 SQLite；不做复杂熔断/降级状态机。

### 多进程

- 一次一个 SEA 实例的主进程（目前 v2 单 SEA）。SQLite 单文件、WAL 模式允许多读；若未来
  多进程并发写，reconcile 兜底 + WAL 乐观锁。文档里记录为已知 tradeoff，当前不引入
  多写协调。

## 7. 测试计划（Given-When-Then）

### 单测（新文件，建议 `test/app/sessionIndex/sqliteSessionIndex.test.ts`）

- given 一个空库，when 插入多个 workspace 的 session，then 按 updatedAt 倒序返回
- given 未归档与已归档 session，when list 未带 includeArchived，then 只返回未归档
- given 某 workspace 的 session，when list 带 workspaceIds 过滤，then 只返回该 workspace
- given 多个 session，when 用 before_id/after_id 游标分页，then 翻页不丢不重
- given 带 parent_session_id 的 custom，when childOf 过滤，then 只返回子会话
- given 空库，when get 不存在的 id，then undefined
- given 库中已含部分 session，when reconcile 遇到目录新增 session，then 补入；目录已删 → 清孤儿
- given 无效 state.json，when 读取，then 跳过该 session 不崩溃

### 集成

- 替换 `ISessionIndex` 为 `SqliteSessionIndex` 后，现有
  `test/app/sessionIndex/sessionIndex.test.ts` 全绿。
- `workspaceAliases` / `workspaceResources` / 无回归（它们不依赖具体实现细节）。

### 回归（保险）

- SQLite 库损坏或打开失败 → `ISessionIndex` 绑定 `FileSessionIndex`，行为不变。

## 8. 范围边界（明确不做）

- 不引入第三方 sqlite 依赖：用 Node 内置 `node:sqlite`。
- 不动每个 session 的读写内容路径：只改 session list 的**读索引**，不重写 message/transcript
  存储。
- 不动 `ISessionIndex` 接口；不改 v1 行为。
- 不做复杂多写协调 / 熔断状态机（当前单进程写 + reconcile + read-model fallback 足够）。
- 本次不把 `custom` 提成独立索引列（如需要 childOf 高频再演进）。

## 9. 验收标准

- 冷启动（重启 SEA process 后首测）`GET /api/v1/sessions?workspace_id=<某大 workspace>`
  显著快于现状（readdir + 全量读 + sort 基线），且**无重建等待**。
- 现有 `agent-core-v2` 测试全绿（替换后无回归）。
- `build.sh` 通过（Step 0–14 全走）。
- 无新增 npm 依赖（仅用 `node:sqlite`）。