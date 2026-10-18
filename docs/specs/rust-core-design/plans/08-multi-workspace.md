# Rust 内核域计划 — 08: 多工作区架构实施（multi-workspace）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——本文件只持**任务分解、批次协议与验收标准**。
> 设计（终态架构 + 裁决 D1-D8，rev 2 已吸收独立设计评审发现）：[../08-multi-workspace.md](../08-multi-workspace.md) · 现状证据（单工作区绑定代码事实）：[../context.md](../context.md) §6 · 开放项状态唯一源：[../ROADMAP.md](../ROADMAP.md) 1.8。
> 上游: 吸收 [rust-v2-method-parity](../../../../plan/rust-v2-method-parity.md) B4 中 workspace.* 域部分（人机回路域仍归 B4）。

**Goal:** 按设计文档 [08-multi-workspace.md](../08-multi-workspace.md) 落地多工作区架构，用户可感知验收：rust+web 下添加工作区真实注册、选择工作区真实切换、会话按工作区分组列出，且任意工作区的会话可正常收发消息与订阅事件。

**Tech Stack:** Rust workspace（mirri-core / mirri-embedded / mirri-server / mirri-session-index / mirri-cli）+ TS workspace（mirri-web `ws-rpc-api.ts` 解封 + e2e 验证）。

## 设计基线与裁决门

- D1（一进程多工作区）已裁决（2026-09-11）；设计已经独立评审（NEEDS CHANGES → 修订 rev 2），评审发现 H1-H4/M1-M6 已全部吸收进设计与本计划。
- D2-D8 已全部裁决（用户逐条确认，2026-09-11）：D2 总管模式 ｜ D3 别名集**本期搬入**（否决分期）｜ D4 删除**三选一**（purge 物理删除 + remove 仅移除 + 取消，确认框鼓励清理）｜ D5 注册先行+惰性+钉扎 ｜ D6 默认不信任 ｜ D7 单服务器统一分发 ｜ D8 新引擎单边并发防护。后续设计变更以新 ADR 追加，不原地改写已裁决行。
- 关键执行序约束（现状证据见 context.md §6.1）：
  1. `workspace.*` 四方法当前是 mirri-server 传输注册表 handler、在 engine fall-through 之前拦截——**W2 必须单批内完成"实现 + 解除拦截"原子落地**，不留中间态。
  2. **W3（会话路由层）必须先于 W4（web 解封）**——只路由 create/list 两个入口时，非默认工作区的会话在 message.send/subscribe/审批等派发面不可见，web 解封等于放行一个坏体验。
  3. **W7 的信任加载门与多根 MCP 加载同批落地，且 W2 起新注册工作区一律按 UntrustedDefault 构建 scope 直到信任门就位**（设计 D6 时序硬约束，MCP + AGENTS.md 两半）——否则新注册工作区的 project 层配置无确认自动加载，属安全暴露窗口。
  4. **W3 内完成按 registry 的 reconcile 扩面**——路由层索引回退依赖它：非默认工作区的持久化会话必须先进索引，否则 W4 解封后 resume/翻页对它们 not-found（reconcile 现状只扫默认 bucket，`session_index.rs:170-251`）。

## Global Constraints（继承 07，全文有效）

- 本地门禁：`cargo test --workspace` 全绿、`cargo clippy --workspace --all-targets -- -D warnings` 零告警、mirri-core zero-IO 守卫 0 命中（新代码 IO——含 D8 锁文件——走 trait 注入）、受影响 TS 包 typecheck + vitest 全绿。
- 测试命名 Given-When-Then；不改实现迁就旧测试（除非实现真有 bug）。
- changeset 走 gen-changesets skill；`major` bump 任何时刻需用户确认（硬门）。
- 契约变更同步事实源链全环节（registry ⇄ TSV ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture）。
- wire log append-only；新增工作区状态域必须写侧 + reducer 双侧。
- 环境护栏：跑大 TS 套件前查 swap；每批开始前 `git status` 干净。

## 每批循环（SDD 协议，同 05/06/07）

brief（侦察 + 设计，`.superpowers/sdd/multi-workspace/task-<Wn>-brief.md`）→ TDD（红→绿）→ 评审（含 mutation）→ 本地门禁 → changeset → commit → ROADMAP/本档状态更新 → 批次报告。

## 停止规则（同 07）

用户裁决门 → 记队列跳过；mutation REJECT 两轮 → 暂停上报；环境阻断 → 记录替代验证；新架构问题 → 记 ROADMAP §5 候选不当场改。

## 任务总表

| 任务 | 主题 | 优先级 | 状态 |
|---|---|---|---|
| W1 | WorkspaceRegistry 核心 + 组合根恢复（注册先行/惰性/宿主钉扎） | P0 | ✅ |
| W2 | workspace.* 真语义 + serve 拦截解除 + 注册表写策略 | P0 | ✅ |
| W3 | 会话路由层（session-id → manager 解析缝，全派发面） | P0 | ✅ |
| W4 | session.create 工作区路由 + web 解封 | P0 | ✅ |
| W5 | session.list + session.search 多工作区化（游标/过滤/迁移） | P1 | ✅ |
| W6 | workspace.update/delete + touch（tombstone/去抖） | P1 | ✅ |
| W7 | MCP 多根化 + 信任加载门 | P1 | ✅ |
| W8 | trust 管理 RPC/路由 | P2 | ✅ |
| W9 | 收口批：迁移 e2e、并发 e2e、宿主测试、pin 联动 | P2 | ✅ |

> **计划收口（2026-09-12，W9 落地）**：W1-W9 全部完成，ROADMAP 1.8 状态已关闭（含各批 §5 遗留的 conditional followup 账目）。W9 收口批内容：`mirri serve` 泛化（`--workspace` 可选 + 移除 serve 根 canonicalize）、存量 v0 db 迁移 e2e、跨引擎并发写 e2e（发现并修复 `SessionIndex::upsert_workspace` 写穿镜像的整文档替换丢行缺口——改走 D8 合并写）、别名集 e2e（折叠 + POSIX 大小写/`~` 不折叠）、ACP/NAPI 共享 home 钉扎测试、TSV workspace.* 行收口。

> **范围声明**：协作域（Q1-Q5）、TS-v2 退役（ROADMAP 1.3）、Web 前端默认启用 rust 后端（ROADMAP 1.4）不在本计划内；`fs.browse`/`fs.home`（绝对路径浏览，无单根假设）不动。TS 别名集语义（`IWorkspaceAliases`）按设计 D3 范围声明不在本期，分歧已记录。

### 依赖关系

```
W1 ──→ W2 ──→ W3 ──→ W4 ──→ W5 ──┐
       │            └──→ W6 ──────┼─→ W9
       └──→ W7 ──→ W8 ────────────┘
```

### 跨域协调（session 域，2026-09-11 排序登记）

- **session-management 域 M2/M3 排在本计划 W4/W5 之后**（排序裁决，用户 2026-09-11 确认）：`session.list`/`workspace.list` 的 pinned/分层返回扩展建立在多工作区 listing 与 web 解封之上——先 M2 后本计划会造成同批文件双写；M1（契约登记）为纯增量，可提前至任意时点（建议与本计划 TSV 批次合流，省一次 pin 联动）；U1（对话撤销）与本计划无文件交叉，裁决完开放项即可并行。
- **W5 吸收 persistence 域开放项**：#1（`archivedOnly` 过滤参数）随 W5 过滤面落地；#5（`lastPrompt` 持久化）即 W5 `exclude_empty` 的数据源——需求级裁决已定（用户 2026-09-11，该域 SDR-004：lastPrompt = 最近一条用户输入无论成败 + 惰性物理创建），随 W5 brief 落地实现。
- **别名集吸收**：设计 D3 已裁决（2026-09-11）**别名集语义本期搬入**（否决分期）——persistence 开放项 #6（大小写分裂桶合并读）随之由本计划吸收，不再是"等真实数据再评估"。

---

## W1: WorkspaceRegistry 核心 + 组合根恢复

**优先级**：P0 ｜ **涉及面**：rust（mirri-core / mirri-embedded）｜ **设计依据**：08 §2、§2.2、§4 D2/D3/D5

**范围**：
- 新建 `WorkspaceRegistry`（生产化 AppScope 语义或新建薄类型——brief 裁决，倾向后者避免测试耦合）：持 `HashMap<workspace_id, RegistryEntry>` + **活跃会话表**（session-id → workspace-id，W3 的解析数据源）+ 共享 `Arc<SessionIndex>` / config / user MCP pool / plugin manager（D2）。
- `bootstrap::assemble`：有 home 时读 `workspaces.json` **逐行登记（注册先行）**——默认工作区（serve = 最近打开行；嵌入宿主 = 钉扎根，§2.2）即刻构建，其余惰性；坏行与**根目录已消失的行**跳过 + warn、保留行（下次可见）。无 home / 注册表空 → 保留现行为（cwd 兜底单工作区）。
- 现有调用面不破坏：registry 暴露 `default_manager()`（= 当前绑定语义，W4 前所有 session 操作走它）。
- 身份按 D3（workspaces.json id 即 bucket key，`session_index.rs:78-83` 注释已预留该语义）。

**验收**：单测——多工作区恢复（注册表 N 行 → N 行登记、默认工作区即建、其余惰性不构建）；坏行/根消失容错；无 home 兜底路径不变（现有测试零回归）；**惰性构建延迟断言**（登记 N 行与 1 行的启动路径，除默认工作区外零 scope 构建成本）。

---

## W2: workspace.* 真语义 + serve 拦截解除 + 注册表写策略

**优先级**：P0 ｜ **涉及面**：rust（mirri-embedded / mirri-server / mirri-proto / mirri-session-index）｜ **设计依据**：08 §2.3、§3（workspace.* 行）、§4 D4/D8

**范围**：
- `workspace.createOrTouch`：校验 root 存在且为目录 → **root 词法规范化**（D3/§4：与 id 派生链同构，不 canonicalize）→ 写注册表 → registry 登记并建 scope+manager → 返回 WorkspaceView；幂等（同 root = touch，不重建）。
- **注册表写策略落地（D8）**：锁文件 + 读-合并-写（本地 ∪ 磁盘、tombstone 胜出）+ 原子替换；锁 mtime 超时抢占；写经 IO trait（zero-IO 合规）。
- `workspace.list`：registry 全量，`session_count` 取 `SessionIndex::count_active` 多 workspace 过滤（`lib.rs:317-336`，**折叠别名集**——D3 裁决本期搬入）。
- **过渡信任策略（N4 闭环）**：信任门就位（W7）前，本批及后续注册的工作区 scope 一律按 `WorkspacePolicy::UntrustedDefault` 构建（`bootstrap.rs:156`）——project 层不加载、AGENTS.md 项目段不注入，不存在策略未定窗口。
- `workspace.get` / `handlerFor`：真实现（unknown id → null，klient maybe 语义）。
- **删除** `mirri-server/src/rpc.rs:784-889` 四个传输注册表 handler，改 engine fall-through；registry/TSV/pin 联动（`workspace.get/handlerFor` 在 TSV 现为 Unimplemented）。
- **workspace 生命周期事件发射（用户裁决并入本批，2026-09-11）**：注册/改名/删除落位时发射 `event.workspace.created/updated/deleted`——事件投影扩展（`mirri-core/src/session/event_projection.rs` 分支 + 契约链五环联动）；消费端同步按 rust/AGENTS.md 契约 3 清单核对（web 投影器：v1 拼写的 REST 消费端已存在、rust 传输层映射待核；SDK 翻译层 `translate.ts`；ACP 翻译层）。
- REST 面（`http.rs` GET /api/v1/workspaces）核对 `{items}` 包装（web rust 走 WsRpcApi 不受影响，此项只影响 e2e 兼容）。

**验收**：contract-e2e + 单测——createOrTouch 新 root 后 list 出现新行且注册表/db 落盘；同 root 幂等 touch；**并发合并单测**（写前磁盘被外部插入新行 → 合并后不丢）；serve 面经 `/api/v1/rpc` 真实多行返回；mutation（恢复传输注册表拦截 → serve 面单行红线红；去合并逻辑 → 并发丢行红线红）；**多客户端同步 e2e**：一条连接注册/改名/删除 → 另一连接收到事件并镜像（`applyWorkspaceEvent` 路径）。

---

## W3: 会话路由层（session-id → manager 解析缝，全派发面）

**优先级**：P0 ｜ **涉及面**：rust（mirri-embedded / mirri-server）｜ **设计依据**：08 §3、§4 D7

**范围**：
- registry 上实现 `manager_for(session_id)` 解析缝：活跃会话表 → `SessionIndex` workspace_id 回退 → not-found（不静默回退默认）。
- **embedded 侧全面接入**：`embedded_server` 内所有按 session-id 经 `self.manager` 寻址的 handler（session.get/close/resume/fork/archive/export、message.send/steer、审批、提问、compaction、cron、事件中继、会话级 MCP bridge——现状清单见 context.md §6.1/§6.4）改经解析缝。机械改造，逐文件清点。
- **serve 传输侧接入**：传输注册表持有的会话寻址 handler（审批/提问/agent/tool/文件/git/btw，`rpc.rs:3246-3277` 一带）、`ws_rpc.rs` 的 `session.subscribe` 与 restore-on-demand 钩子（`rpc.rs:479-492` 的 `live_or_restored_session` 链）、**fire 模式状态派生 `prepare_fire_watch`**（`state.rs:417-440`，会话寻址遗漏点——fire 模式是 web 常规发送路径）。
- **reconcile 扩面前移至本批**（路由层的数据源）：`reconcile_session_index_once`（`session_index.rs:170-251`）从单 bucket 收敛改为按 registry 逐工作区收敛；孤儿行归属注释语义更新——否则非默认工作区的持久化会话不进索引，W4 后 resume/翻页对它们 not-found。
- 会话创建/恢复路径登记活跃会话表，关闭/注销摘除。

**验收**：e2e——在**非默认**工作区创建会话（直接经 registry 注入测试路径，不依赖 W4 的 web 参数）：message.send 送达、subscribe 收到事件、审批/提问回路通、resume 通；mutation（解析缝退化为恒返回 default_manager → 非默认工作区全部红线红）。

---

## W4: session.create 工作区路由 + web 解封

**优先级**：P0 ｜ **涉及面**：rust（mirri-embedded）+ TS（mirri-web）｜ **设计依据**：08 §3、§4（session.create 行）

**范围**：
- `dispatch_create_session`（`session_handlers.rs:55-121`）接受 `workspaceId`，经路由层路由；未知 id → invalid_params；缺省 → default_manager（兼容）。
- `work_dir` / `SessionScope.workspace_root` / `WorkspacePaths` 构造链（`session_scope.rs:150-184`）从目标工作区取值，去掉绑定根回填（`manager/mod.rs:353-355`）。
- web `WsRpcApi.createSession` 解封（`ws-rpc-api.ts:510-528` 现主动丢弃 `cwd/workspaceId`）：透传拼法与 REST `client.ts:346-365` 的 `workspace_id` + `metadata.cwd` 双发对齐。
- `migrate_legacy_flat_sessions`（`manager/mod.rs:777-782`）flat 布局会话归属目标工作区 bucket。

**验收**：rust+web 手动场景（用户可感知主目标）：空 composer 选工作区 A 发消息 → 会话落 A 的 bucket、消息正常往返（W3 路由兜底）、`session.list` 行 cwd = A 根；e2e 钉住。

---

## W5: session.list + session.search 多工作区化

**优先级**：P1 ｜ **涉及面**：rust（mirri-embedded / mirri-session-index）｜ **设计依据**：08 §4（session.list/search 行）

**范围**：
- listing 数据源从单 bucket 扫描（`session_index.rs:349-445`）换成 `SessionIndex::list_sessions` 多工作区查询。
- **索引游标**：`ListOpts`（`lib.rs:140-150`，现状仅 limit/offset）扩展 `before_id`/`after_id` id 游标。
- **`exclude_empty` 数据源（需求级裁决已定，用户 2026-09-11）**：① `lastPrompt` 语义 = **最近一条用户输入，无论成败**——"从未对话"才是空，"对话失败"不算空；② **惰性物理创建**〔设计增强，超出 TS 现网〕：从未对话的会话不落物理会话目录与索引行，**首条用户消息发出时才物化**（TS 现网 create 即 materialize + 索引写通：`sessionLifecycleService.ts:203-234`），create 写通点相应后移。索引 schema 的"无 prompt 会话"字段（含存量 db 迁移设计，迁移 e2e 归 W9）仍保留——exclude_empty 为承重过滤不可分期（web listSessions 恒发 `excludeEmpty: true`），服务存量与 TS 兼容桶的空会话；与 TS 现网判定（`kap-server/src/routes/sessions.ts:422`）对齐。决策记录：会话持久化域 SDR-004；persistence 域开放项 #5 随此闭环。
- **嵌入宿主可见性过滤**：list/search 在嵌入宿主下传 `workspace_ids = [钉扎根]`（设计 §2.2"不参与默认可见集"的落点）。
- 过滤面（`busy`/`archived_only` 等）以 TS 现网为准精确钉 TSV，避免第二轮 honest-gap（含 persistence 开放项 #1 的 `archivedOnly`，随本批落地）。
- cwd 不再回填绑定根（`session_handlers.rs:325-358`）；`session.search`（`session_handlers.rs:1769-1777`，现状回填绑定根）跨工作区扫描，hit 的 workspace_id 为真实所属。
- **别名集折叠（D3 裁决本期搬入）**：移植 TS `IWorkspaceAliases` 语义——同一根目录的异拼写桶（大小写 / `~` 展开 / 符号链接历史形态）视为一个工作区；折叠点对齐 TS 现网：会话列表 workspace 过滤（`kap-server/src/routes/sessions.ts:170-172`）、会话计数（`routes/workspaces.ts` 的 count 折叠）、索引刷新按别名集收敛（`workspaces.ts:326-337`）；测试矩阵覆盖畸形历史形态。
- web `WsRpcApi.listSessions` 恢复发送过滤参数（`ws-rpc-api.ts:460-474` 现为 `void input`）。

**验收**：跨工作区 fixture（两 bucket 各 N 会话）→ list 全量排序/过滤/游标分页正确、search 跨桶命中且归属正确；web 侧边栏分组按真 cwd 派生（`mergeWorkspaces` 链路）。

---

## W6: workspace.update/delete + last_opened_at touch

**优先级**：P1 ｜ **涉及面**：rust + TS（web 排序依赖）｜ **设计依据**：08 §2.1、§2.3、§4 D4/D8

**范围**：
- `workspace.update`（rename）写穿注册表与索引；`workspace.delete` 按 D4（用户裁决三选一）：**purge 物理删除**（经 W3 路由层逐会话 close → 删磁盘桶数据与索引行，不可恢复）/ **remove 仅移除**（close → 数据保留，重新注册即恢复）；两种模式均摘除 scope/manager + tombstone；携带模式参数进契约（TSV 联动）。两方法脱离 `applied:false`（`workspace_handlers.rs:110-154`）。
- **web 删除入口改三选一确认框**（物理删除 / 仅移除 / 取消——物理删除为主推项，鼓励清理存量会话占盘）。**确认框形态已定（实现侧自决，用户授权 2026-09-11）**：主按钮"删除数据并移除"（danger 强调、Enter 默认）、次按钮"仅从列表移除"（附可逆性提示——重新添加同一路径即可找回会话）、Esc/点遮罩 = 取消；框内展示将影响的会话数（给清理决策量化依据）；逐字文案随本批 brief 定稿。
- **tombstone 新增面**：`registry.rs` 补"写 tombstone"API + 读侧行集过滤（写侧保留逻辑已有 `registry.rs:108-149`）。
- `last_opened_at` touch：该工作区 session.create 后内存 touch + **去抖落盘**（W2 的写策略复用）；web 侧边栏 `recent` 排序依赖。
- `workspace.delete` wire 漂移钉进 contract-e2e fixture（web `deleteWorkspace` 返回 void，两侧引擎返回结果对象）。

**验收**：web rust 后端下重命名生效 + 删除走三选一确认框；purge e2e（桶数据与索引行真删、不可恢复）+ remove e2e（数据保留、重新注册即恢复）；tombstone 单测（删除后读侧过滤、写侧保留）；去抖断言（N 次 create 触发一次落盘）。

---

## W7: MCP 多根化 + 信任加载门

**优先级**：P1 ｜ **涉及面**：rust（mirri-embedded / mirri-mcp / mirri-cli）｜ **设计依据**：08 §2 隔离/复用、§4 D6（时序硬约束）

**范围**：
- **信任加载门（本批硬性范围，不可拆至 W8）**：未信任工作区的 project 层 MCP 配置不加载、AGENTS.md 项目段不注入；新注册工作区默认 untrusted（`WorkspacePolicy::UntrustedDefault` 既有语义生产化）。信任状态存储（home 级，不随注销删除）落本批——W8 只做管理 RPC/路由。
- `McpReconcileKernel.workspace_root` 单值（`mcp_config_handlers.rs:901-923`）改 per-workspace：registry 逐工作区的 project layer 配置加载/写侧路径/工具过滤 fan-out（`mcp_config_layer_paths` 739-747、`writable_scope_path` 752-768）；bootstrap `load_mcp_manager_for_root`（`bootstrap.rs:489-530`）同。
- user pool 保持进程级单份（`shared_user_mcp_pool_for_home`，`engine_bridge.rs:97-115`，不随工作区复制）。
- watcher（`mirri-cli`）从单 `project_root`（`main.rs:343-346`）改逐注册工作区 watch。

**验收**：两工作区各持不同 project mcp.json，同名 server 遮蔽按工作区独立生效；user pool 每 server 只连一次（连接数断言）；**未信任工作区的 project mcp.json 零加载断言**（红线）；trust 后生效。

---

## W8: trust 管理 RPC/路由

**优先级**：P2 ｜ **涉及面**：rust + TS（web trust 路由）｜ **设计依据**：08 §4 trust 行、§4 D6

**范围**：
- trust/untrust RPC + REST 路由（TS v2 对齐：kap-server `workspaces.ts:236-308`），读写 W7 落地的信任存储。
- **trust 网页入口（形态已裁决，用户 2026-09-11）**：web 现无任何 trust 交互与 API 消费（TS 契约有路由、web 未接）——多工作区下新注册工作区默认未信任。入口形态 = **用户在界面选择一个未信任的新工作区时触发信任确认**（两动作：信任 / 暂不；暂不则正常使用但项目层配置不生效且界面诚实呈现），取代原"注册后一次性确认"的临时形态；需求侧见 workspace-management US4.2 + SDR-007，随本批 RPC 落地，避免加载门就位而用户无解锁路径。
- 取代 `scope/config.rs` 静态 `WorkspaceTrustLevel` 的 serve 路径消费。

**验收**：web rust 后端 trust/untrust 生效且联动 W7 加载门；AGENTS.md 注入按信任级控制。

**落地记录（2026-09-12）**：`workspace.setTrust` RPC（both-faces rust-first 行，TS v2 REST 信任路由为跨端参考——klient 无 trust RPC）写 home 级信任存储后**原位翻转**活门（运行时 `trusted` 原子 + scope/每会话 `load_project_agents_md` 门 + 活动代理系统提示刷新），**不重建运行时**（重建会使活会话在路由缝上搁浅）；信任 ⇒ kernel 下一次 pass 连接 project 层；撤销 ⇒ kernel 的 converge-to-empty 分支即刻断开 project 层连接（US4.1 对称）。`workspace.list`/get/createOrTouch/update 视图行携带**实时** `trusted`（store 读，默认工作区恒 true；撤销 pin 默认工作区 = invalid_params）。web：`WsRpcApi.setWorkspaceTrust` + `useWorkspaceState.trustWorkspace`/`maybePromptWorkspaceTrust`（SDR-007：openWorkspace/openWorkspaceDraft 选中未信任工作区 → 非阻断确认 信任/暂不）+ `TrustWorkspaceDialog(.vue/Host)` + 未信任标签（侧栏组头 + composer picker）+ i18n en/zh + vitest。契约五环（registry/TSV/EMB 路由/fixture/快照）与 cross-backend W8 pin 同批落地。

---

## W9: 收口批

**优先级**：P2 ｜ **涉及面**：全仓 ｜ **设计依据**：08 §7 验收标准

- `mirri serve` CLI：`workspace_root` 参数变可选（有注册表时启动全量；`main.rs:141-201, 442-449` 泛化）；**显式根与非空注册表共存的优先级**（显式根作为默认工作区优先，其余行惰性登记）+ 移除 `main.rs:162` 的 canonicalize（与设计 §4 词法规范化立场对齐，否则 symlink 启动会注册同目录第二行）。
- **存量 `session-index.db` 迁移 e2e**（W5 schema 扩展的既有安装升级路径）。
- **跨引擎并发写 e2e**：同 home 下模拟 TS daemon 与 rust 交错注册/注销——不丢行、tombstone 不复活（D8 合并语义，超越 W2 单测的进程级验证）。
- **别名集 e2e**：构造同根异拼写的分叉桶历史数据（大小写 / `~` 展开），验证列表过滤、计数、刷新的折叠语义与 TS 一致（D3 裁决的验收面）。
- **ACP/NAPI 共享 home 测试**：非空注册表下钉扎语义（协商根默认、其余惰性）。
- parity 账目：TSV workspace.* 行 disposition 更新（B4 的 workspace 部分由本计划关闭）；pin 联动；`searchSessions`/`listSessions` 过滤面（web 实际发送集）钉入 contract-e2e。
- changeset：行为变化属 feature，预计 `minor`；若有 wire 破坏性变更 → 停下走 major 确认硬门。

---

## 风险（执行风险）

| 风险 | 缓解 |
|---|---|
| 多 manager 内存/MCP 连接放大 | user pool 进程级共享；project pool 懒连接（MCP spec A3 既有裁决）；W7 验收含连接数断言 |
| `workspaces.json` 跨引擎并发写丢更新 | rust 侧 D8 锁 + 读-合并-写（W2 落地，W6 补 tombstone 面）；W9 并发 e2e 钉住；touch 去抖控制写放大；**TS 侧写方无锁整目录替换的残余窗口已声明**（设计 §7.3/D8 范围声明，TS 补丁为 conditional followup 随退役线重估） |
| serve 拦截解除后 engine 未实现的方法面空洞 | W2 单批内"实现 + 解除"原子落地 |
| 路由层改造面大（embedded + 传输层全派发点） | W3 独立批次 + 逐文件清点清单进 brief + mutation 钉解析缝；机械改造不改语义 |
| 删除工作区时活跃会话丢失 / 误删风险 | D4 三选一确认框（物理删除主推但需显式选择；仅移除可逆）；purge 路径不可恢复性落 e2e 钉住；处置语义经 W3 路由层逐会话 close |
| 启动恢复 N 工作区的启动延迟 | D5 注册先行 + 惰性构建；W1 验收含延迟断言 |
| 存量 `session-index.db` schema 迁移 | W5 设计迁移路径，W9 迁移 e2e |
| 别名集移植的畸形历史形态测试矩阵（大小写不敏感 FS / symlink / `~` 展开） | W5 折叠实现 + W9 e2e 钉住；以 TS 现网行为为对照基准 |
| 嵌入宿主在共享 home 被非空注册表干扰 | §2.2 钉扎语义；W9 ACP/NAPI 共享 home 测试 |
| `SessionManager` 40+ `workspace()` 调用点被误改 | D2 使 manager 内部零改动；路由只动派发层；评审 diff 纯度审计盯住 |

## 验收标准（计划级）

对应设计文档 08 §7 五条验收标准的执行侧细化：

1. rust+web 手动场景全通：添加工作区（新 root 真实注册）→ 侧边栏出现新分组 → 选工作区发消息落对应 bucket 且消息/事件正常往返（W3+W4）→ list/search 按工作区过滤分页 → 重命名/删除生效。
2. 会话路由完整性：非默认工作区会话的 send/subscribe/审批/提问/resume 全通（W3 验收）。
3. 跨引擎并发共存：交错读写不丢行、tombstone 不复活（W2 单测 + W9 e2e）。
4. 契约链五环全绿 + `cargo test --workspace` / clippy / zero-IO / web typecheck+vitest 全绿。
5. TSV workspace.* 行全部脱离 Unimplemented/空应答 disposition，pin 联动收口；未信任工作区 project 层零加载红线绿。
