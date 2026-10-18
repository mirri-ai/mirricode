# 工作区管理 — 现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-11 ·
> 配套设计文档：[workspace-management-architecture.md](workspace-management-architecture.md) · 需求：[workspace-management-requirements.md](workspace-management-requirements.md)
> 本文不描述目标、不含裁决——终态与裁决在需求/架构文档；Rust 引擎侧现状不在此重复（唯一源：[rust-core-design/context.md](rust-core-design/context.md) §6，含单根假设载体与多工作区地基两张清单）；执行状态见 [workspace-management-ROADMAP.md](workspace-management-ROADMAP.md)。

## 1. TS v2 引擎侧服务现状（packages/agent-core-v2）

| 位置 | 事实 |
|---|---|
| `src/app/workspace/workspace.ts:12-60` | 契约：`Workspace {id, root, name, createdAt, lastOpenedAt}`；`IWorkspaceService` = list / getSnapshot（未去重原始）/ invalidateCache / listWithSessionCounts / get / createOrTouch / update / delete |
| `src/app/workspace/workspaceService.ts:78-118` | 无内存写缓存——每次写操作对 workspaces.json 读-改-写，经 promise 链互斥队列串行；进程级读缓存 `listCache` 由自身写失效（外部写入者仅经手动刷新端点可见） |
| `workspaceService.ts:202-255` | `createOrTouch`：按编号或根键别名幂等复用；lstat + realpath 复核，缺失/非目录报 FS_PATH_NOT_FOUND；新条目 `id = encodeWorkDirKey(root)`、名称缺省 basename、createdAt = lastOpenedAt = now；既有条目只 bump lastOpenedAt；清除删除标记 |
| `workspaceService.ts:276-306` | `delete` 软删：根的全部别名编号（目录 + 旧索引拼写）从名单移除并入 `deleted_workspace_ids`；未知编号也照样 tombstone 自己 |
| `workspaceService.ts:308-369` | 进程首次操作与 legacy `session_index.jsonl` 对账：无名单则按**规范化根键去重**重建；有则并入未知 workDir（重建/合并路径故意跳过根存在性检查） |
| `workspaceService.ts:139-192` | `listWithSessionCounts`：别名集折叠计数（`ISessionIndex.countActive`，非归档） |
| `src/app/workspace/fileWorkspacePersistence.ts:24-83` | `<数据根>/workspaces.json` v1 形状读写经原子文档存储；load 清洗丢弃畸形条目、无可用文件返回 undefined 触发重建；tombstone 往返保留；save 仅由 WorkspaceService 触发——**整文档替换、无跨进程锁、不合并**（SDR-005 残余窗口的 TS 侧成因） |
| `src/app/workspace/workspaceAlias.ts:31-68` | `collectAliasIds`（目录 + 旧索引收集同根全部拼写）/ `dedupeByRoot`（折叠为代表，优先规范派生编号）；旧版 `session_index.jsonl` 解析在 :70-117 |
| `src/_base/utils/workdir-slug.ts:29-43` | 身份规则（**双键结构**）：`encodeWorkDirKey`（编号铸造）仅做斜杠归一 + 去尾斜杠，**大小写保留哈希**（Windows 大小写差异 → 不同编号，这是别名分叉的主要成因）；`workspaceRootKey`（比较键）在其上对 Windows 形态整体小写，仅用于同目录判定（复用/去重/折叠代表），不改写已铸造编号 |
| `src/app/workspaceAliases/workspaceAliasesService.ts:52-90` | `resolveAliasIds`（目录快照 + 索引缓存折叠）；会话创建/fork/child 与刷新路由清缓存 |
| `src/app/workspaceSessions/workspaceSessionsService.ts:26-35` | `count`/`listRecent`（先别名展开再索引查询；recent 上限 20） |
| `src/workspace/workspaceTrust/workspaceTrustService.ts:7-112` | 信任存储：独立文档 scope `workspace-trust`，按 `encodeWorkDirKey(root)` 键；**文档存在即信任**（trust 写、untrust 删）；读失败按未信任；Workspace 域按需物化 |
| `src/app/workspaceLifecycle/workspaceLifecycleService.ts:15-131` | `handlerFor`：编号引用查目录（未知报 NOT_FOUND）、根引用走 createOrTouch；物化幂等（in-flight 合并）；句柄永驻随 App；物化种子 workspaceContext（编号、根、持久化 scope = `sessions/{wd_id}`）；"只有本地工作区才写入 workspaces.json" |

## 2. 信任门控的 TS 现状（gate 位置）

| 位置 | 事实 |
|---|---|
| `src/workspace/workspaceMcpConfig/workspaceMcpConfigService.ts:119-136, 186-207` | 项目层 MCP 加载开关 = `trust.isTrusted()`（初始化与文件/信任翻转热重载两处，`onDidChange` 接线 :87-93）；未信任跳过两份项目文件（git 根 `.mcp.json`、`.mirri-code/mcp.json`），用户全局 + 插件层不受影响 |
| `src/app/mcpConfig/mcpConfigService.ts:281-340` | 设置面：项目层仅在首个已物化且受信工作区可见；`scope:'workspace'` 写入未信任时报错 |
| —（全仓核验） | `IWorkspaceTrust` 消费点仅上述两处 + 信任路由——**AGENTS.md 注入、会话加载等一律不门控**（需求 SDR-002 扩大范围的动因）；**默认未信任**（记录缺失即未信任，`POST /workspaces` 不触发 trust）；信任变化仅**同进程**热重载生效（跨进程需重启感知，`workspaceTrustService.ts:12-14` 文档自述） |

## 3. 服务端路由现状（packages/kap-server）

| 位置 | 事实 |
|---|---|
| `src/routes/workspaces.ts:104-167` | GET / POST /workspaces（POST：绝对路径校验 + stat 目录校验 + createOrTouch） |
| `workspaces.ts:168-234` | PATCH/DELETE（DELETE 不删磁盘内容，"does not remove on-disk content"） |
| `workspaces.ts:236-308` | trust/untrust 三路由（经生命周期服务物化 handler） |
| `workspaces.ts:310-355` | refresh：先失效目录/别名缓存（外部写入者可见）再别名集收敛入 sqlite |
| `workspaces.ts:383-409` | 单行会话数投影：`IWorkspaceSessions.count`（别名折叠） |
| `src/routes/sessions.ts:283-327, 378-402` | 会话创建**总是** `createOrTouch(workDir)` 后经 `handlerFor({root})` 路由（隐式注册）；显式 workspace_id 校验存在且 cwd 与根一致；列表 `workspace_id` 过滤经 `resolveAliasIds` 集合匹配（`sqliteSessionStore.ts:144-151` 持久化列） |
| `src/routes/workspaceFs.ts:118-165` + `agent-core-v2/src/app/hostFolderBrowser/hostFolderBrowserService.ts:32-75` | `fs:browse`（仅目录、realpath、点后缀排序、父级 = dirname）；`fs:home`（$HOME + 前 8 个注册根——**返回了但前端未渲染**）；另有 `fs::mkdir`（非递归新建目录，EEXIST/ENOENT/EACCES 映射，`workspaceFs.ts:197-222`）——**后端就绪、前端零调用**（休眠预埋） |

## 4. 网页端现状（apps/mirri-web）

| 位置 | 事实 |
|---|---|
| `dialogs/AddWorkspaceDialog.vue` | 文件夹选择器：目录树导航 + 面包屑 + 上级；**单输入框三合一**（当前目录模糊搜索，限 600 目录/深 6/150 结果；绝对路径模式含 `~`/盘符/UNC，状态机 checking/valid/not-found/bad-parent + 父目录前缀候选）；浏览服务不可用时退化为纯路径输入；git 徽章 UI 存在但 v2 wire 无字段（休眠） |
| `WorkspaceGroup.vue` + `Sidebar.vue` | 分组动作全集：折叠（本地记忆）、刷新（加载态防重）、组内新会话、拖拽重排（本地序）、复制路径、重命名（内联，IME 守护，空名静默不生效，派生工作区失败回退本地覆盖 `storage.ts:191-201`）、移除（两行确认框，**无数据保留提示、无模式选择**——`deleteHasSessions` i18n 键存在但未使用）；右侧键菜单 = 同集 |
| `useWorkspaceState.ts:2375-2422` | 移除端到端：本地隐藏集合（`mirri-web.hidden-workspaces`，按根路径）+ 尽力 DELETE（失败吞掉记日志）；隐藏是防"派生复活"的机制（`mergeWorkspaces` 会从会话 cwd 重新派生）；WS 删除事件同镜像；重加同路径取消隐藏；活跃工作区被删回落首行或清空 |
| `useWorkspaceState.ts:811-845` | 刷新：POST refresh → 重拉 GET /workspaces → 重拉该工作区最新页合并（旧行替换、去重、重播种子）；失败 toast、旧数据不动 |
| `mobile/MobileSwitcherSheet.vue` | 移动端：切换/新建会话/新建工作区/复制路径/移除（确认框同文案）；**无**刷新/拖拽/排序/内联重命名（重命名走系统 prompt） |
| `useMirriWebClient.ts:2388-2410` + `lib/mergeWorkspaces.ts` | 展示数据：`{id, name, root, shortPath, branch, sessionCount}`；分支仅活跃工作区（会话 git 信息兜底）；派生工作区 `name = basename(cwd)`；会话数 = 本地精确值或服务端计数取大（hasMore 时） |
| 空态 | 零工作区：侧边栏"No workspace" + 居中 composer 的添加入口（`ConversationPane.vue:1400-1408`）；零会话：分组"No conversations yet" + 打开即草稿；新会话撰写区的工作区选择器（当前工作区名 + 下拉 + "更多"折叠 + 添加入口，`ConversationPane.vue:1358-1399`）；**桌面侧边栏的独立添加按钮被硬编码隐藏**（`showNewWorkspaceButton = false`，入口评估中，`Sidebar.vue:594-595`）——已有工作区时桌面添加入口仅在选择器内 |
| 排序与事件 | `GET /workspaces` 返回序 = 注册表插入序（**v2 无排序语义**，网页自行按最近使用排序；两处网页注释声称"daemon 按 last_opened_at 排序"对 v2 已过时）；**v2 后端不发射 workspace 生命周期事件**（`workspaceCreated/Updated/Deleted` 仅 v1 发射 `packages/agent-core/src/services/workspace/workspaceRegistryService.ts:164,185,211`）——网页消费端就绪但 v2 下多客户端同步不生效 |
| **信任 UI** | **全仓无任何信任交互/文案/调用**（grep 核验：仅注释命中）——REST 方法存在、网页零消费（R5 可用性诚实缺口，需求 US4.2 的动因） |

## 5. 覆盖差异矩阵（已文档化 vs 现状剩余）

| 能力 | 文档化 | 现状备注 |
|---|---|---|
| 注册/隐式注册/幂等/身份/别名折叠 | ✅ 需求 E1 | 全覆盖 |
| 列表过滤/分页/排除空会话 | ✅ US1.1/1.6 | 排除空会话的判定细节（lastPrompt）归会话域 |
| 展示/重命名/折叠/空态/移动端 | ✅ E2 | 手动排序本地序的现状（manual/recent 双模式）归会话管理域 M2 迁移 |
| 移除（TS 基线 + 三选一增强） | ✅ E3 | TS 现网仅"本地隐藏 + 尽力删除"（US3.1 如实记录） |
| 信任（门控 + 入口增强 + 范围扩大增强） | ✅ E4 | TS 现网无入口、门控仅 MCP——增强点已在需求标记 |
| 多工作区并发/惰性/钉扎 | ✅ E5 | TS 基线 + Rust 目标（引用 rust-core-design 08） |
| 跨引擎并发/刷新 | ✅ E6/E7 | SDR-005 残余窗口如实记录 |
| v1 消费面 | ⚠️ 待确认 | v1 `services/workspace` 存在；desktop 对工作区的实际消费面未盘点（开放项 #2） |

## 6. 现状痛点（对照需求的差距源）

1. **删除无力度选择**：现网移除一律保数据——占盘用户无从清理（SDR-001 动因）。
2. **信任零入口**：后端机制完备、网页不可达（SDR-002 动因）。
3. **休眠预埋**：fs:home 返回的最近根未渲染、浏览 git 徽章无字段——界面与服务能力错位。
4. **跨引擎并发窗口**：TS 写方无锁整写（SDR-005 声明接受）。
