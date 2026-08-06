# Startup Speed Phase 5：懒加载 + sqlite 权威读 + 手动刷新

- 日期：2026-08-04
- 分支：`feat/startup-speed-phase4`（同一工作树下继续）→ 建议新分支 `feat/startup-speed-phase5`
- 前置：`.internal-docs/agent-core-v2/read-path-phase4-design.md`（读路径收敛，已落地）

## 1. 背景与目标

phase-4 把 `GET /sessions` 的服务端成本压到了毫秒级（冷启动 18 并发实测 14–740ms，
热路径 ~1–5ms）。但用户在 Desktop 里的**感知启动**仍是「服务起来了 → 交互页能看 → 各
workspace 会话列表还在慢慢冒」。两者之间剩下的差距不在服务端单请求耗时，而在：

1. 前端把 18 个 workspace 的会话请求**一次性并发**发出，冷启动的首批请求一起分摊
   JIT/sqlite 预热（每个首请求仍有百毫秒级收敛），且没有「先看到最重要的」的递进感；
2. 读路径每次请求仍做一次 disk 校验（`syncFromDisk` + mtime stat / 变更时全扫），
   即使 phase-4 已把它压到毫秒级，它依然让「sqlite 是真相」这个模型不成立。

本次目标（用户在讨论中已拍板的方案）：

- **前端懒加载**：首个 workspace（`last_opened_at` 最新，即 `/workspaces[0]`）先出
  最新 3 条；其余 workspace 保持折叠，按 lastOpenedAt 顺序**以 3 并发**补齐；
- **服务端权威读**：`list` / `countActive` 不再做每请求 disk 校验，纯读 sqlite；
- **手动刷新兜底**：工作区标题旁新增刷新按钮，用户显式触发「把该 workspace 从磁盘
  重新收敛到 sqlite」。

验收（全 = 真实动作验证，不做猜测性验证）：

- A1 回归判据：冷启动 dev server 后并发打 18 个 workspace 的 `/sessions` 全部 ≤2s；
- A2 `list`/`countActive` 在启动 reconcile 完成后的**任意次读**不再触发磁盘枚举
  （单测计数器验证）；
- A3 手动刷新：运行期 v1 写入磁盘 → `POST /workspaces/{id}/refresh` → 新 session 出现在后续 list；磁盘删除 → refresh 后从 sqlite 移除；
- A4 单测 + typecheck + 既有回归全绿。

## 2. 现状事实（已核实的代码事实）

### 前端（`apps/mirri-web`）

- `useWorkspaceState.ts`：
  - `SESSIONS_INITIAL_PAGE_SIZE = 5`，`SESSIONS_LOAD_MORE_SIZE = 30`，
    `SESSIONS_RECENT_WINDOW_MS = 12h`；
  - `load()` 里 `loadWorkspaces()` 之后 **background** 调
    `loadInitialSessionsByWorkspace()`（首帧不等待列表）；
  - `loadInitialSessionsByWorkspace()` = `Promise.allSettled(workspaces.map(...))`，
    **全部 ws 并发**；每个 ws 走 `loadInitialSessionsForWorkspace()`：首页 5 条后，
    若最旧一条仍在 12h 窗口内就继续翻页（活跃 ws 可能一上来拉多个 5 条页）；
  - 结果合并后全局按 `updatedAt desc` 排序；`sessionsHasMoreByWorkspace` /
    `sessionsCursorByWorkspace` / `sessionsInitialCountByWorkspace` 三个 map 播种。
- `Sidebar.vue`：`expandedIds`（组内「超过首屏的分页展开」）默认全折叠；`WorkspaceGroup`
  折叠时渲染首屏切片（inert）；`sessionCount` 徽标来自 workspace 列表。
- `/workspaces` 已按 `last_opened_at desc` 返回（`workspaceService.ts` 每次 touch 写
  `lastOpenedAt: now`；前端 `mergeWorkspaces.ts:54` 注释确认）。Wire 类型
  `WireWorkspace.last_opened_at` 已有。
- 测试落点：`apps/mirri-web/test/workspace-state.test.ts`（useWorkspaceState 测试已存在）。

### 服务端（`packages/agent-core-v2`）

- `sqliteSessionIndex.ts`：
  - `list()`（L189）与 `countActive()`（L216）每请求调 `syncFromDisk({workspaceIds})`；
  - `syncFromDisk` = 按 scope 包干 enumerateWorkspace：有 `statWorkspaceMtime` 快照 +
    in-flight 去重，无变化只 stat；
  - `get()` 已不 sync（L204-214）；
  - `reconcile(source)`（启动）为带删除的全量收敛；
  - `enumerateWorkspace()` 目前是 **insert-only**（缺孤儿删除）。
- `sqliteSessionStore`：`idx_sessions_ws_updated (workspaceId, updatedAt DESC)` 已建，
  per-ws `ORDER BY updatedAt DESC LIMIT n` 是索引查询。
- 路由：`packages/server/src/routes/workspaces.ts`（GET/POST/DELETE + rename），
  sessions 在 `routes/sessions.ts`（`?workspace_id=&page_size=` 已支持，page_size 最小 1）。
- 测试落点：`packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts`
  已存在（含 disk source 枚举计数器 seam）。

## 3. 决策记录（用户拍板）

| 决策点 | 结论 |
|---|---|
| 其余 workspace 加载节奏 | 首个 ws 加载完 → 其余按 lastOpenedAt 顺序**3 个并发队列**（不回退到严格全串行） |
| 初始会话条数 | **3 条**（page_size=3，非 5） |
| 初始是否继续 12h 窗口翻页 | **否**（初始只 1 页=最新 3；翻页只发生在用户「展开/加载更多」时） |
| 读路径 | **纯 sqlite 权威读**，去掉 list/count 的每请求 syncFromDisk |
| 漂移防护 | **手动刷新**（前端按钮）为唯一运行期入口；**不做**后台 watcher / 周期 dirty check |
| v1/v2 兼容 | 保留启动 reconcile（含删除）；刷新端点对 FileSessionIndex（fallback v1 路径）无操作（其读本就实时） |

## 4. 前端设计

### 4.1 懒加载：priority-first + 3-worker 队列

**文件**：`apps/mirri-web/src/composables/client/useWorkspaceState.ts`

替换 `loadInitialSessionsByWorkspace()` 的内部为：

```
const [priority, ...rest] = workspaces          // workspaces 已按 lastOpenedAt desc
await loadInitialSessionsForWorkspace(priority.id, { initialOnly: true })  // page_size=3, 不翻页
// then 3 workers 消费 rest（保序，按数组顺序）
```

- `loadInitialSessionsForWorkspace(ws, { initialOnly: true })`：
  - `pageSize = SESSIONS_LAZY_INITIAL_PAGE_SIZE (=3)`；
  - `initialOnly` 时**不执行** RECENT_WINDOW 续页循环（直接取首页）。
- worker 实现：一个简单队列（`shift()` 取任务，固定 3 个协程），结果聚合方式复用现有
  `successfulPages / failedWorkspaceIds / hasMore / cursors / counts` 播种逻辑；
  失败语义不变：单个 ws 失败不影响其他，全失败返回 `undefined` 保留旧数据。
- 折叠语义沿用现状（`expandedIds` 默认空 = 全折叠；首个 ws 也不强制展开——只是请求
  **顺序**优先，折叠显示首 3 条）。
- `sessionsInitialCountByWorkspace[ws] = 3`（对齐新 page_size）。
- 注意：`initialOnly` 是本分支对「启动裁剪」的明确取舍；「展开某工作区时看到完整近期
  历史」由 `loadMore`（已存在，before_id 翻页）+ 手动展开保留。

### 4.2 手动刷新按钮

**文件**：`useWorkspaceState.ts`、`Sidebar.vue`、`WorkspaceGroup.vue`、`App.vue`

- `WorkspaceGroup` header 的 ws menu 旁新增刷新 icon 按钮；`emit('refreshWorkspace', wsId)`；
  `refreshing` 时为 spinner（disabled）。
- `useWorkspaceState` 新增：
  - `refreshingWorkspaces: Set<string>`（对外暴露 `workspaceRefreshingIds` prop）；
  - `async refreshWorkspaceSessions(workspaceId)`：
    1. `await api.refreshWorkspace(workspaceId)`（新 client 方法 → POST `/workspaces/:id/refresh`）；
    2. `await loadWorkspaces()`（刷新 count 徽标，走现有 /workspaces）；
    3. `await loadInitialSessionsForWorkspace(id, { initialOnly: true })`（首页 3 条）；
    4. 合并：从 `rawState.sessions` 移除该 ws 旧项 + 混入新首页（复用
       `setSessionsPreservingLiveUsage` / 去重复用 `mergePartialSessionsWithCached` 思路），
       更新 hasMore/cursor/count map；
    5. 失败 → `pushOperationFailure('refreshWorkspace', error)`，保留原数据。
  - 并发去重：同一 ws 刷新进行时重复点击直接返回。
- `client.ts` / `api/types.ts`：加 `refreshWorkspace(workspaceId)`。

### 4.3 前端测试（`apps/mirri-web/test/workspace-state.test.ts` 增补，Given-When-Then）

- Given 3 个 workspace / When 初始加载 / Then 首个 ws 的请求先发（mock 调用顺序），
  且并发窗口最大 = 3（同一时刻在飞请求数 ≤3）；
- Given 首个 ws 首页 has_more=true 且最近会话在 12h 内 / When 初始加载（initialOnly）/
  Then 只发 1 页（不再续页）；
- Given 用户点击刷新 / When `refreshWorkspaceSessions` / Then 调 POST refresh + 重新拉首页
  + 该 ws 的旧 sessions 被替换、其他 ws 不受影响；
- Given refresh 进行中再次点击 / Then 幂等（只一次请求）。

## 5. 服务端设计

### 5.1 权威读

`packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts`

- `list()`：删除 `await this.syncFromDisk(...)`（L192-195 移除；保留 `ensureOpen` + fallback 分支）；
- `countActive()`：删除 `await this.syncFromDisk({ workspaceIds })`；
- `get()` 保持现状；
- 更新类注释与 `syncFromDisk`/`enumerateWorkspace` 的 doc comment：它不再是「read 前的
  对账」，而是 **reconcile / refresh 专用**的收敛原语。
- `_fallback`（sqlite 打不开）模式照旧委托 `legacy`（FileSessionIndex 直接读盘，无需改语义）。

### 5.2 refresh 能力

- `ISessionIndex`（`sessionIndex.ts`）新增方法
  `refreshWorkspace(workspaceIds: readonly string[]): Promise<void>`；
- `FileSessionIndex`：no-op（其读路径本就实时，无可收敛）；
- `SqliteSessionIndex.refreshWorkspace`：
  - 复用同一个 `syncFromDisk({workspaceIds})` wrapper（并在 wrap 里保留 in-flight
    去重与 `workspaceDirMtimes` 快照语义）；
  - **但**把 `enumerateWorkspace` 从 insert-only 升级为**带孤儿删除的 workspace-scoped
    converge**（与启动 `reconcile` 同一语义）：
    - 磁盘有、库里无 → upsert；
    - 库里无、磁盘有 → remove；
    - 两边都有且 stateMtime 变化 → re-read update；
    - 相同 → skip。
  - 实现方式：将 `reconcile()` 的 per-workspace 收敛逻辑抽成
    `private convergeWorkspace(source, ws)` 复用，`reconcile()`（全量）与
    `refreshWorkspace(ids)`（scoped）都调它。启动 reconcile 的内存量（增量跳过相同
    mtime）保持不变。
- 路由 `packages/server/src/routes/workspaces.ts`：
  - `POST /workspaces/{workspace_id}/refresh`：直接
    `ix.get(ISessionIndex).refreshWorkspace([workspace_id])`，返回
    `okEnvelope({ refreshed: true })`。
  - workspaceId 即 `sessionsDir` 下的目录名（`sqliteDiskSource.ts` 的
    `listWorkspaceIds`/`statWorkspaceMtime`），不需要
    `IWorkspaceRegistry` 存在性校验；目录不存在时收敛为「空」并清掉库里残留，同样返回
    ok（幂等）。

### 5.3 一致性模型（明示的「放弃点」）

| 场景 | 行为 |
|---|---|
| 本进程写 session（API/agent） | 写库同步，即时可见（不变） |
| 启动前磁盘已有（v1 遗留） | 启动 reconcile 收敛（不变，增量） |
| **运行期 v1/CLI 并发写** | **不可自动可见**；用户点手动刷新后才收敛 —— 已用户拍板 |
| 用户手动删/改会话文件 | 同上，refresh 后删除/更新 |
| 进程崩溃残留 | 下个启动 reconcile 兜底 |

不引入：文件 watcher、周期 dirty check、`/sessions` 读时的隐式对账。这是当前设计明确
的简化的「真相边界」：**sqlite = 权威；磁盘 == 变更源，仅在 reconcile（启动）与 refresh
（手动）时被搬到 sqlite**。

### 5.4 服务端单测（`sqliteSessionIndex.test.ts` 增补）

- Given 启动 reconcile 完成后 / When 连续 `list` + `countActive` /
  Then disk 枚举计数器保持 0（零磁盘调用断言）；
- Given 运行期 v1 新写一个 session 到磁盘 / When `refreshWorkspace([ws])` /
  Then 该 session 出现在后续 list；且再次 `list` 不再枚举（快照生效）；
- Given 磁盘上某 session 目录被删 / When `refreshWorkspace([ws])` / Then 从 sqlite 移除
  （孤儿删除生效；对照组：不 refresh 时仍返回旧数据）；
- Given refresh 与 list 并发 / When … / Then 只发生一次枚举（in-flight 去重）。

## 6. E2E（真实 dev server，验收 A1/A3）

复用 `.tmp/e2e-phase4.mjs` 骨架新建 `.tmp/e2e-phase5.mjs`：

1. 全新临时 HOME + 种子：按 v1 磁盘格式创建 **18 个 workspace × 每个 3–8 个 session**
   （写真实的 `session.jsonl` / `state.json` 等文件，不经过 v2 写路径模拟 v1 遗留）；
2. 冷启动 `pnpm dev:v2 --log-level info`（真实端口、真实 tsx 入口）；
3. 等 server ready 后**立即**并发打 18 个 `/sessions?workspace_id=X&page_size=3&exclude_empty=true`
   → 断言全部 ≤2s；
4. 运行期 v1 写入：另起进程/脚本在该磁盘上新增一个 session 目录 + 文件 →
   直接 `GET /sessions` 断言**不可见** → `POST /workspaces/{id}/refresh` →
   再 `GET /sessions` 断言**可见**（验证权威读 + 手动刷新闭环）；
5. 清理：kill server 进程 + 删除临时 HOME。整个脚本退出码非 0 即失败。

## 7. 验收标准（数字）

- A1：18 并发冷启动 ≤2s（回归）
- A2：权威读零磁盘调用（单测断言）
- A3：refresh 闭环通过（单测 + e2e）
- A4：`agent-core-v2` 与 `apps/mirri-web` 相关单测绿；两包 typecheck 绿；
  `apps/mirri-web` 相关单测绿

## 8. 风险与缓解

- 运行期静默漂移（v1 并发写）不再自动可见：已被用户明确接受；**首屏有时效性提示的
  场景**（会话状态、活跃流）仍走 WS 事件流不受影响；
- 初始从 5 条改 3 条：活跃 ws 折叠首屏内容变少；「展开/加载更多」原逻辑保留，清晰可发现；
- 3-worker 队列尾部：18 ws 量级最后一个 ws 等待约「首批 1 + 剩余 17/3×单请求耗时」；
  热路径单请求 ~ms 级，最差 ~百毫秒级，循环可接受（如进化成 50+，才动用「仅渲染最新
  N 个 ws」的进一步裁剪，本轮不做）。
- 前端 mock 断言在 workspace-state.test.ts 里依赖请求顺序：需要把 api mock 换成可
  记录调用顺序的 version（现有测试已 mock `getMirriWebApi`，扩展即可）。

## 9. 明确不做（YAGNI）

- 前端「仅加载最新 N 个 ws 其余全 3」的进一步裁剪（>50 ws 场景才考虑）；
- 后台 watcher / 周期 dirty check / 全量缓存预热；
- 改 `/sessions` 响应 schema 或 wire 类型；
- 改启动 reconcile 的既有增量逻辑（保留）。

## 10. 交付物清单（实现时）

- src：
  - `packages/agent-core-v2/.../sessionIndex/sessionIndex.ts`（接口 + 注释）
  - `.../sqliteSessionIndex.ts`（list/count 去 sync；convergeWorkspace；refreshWorkspace）
  - `.../sessionIndexService.ts`（FileSessionIndex.refreshWorkspace 实现）
  - `packages/agent-core-v2/src/session/sessionMetadata/sessionMetadataService.ts`（**新建会话时立即 mirror 到 sqlite 索引**——权威读后，无 title 的 create 必须立即可见，load() 的新建分支补了一次 write-through）
  - `packages/kap-server/src/routes/workspaces.ts`（POST /workspaces/{id}/refresh 路由 + 协议 schema）
  - `apps/mirri-web/src/composables/client/useWorkspaceState.ts`（懒加载 + 刷新）
  - `apps/mirri-web/src/api/daemon/client.ts`、`apps/mirri-web/src/api/types.ts`（refreshWorkspace）
  - `apps/mirri-web/src/components/WorkspaceGroup.vue`、`Sidebar.vue`、`App.vue`（按钮/emit）
- test：
  - `packages/agent-core-v2/test/app/sessionIndex/sqliteSessionIndex.test.ts`
  - `apps/mir-web/test/workspace-state.test.ts`
  - `packages/kap-server/test/sessions-split-buckets.test.ts`、`test/sessions.test.ts`
  - `.tmp/e2e-phase5.mjs`
- 文档：本设计文档 + changeset（minor，gen-changesets 流程）