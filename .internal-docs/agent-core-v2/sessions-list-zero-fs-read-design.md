# Sessions 列表冷启动零磁盘读（手动刷新统一失效）

- 日期：2026-08-04
- 分支建议：`feat/sessions-list-zero-fs-read`
- 前置：`.internal-docs/agent-core-v2/startup-lazy-and-refresh-design.md`（Phase 5，已落地）
- 范围：v2 引擎（`agent-core-v2` + `kap-server`）的 `GET /api/v1/sessions` 冷启动首请求

## 1. 背景与现象

用户实测（Desktop 端口 58827，v2/kap-server 引擎）：

- 冷启动 server 后 10s 内的**第一个** `GET /sessions?page_size=3&exclude_empty=true&workspace_id=...`：server 端 `responseTime` = **2439ms / 2489ms**
- 同一个 server（PID 8766）跑 12 分钟后，同一请求：**1.7ms / 1.8ms / 4.3ms**
- 冷启动时间线里，凡是有磁盘读的请求都慢：`/assets/katexRenderer.worker.js` 2.1s、`/assets/mermaidParser.worker.js` 1.9s；不碰磁盘的 `/tasks` 轮询全程 <2ms

SQLite 查询本身 0.35ms（`node:sqlite` `DatabaseSync` 同步执行、不占 libuv 线程池）。原始文件读取
`workspaces.json` 0.16ms、`session_index.jsonl` 0.5ms。**结论（已与用户确认）**：2.5s 不是 SQL 慢、
不是文件内容慢，而是冷启动瞬间 **libuv 线程池被启动期后台 I/O 占满**，`fs.readFile` 排队秒级；
sessions 请求路径上恰好有 3 次 `fs.readFile`（见 §2），因此被放大到 2.4s。

目标：**让 sessions 请求路径上的 fs 读归零** —— 冷启动第一个请求不再依赖磁盘，直接命中
预热好的内存缓存；任何"刷新能看到最新"的语义由写时失效 + 手动 refresh 统一失效兜底。

验收（真实动作验证，不做猜测性验证）：

- A1 冷启动 dev server → `/healthz` 后立刻打 `GET /sessions?workspace_id=X`，server
  `responseTime` < **50ms**（期望 1–5ms）；连续 10 次全部 <50ms
- A2 `GET /sessions` 请求处理过程中不再触发对 `workspaces.json` / `session_index.jsonl` 的任何
  `readFile`（计数器/探针断言）
- A3 手动刷新闭环：运行期 v1 写入磁盘（新 session / 新 alias bucket）→
  `POST /workspaces/{id}/refresh` → 后续 `GET /sessions` 立即反映（含**新 alias bucket** 场景，
  现状不可见，见 §3.3）
- A4 单测 + typecheck + 既有回归（`agent-core-v2`、`kap-server`、`mirri-web`）全绿
- A5 不改变 wire 协议：请求/响应结构、错误码、`has_more`/cursor 语义与现状逐字段一致

## 2. 现状事实（已核实的代码事实）

### 2.1 `GET /sessions` 处理链（`packages/kap-server/src/routes/sessions.ts:370-471`）

```ts
const workspaces = await core.accessor.get(IWorkspaceService).list();        // ① fs.readFile(workspaces.json)
...
const workspaceIds = raw.workspace_id === undefined ? undefined
  : await core.accessor.get(IWorkspaceAliases).resolveAliasIds(raw.workspace_id); // ② workspaces.json(再读) + session_index.jsonl
const page = await core.accessor.get(ISessionIndex).list({ workspaceIds, ... }); // ③ SQLite，0.35ms，不占线程池
```

之后（cwd 过滤、cursor、`toWireSession`、`resolveSessionFacts`）全部同步、无 I/O。

### 2.2 两个 150ms TTL 内存缓存（`packages/agent-core-v2/src/app/...`）

- `WorkspaceService`（`workspace/workspaceService.ts`）：`LIST_CACHE_TTL_MS = 150`
  `listCache`，写路径（`createOrTouch`/`update`/`delete`）调 `invalidateListCache()` **自清**。
- `WorkspaceAliasesService`（`workspaceAliases/workspaceAliasesService.ts`）：`CACHE_TTL_MS = 150`，
  两个独立缓存 `catalog`（= workspaces.json 解析）+ `indexEntries`（= session_index.jsonl 解析），
  **无任何失效入口**，只靠 TTL 过期；且 catalog 与 `WorkspaceService` 各自独立读同一个
  `workspaces.json`（重复读）。

TTL=150ms 而 UI 每 ~1s 轮询 → 每次请求都过期 → 每次请求都走 3 次磁盘读 → 冷启动线程池饱和时被放大成秒级。

### 2.3 启动预热现状（`packages/kap-server/src/start.ts:306-332`）

启动时 `await core.accessor.get(IWorkspaceService).list()`（预热 listCache，但 150ms 内过期失效）+ `await index.reconcile(diskSource)`（SQLite 收敛）。`WorkspaceAliasesService`（注册为 `ScopeActivation.OnScopeCreated`）虽在启动时被构造，缓存却是冷的。

### 2.4 session_index.jsonl 的写者（v2 引擎）

`SessionLifecycleService.appendSessionIndexEntry`（`workspace/sessionLifecycle/sessionLifecycleService.ts:293-301`）
在 `create()`/`fork()` 时 append（append-only，无 delete/rewrite）。v1 引擎（`agent-core`）也写同名文件，
无跨进程锁，O_APPEND 单行原子。

### 2.5 SQLite 写透

`SessionMetadata.mirrorToSqliteIndex()`（`session/sessionMetadata/sessionMetadataService.ts`）在
create/update 时写透 SQLite `sessions` 表，`cwd` 可靠填充（v1 遗留 session 由 reconcile 的
`recoverCwd` 回填）。**SQLite 是 sessions 权威读模型，本设计不动它。**

## 3. 设计

### 3.1 缓存生命周期：永久 + 写时失效

把"150ms TTL 到期"从这两个缓存的失效机制中移除，改为**进程生命周期有效，仅靠显式失效**：

1. `WorkspaceService`：
   - `LIST_CACHE_TTL_MS` 语义改为"永久"（去掉时间戳比较，cache 存在即命中）
   - 保留 `createOrTouch`/`update`/`delete` 的 `invalidateListCache()`
   - 新增暴露 `getCatalogSnapshot(): Promise<{ byId, workspaces }>`（复用内部缓存），供 alias 服务复用
2. `WorkspaceAliasesService`：
   - 删除自己的 catalog 缓存与 `store.load()`，改为从 `WorkspaceService.getSnapshot()` 取
     （消除重复磁盘读）——catalog 侧**从此无自有缓存**，其新鲜度天然等于
     `WorkspaceService` 的缓存（写时失效已覆盖），无额外失效需求
   - `indexEntries` 缓存改为永久 + 失效通知（见 3.4）
   - 新增公共 `clearCaches()`（仅清 `indexEntries`；catalog 无缓存）
3. 外部进程（v1 daemon 共享 HOME）写入：**不轮询、不 TTL** —— 由手动 refresh 统一失效兜底（§3.3）

### 3.2 启动预热

`start.ts` 启动序列补 alias 预热（与现有 `list()` 并行化）：

```ts
await Promise.all([
  core.accessor.get(IWorkspaceService).list(),        // 现有
  core.accessor.get(IWorkspaceAliases).prewarm(),    // 新增：填充 indexEntries 缓存
]);
```

`prewarm()` = 加载 `session_index.jsonl` 到 `indexEntries` 缓存（可与启动 reconcil 共用一次读取结果）。

### 3.3 手动 refresh 成为统一失效入口

现状 `POST /workspaces/{id}/refresh`（`packages/kap-server/src/routes/workspaces.ts:309-341`）只调
`SqliteSessionIndex.refreshWorkspace([id])`，不碰任何内存缓存，且收敛逻辑只扫**单个 bucket**
（`convergeWorkspace`，`sqliteSessionIndex.ts:179-217`），不展开 alias。

改动：

1. `refreshWorkspace(workspaceIds)` 内部先用 `resolveAliasIds(ws)` 展开 alias 集合，再对每个
   alias bucket 执行现有 `convergeWorkspace` 收敛（每个 alias 的 insert/remove 计入返回）
   —— 新 alias bucket 的 session 刷新后首次可见
2. refresh 完成后**统一失效**缓存：`WorkspaceService.invalidateListCache()` +
   `WorkspaceAliasesService.clearCaches()`（新增）—— 保证后续 `GET /sessions` 的 workspace 校验、
   alias 解析都基于最新
3. 实现选型（用户已定）：**直接调用，不走事件总线** —— `WorkspaceAliasesService.clearCaches()`
   做成公共方法，**refresh 路由直接调它；`WorkspaceService` 的写路径不退调 alias 失效
   （catalog 无自有缓存，天然跟随 `WorkspaceService` 失效，见 §3.1），避免 `WorkspaceService → AliasesService`
   循环依赖**

顺带关闭的现有 bug（§5）：新 alias bucket 出现 → 现状 refresh 后仍不可见（两层原因：收敛不
扫sibling bucket + alias 缓存永不过期）。

### 3.4 会话生命周期的失效通知

`WorkspaceAliasesService.indexEntries` 缓存需要在新 session 创建时失效（append-only，只在
create/fork 变多）：

- 注入 `IWorkspaceLifecycleService` 并订阅 handler 生命周期（`onDidCreateSession` / `onDidForkSession`，
  经 `followWorkspaceHandlers`），收到事件 → `clearIndexCache()`
- 或者由 `SessionLifecycleService.appendSessionIndexEntry` 成功后直接调
  `WorkspaceAliasesService` 的失效（修改了 core 写路径）

权衡后采用前者（订阅，不改写路径，避免反向依赖）。若因 handler 生命周期在 App scope 不可达，
退化为在 route 层（create/fork route 调用）失效并记录。此节实现细节在 plan 阶段验证 DI 可达性。

### 3.5 错误处理与边界

- `prewarm()` 失败：日志 warn，首个请求仍按旧路径读盘一次（首请求慢≤读盘时长），下一个请求
  命中缓存
- `WorkspaceService.getSnapshot()` 在 `ensureMerged` 未跑时仍先 ensure（保持现有语义）
- 缓存永久化后进程内 staleness：写路径全部覆盖（workspace 写、session create/fork、refresh）；
  未覆盖到的写（理论上 v1 外部进程写 `session_index.jsonl`）由手动 refresh 兜底，可接受的延迟
- `fallback`（SQLite open 失败 → `FileSessionIndex`）路径不受影响，缓存语义相同

## 4. 涉及文件（草案）

| 文件 | 改动 |
|---|---|
| `packages/agent-core-v2/src/app/workspace/workspaceService.ts` | 永久缓存；暴露 `getSnapshot()`（供 alias 复用） |
| `packages/agent-core-v2/src/app/workspaceAliases/workspaceAliasesService.ts` | 删自有 catalog；永久 indexEntries 缓存；`clearCaches()`/`prewarm()` |
| `packages/agent-core-v2/src/app/workspaceAliases/workspaceAliases.ts` | 接口扩展（可选） |
| `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts` | `convergeWorkspace` alias 展开（或收敛器循环） |
| `packages/kap-server/src/routes/workspaces.ts` | refresh 后统一失效 |
| `packages/kap-server/src/start.ts` | 启动预预热 `prewarm()` 并行 |
| `packages/kap-server/src/routes/sessions.ts` | 不改（只验证） |

## 5. 验收场景（测试）

- 单测（agent-core-v2）：
  - `WorkspaceService`：无 TTL 后两次 `list()` 触发 1 次 `store.load`（读数计数器）；写操作后 invalidate
  - `WorkspaceAliasesService`：`resolveAliasIds` 两次调用不重读（仿数据计数的 storage spy）；`clearCaches()` 后重读；create 事件后 indexEntries 失效
  - refresh：新 alias bucket → `refreshWorkspace` 展开 → `list` 出现新 session（数据）
- kap-server e2e（dev server 冷启动）：A1 首次 /sessions <50ms；A2 计数无磁盘读；A5 wire 协议跑既有 sessions e2e 套件
- mirri-web：既有 workspace-state 测试回归（路径上 server 端接口不变）

## 6. 非目标（明确不做）

- 不动 SQLite schema、wire 协议（`sessions` 表、cursor 语义）
- 不做线程池扩容（`UV_THREADPOOL_SIZE`）与静态资源缓存（assets worker 的 2s 是另一问题，影响首屏而非 sessions 协议）
- 不迁移 alias 解析到可配置的纯 SQLite（`cwd` 覆盖了但仍保留 catalog-only alias，复杂度不值当；现状已足够）
- 不给 v1 引擎（`packages/server` + `agent-core`）改造——v2 是 Desktop/web 的 target

## 7. 实现偏差与实测（实现完成后记录）

- **alias 展开放到 refresh 路由层**，而非 `SqliteSessionIndex.convergeWorkspace` 内：
  `refreshRoute` 先用 `resolveAliasIds(workspace_id)` 展开，再 `refreshWorkspace(expanded)`。
  避免 session index 反向依赖 `IWorkspaceAliases`（路由层组合更干净，且 `refreshWorkspace` 本就收多 id）。
- **refresh 顺序修正**：必须**先失效两个缓存再 resolve alias**——外部 v1 可能刚写入
  `workspaces.json`，若先用旧 catalog 快照解析别名，新 workspace 会 404。实现为
  `workspaceService.invalidateCache()` + `aliases.clearCaches()` → resolve → converge。
- **session create/fork 失效走路由层兜底**（设计 §3.4 的 fallback）：三个
  `event.session.created` publish 后紧跟 `aliases.clearCaches()`，未做 handler 生命周期订阅。
- **A6 交付产物**：`./build.sh` 全绿，产出 `apps/mirri-desktop/dist-app/`（DMG + zip + 未签名）。
- **实测**（clean HOME，30 sessions，冷启动 dev server）：
  - 第一个 `GET /sessions` server 端 responseTime **0.5ms**（改动前同配置 2439ms）
  - 连续 10 次 0.6–1.3ms，全部 < 50ms
  - 把 `workspaces.json` + `session_index.jsonl` 移走后再请求，列表仍返回 30 条（内存缓存证明 A2）
  - manual refresh：新 session 同 bucket 与新 alias bucket 均**刷新后可见**（A3），
    alias 展开日志 `workspaceIds: [lower, typed]` 证实兄弟 bucket 被收敛
  - agent-core-v2 5052 测试、kap-server 982 测试全绿，两个包 typecheck 通过