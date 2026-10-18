# 会话管理 — 现状盘点（Context）

> 快照日期：2026-10-09（§1.3/§1.4/§1.7 fork 现状核实刷新——同日 F1-F6 落地后 §1.3/§2 再刷新；undo 差异节 §1.6 保留 2026-09-10 基线；§1.8 运行状态三引擎盘点 2026-10-13 增补）·
> 本文只盘点**当前状态与痛点**，不含目标、不含决策（那些见 [session-management-requirements.md](session-management-requirements.md) / [session-management-architecture.md](session-management-architecture.md)）。
> 排查基线：Rust 分支 `feat/rust-agent-profile`（会话持久化域 Phase 1–3 已合入，见 [session-persistence-context.md](session-persistence-context.md)）。

## 1. 现状事实

### 1.1 网页（apps/mirri-web，首个消费方）

- 侧边栏已是统一结构：会话分组 + 可折叠工作区头部（`apps/mirri-web/src/components/Sidebar.vue`；旧的工作区栏/标签页已移除）；会话行组件 `SessionRow.vue` 含状态标记、重命名、归档动作。
- 无任何置顶概念（全代码库无 pin 相关 UI）；无任何列表拖拽交互（现有拖放仅文件上传，`composables/useAttachmentUpload.ts`）；`package.json` 无拖拽库依赖。
- 会话行数据已含 `archived` 字段映射（`api/daemon/mappers.ts`），列表默认排除归档已通（持久化域 Phase 1）。
- 列表数据源：`session.list`（按 created_at 倒序返回）；工作区列表：`workspace.list`。

### 1.2 wire 契约面（mirri-proto）

- 已登记：`session.list / rename / archive / restore / fork / updateMeta …` 与 `workspace.createOrTouch / delete / list / update`（`rust/crates/mirri-proto/src/rpc/authoritative-methods.tsv`）。
- 未登记：`session.pin`、`session.unpin`、`session.reorder`、`workspace.reorder`——全部为新契约。
- `session.list` 行现无 `pinned` 字段；返回顺序为 created_at 倒序，无分层逻辑。

### 1.3 引擎（Rust）

- 存储底座就绪：会话桶布局、`state.json` 并集契约、S4 索引（`workspaces.sort_order` 列已预留）、AtomicDocument 原语——见 [session-persistence-context.md](session-persistence-context.md) §1。
- 会话元数据（`SessionMetadata`）现无 `pinned` 字段；无顺序文档；索引无 pinned 列。
- fork 现状（2026-10-09 F1-F6 批次落地后复核，分支 `feat/session-fork-pipeline`，`session/manager/mod.rs` `fork_session_with_options` + `session/fork_pipeline.rs`）：**按记录截取重写的会话级分叉管道**——源会话主 wire log 经 in-memory 迁移链读取，按 turnId 锚点（缺省 = 最近已收尾回合）截断到目标回合的 `turn.ended`，过滤表剔除运行中资产（cron 域三变体 + 历史 `forked` 标记 + `goal.update` 生命周期字段），重写入新会话 `wire.jsonl`；成员 journal（`agents/<id>/wire.jsonl`）按各成员自己的括号结构同算法截取复制，`blobs/` 全量复制，TS `agents` manifest 原样携带；`tasks/` 与 `snapshot.json` 不复制。血缘 `forkedFrom` 落 `state.json`（`Fork of <源标题>` 命名，源标题不可得退化源 id），custom 元数据按 v2 forkCustomMetadata 语义合并（goal 键双侧剔除）。锚点护栏为 ADR-F4 修订版（锚点校验而非会话级忙碌）：显式 `turnId` 命中已收尾回合即放行（源会话可有活跃回合），命中无 `turn.ended` 记录 → `ForkTargetNotFound`（invalid_params，消息带可用回合范围与活跃尾解释）。落位原子：任何失败 → 目标会话从 workspace 撤销 + artifacts 排空 + 目录删除（零残留，源会话零触碰）。既有对齐项保留：不触发会话启动钩子、用户级权限规则继承（D-34）、cron 不带（DEC-4——V3 含 fork→resume→cron.list 空 e2e 钉住）、`last_prompt` 继承、`SessionForked` 事件在源会话总线发布、`session.createChild` 复用同一管道（`Child: <源标题>` 命名不变，缺省锚点同 fork）。差距项（消费端，见 rust-core-design ROADMAP 1.9 遗留）：TS 侧契约（klient/v2/node-sdk 的 fork 参数 schema）尚无 `turnId`；web/TUI 分叉点选择 UI 未实现。
- 缓存归属标识：Rust 内核的模型请求配线**无任何归属/缓存键支持**（`llm.rs`/`profile.rs` 无 cache_key 相关字段——v2 有 `cacheKey` 通路、Rust 完全缺失）。

### 1.4 TS 参照（agent-core-v2）

- 无置顶概念、无排序契约（全库无相关实现）；`session-index.db` schema 无 pinned/order 列。
- `workspaces.json` 由 TS 读改写（`fileWorkspacePersistence.ts`）——未知字段的写回透传行为**未验证**（架构 §5 风险项）。
- fork 的定时任务处理（2026-09-09 实测）：`duplicateCronTasks`（`sessionLifecycleService.ts:637`）把源会话名下全部定时任务**克隆成新任务**（新 ULID、归属改打新会话）存入工作区级 cron store——源会话那份不动，形成**双跑**（两边开着时同一条提醒各触发一次）。克隆体继承 `lastFiredAt` 游标（节奏与源一致）；定时任务无运行态字段，仅在所属会话打开时触发（session 级轮询，`sessionCronServiceImpl.ts:310`）。cron 的 wire 操作 `persist: false`（live-only，`cronOps.ts:46`）——fork 复制 wire journal 不产生幽灵任务、克隆体恢复时从 store 加载，无跨会话互踩。
- fork 复制边界（2026-10-09 复核，`sessionLifecycleService.ts:430` `fork()`）：**全量成员**复制（遍历 `state.json` 的 agents 逐个 `copyAgentWire`，与 SDR-011 一致）；逐 agent 复制 wire journal 并追加 `forked` 边界记录（`copyAgentWire`，`sessionLifecycleService.ts:559`）；文件复制跳过 `state.json`/`logs`/wire，其余原样拷贝（含后台任务输出等静态文件，为死数据）；源会话若有未收尾的进行中回合，其半截记录也会被复制（无回合原子截断）；**无分叉点参数**（`ForkSessionOptions` 仅 `sourceSessionId/newSessionId/title/metadata`，`sessionLifecycle.ts:35`）；**无忙碌护栏**（`fork()` 不检查回合活跃——对比 v1 `core-impl.ts:576` 有）；调用方**可指定新会话 id**（`opts.newSessionId`）；元数据：`forkedFrom` 写入、`lastPrompt` 继承、自定义元数据合并（`forkCustomMetadata` 剔除 goal 键）；`SessionForked` 事件发布于 fork 返回前（`_onDidForkSession` + `announceCreated`）。失败回滚完备（catch 分支清理内存态 + 目标目录）。
- 附件本体存于**会话数据侧**（`<会话目录>/<agentId>/blobs/<内容哈希>`，`agentBlobServiceImpl.ts:6`；wire 里以 `blobref:` 引用）——当前 fork 拷贝会话目录时**连附件本体一起复制**，与 SDR-007 的「共享存储、不另存副本」相悖，且 blob 清理无跨会话引用保护（源会话删除可能带走分叉会话还在引用的附件）。
- v1 侧（`packages/agent-core`，过渡引擎）：fork 有忙碌护栏（`SESSION_FORK_ACTIVE_TURN`，`rpc/core-impl.ts:576`）、走目录整拷 + 清理 + resume（`session/store/session-store.ts:78`）；目标 id 可指定；删除 `upcoming-goals.json`、追加 `forked` 标记。

### 1.5 终端（TUI/CLI）

- 终端有会话历史/恢复入口（消费 `session.list` 契约）；无置顶/排序消费面。呈现顺序由引擎统一给出后终端零成本对齐（架构 ADR-005）。

### 1.5a fork 的界面入口现状（2026-10-09 增补，US5.1 界面条款的现状依据）

- TUI：`/fork` 命令（`apps/mirri-code/src/tui/commands/session.ts:57-77`）——fork 后**切换到新会话**并提示返回路径（"To return to the original session: mirri -r <源id>"）；标题取 `forkSourceTitle`。无分叉点选择能力（等价于末尾分叉）。
- 网页：会话行注释指向的完整动作菜单含 fork（`SessionRow.vue:4`——动作在父组件菜单），对话头部菜单有"复制会话"项（`ChatHeader.vue:319`，emit `forkSession`）；`forkSession()` composable（`useWorkspaceState.ts:2764`）fork 后 `upsertSessionFront`（新会话插列表顶部）+ `selectSession`（立即选中）——**落点 = 跳转新会话**。侧栏快捷命令 `/fork` 亦注册（`slashCommands.ts:37`）。
- 可复用的选择器先例：TUI 的撤销选择器（`undo-selector.ts`——可搜索列表，每项含用户输入预览 `UndoChoice.input`），分叉点选择器的交互模式与其同构。
- 血缘呈现：两端的会话列表/头部目前均**无** fork 来源的可见标识（`forkedFrom` 数据在 v2 有写入、界面未消费）——US5.1"溯源可见"条款的待补项。

### 1.6 对话撤销（undo）三引擎现状（2026-09-10 增补）

三引擎均有撤销实现，行为与保真性分叉（U1 立项的现状依据）：

**TS v1（TUI `/undo` + 引擎落盘）**
- TUI 入口：`/undo [count]` 与可视化撤销选择器（以"真实用户输入"为撤销锚点——用户消息、用户触发的 skill/plugin 命令；`apps/mirri-code/src/tui/commands/undo.ts` `handleUndoCommand`）；回复流式进行中拒绝（`undo.ts:48-51`）；可撤销范围同时按转录与引擎上下文计算，压缩分隔条之后的对话才是可撤销范围（`undo.ts:262-289`、`undoAvailabilityFromContext` `undo.ts:214-232`）；超额请求拒绝并反馈"最多可撤 M 条 + 是否到压缩边界"（`undo.ts:341-349`）。
- 引擎实现：`packages/agent-core/src/agent/context/index.ts:182-219`——`undo(count)` 自尾向前计数真实用户输入锚点、连同其后全部内容移除；遇压缩摘要边界停止（`index.ts:195-198`）；**先写 wire 记录再改内存**（`index.ts:186`，`context.undo` 记录，**count = 锚点数**）；重放侧折叠（`agent/records/index.ts:113-114`）、转录投影按 undo 截断（`services/message/transcript.ts:305`）。

**TS v2（`contextUndo` wire op + 压缩边界即停 + 检查点深度限制 + 护栏）**
- 服务编排：`packages/agent-core-v2/src/agent/undo/undoService.ts`——忙碌护栏（回合活跃经 `tryAcquireQuiescence`、压缩进行中拒绝，`undoService.ts:100-122`）；预检（可撤销数、压缩边界，`contextOps.ts` `precheckUndo`）；**检查点深度上限**（`CHECKPOINTED_MODELS`——可撤销轮数受已保存会话状态检查点约束，超限报 `checkpoint_lost` / `compaction_boundary`，`undoService.ts:124-180`）；撤销后参与者对账与 lastPrompt 元数据回写（`undoService.ts:182-237`）。
- wire 记录：`contextOps.ts:377` 定义 `context.undo`（`contextUndo`）；`contextMemoryService.ts:83-95` 撤销时**先派发 undo op + 截点尺寸 ops 再改内存**（`sizeOpsForCut` 携带截点消息数）；转录折叠遇压缩摘要停止（`contextTranscript.ts:173`）。

**Rust（`session.undo`：内存锚点回退，不落盘——保真缺口）**
- RPC `session.undo`（`sessionId` / `agentId?` / `count?` 缺省 1，返回 `{ok, undone}`；契约已登记，authoritative-methods.tsv 67 行）→ `dispatch_undo_history`（`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1605-1638`）→ `ContextMemory::undo_count`（`rust/crates/mirri-core/src/context_memory/mod.rs:863-919`）：`compute_undo_cut` 自尾向前计数真实用户输入锚点（`mod.rs:987-1017`），遇压缩摘要 origin 停止（`mod.rs:1002-1005`），移除锚点及其后全部内容并清理 open steps / pending tool results / deferred。
- **不写任何 wire 记录**：全仓生产路径无 `WireOp::ContextUndo` 写入方（wire 枚举已定义 `ContextUndo { count }`，`rust/crates/mirri-proto/src/wire.rs:189`；唯一消费方是 reducer fold 与测试）——**撤销后收起重开，重放回到撤销前内容**（US-U2 要消除的缺口本体；ADR-006 定接口义务）。
- **无忙碌护栏**：dispatch 不做回合/压缩忙碌检查（对比 v2 `undoService` 的 quiescence/compaction 守卫）。
- reducer fold 的 `ContextUndo` 臂（`mirri-proto/src/reducer.rs:225-242`）：按**消息条数**截断（`count` = 从尾部移除的消息数，时间线同截），仅由 TS 写入日志与测试夹具行使（`mirri-core/tests/compaction_tests.rs:4084`、`mirri-proto` proptest/reducer_tests）。**口径分裂事实**：同一条 `context.undo` 记录，v1 写的是锚点数、rust fold 读的是消息条数——跨引擎重放同一日志的截断位置不一致（该口径的收敛方向由架构 ADR-006 裁定，见 [session-management-architecture.md](session-management-architecture.md)）。
- 编号交互（压缩域 ADR-007）：撤销产生的编号消耗间隙经 apply 记录区间括号校正——**仅对 wire 记录形态的撤销有效**；rust live 撤销不落盘，故其编号消耗在重放侧不可见；切点落入头部精华区/保留尾区之内时该区段欠校正（压缩域文档化残差，compaction-context §1.2 与 reducer 文档）。

### 1.7 分叉的缓存计费现状（2026-10-09 增补，SDR-010 修订裁决/ADR-007 的现状依据——现状即目标行为）

- TS v2：模型请求配线的 `cacheKey` = **当前会话 id**（`agent/profile/profileService.ts:549` `cacheKey: this.sessionContext.sessionId`；经 kosong 分发到各服务方——OpenAI 系 `prompt_cache_key`（`openai-responses.ts:1096`、`openai-legacy.ts:668`）、Kimi `prompt_cache_key`（`kimi.contrib.ts:187`）、Anthropic 系 `metadata.user_id`（`anthropic.ts:895`））。fork 换新会话 id → `cacheKey` 跟着换 → 分叉会话首请求对按 key 路由缓存的服务方是全新对话。**v1 同构**（`packages/agent-core/src/rpc/core-impl.ts:1171` `promptCacheKey: sessionId`）。
- Rust：模型请求配线无 cache key 通路（`mirri-core/src/{llm,profile}.rs` 无相关字段）——缓存归属的引擎侧支持完全缺失。
- 两代 TS 引擎的 `cacheKey` 都没有"分叉继承"概念：fork 后新 id 即新缓存身份。服务方侧的实际影响因提供方而异（按前缀匹配的缓存不换 key 也命中；按 key 路由的换 key 即 miss）。

### 1.8 会话运行状态三引擎现状（2026-10-13 增补，US4.4/SDR-013 的现状依据——刷新丢"运行中"标记的缺口本体）

**TS v2（参照实现，两侧均有——目标形态）**
- 列表读面：列表行携带 work facts 四元组（`busy`/`main_turn_active`/`pending_interaction`/`last_turn_reason`，`kap-server/src/routes/sessions.ts:1093`）——刷新后侧边栏直接从列表行恢复"运行中"标记。
- 事件面：核心层的会话活动聚合视图（每会话一个 fold，输入去重——状态未变的重复输入不产生事件）→ 状态广播器把每次真实迁移发为 `event.session.work_changed`（`sessionEventBroadcaster.ts:1137`），**durable**（不在 `VOLATILE_EVENT_TYPES` 白名单——落会话事件日志、带 seq、可 cursor 重放）且**全局扇出**（`isGlobalEvent` 匹配 `event.session.*` 前缀，任何连接不订阅该会话也能更新列表行）。回合结束的迁移经微任务缓冲，保证排在对应 `turn.ended` 帧之后。

**Rust（新内核——两侧均缺，即用户报告的 bug）**
- 列表读面：`SessionView`（`mirri-embedded/src/embedded_server/mod.rs:574`）无 `mainTurnActive` 等四元组字段，只有 `busy`；且 `busy` 的来源（在途回合注册表，纯内存）只覆盖**已构建 runtime 的 live 会话**——索引行（SQLite 会话索引的静态行）`busy` 恒 `false`（`session_index.rs` 的行构造）；非默认工作区的 runtime 懒构建（首次触碰该工作区的会话才 build），列表刷新不触发构建 → 非默认工作区里正在跑的会话刷新后只剩索引行，`busy=false`。
- 快照读面：`session.snapshot` 的会话视图（`mirri-server/src/rpc.rs:1950`）同样无四元组；有 `busy`（在途注册表 + 引擎侧联合）与 `in_flight_turn`（仅主 agent、仅 runtime 已构建时非空）；存在会话日志未收尾回合探测能力（`read_only_open_turn_started_at`，`mirri-core/src/session/manager/restore.rs:550`）但只用于快照的在途回合锚点，未接入列表。
- 事件面：`event.session.work_changed` **无任何生产者**（全仓 rust 无发射点）；会话活动聚合视图已移植（`mirri-core/src/session_activity.rs`，四元组 fold + 去重 + 迁移原因）但**零消费方接线**——聚合器存在说明设计意图本要对齐，发射端从未落地。
- 结果：网页刷新后，运行中标记的三个客户端内存来源（本地在途表/回合活跃表/会话行字段）全部清零，服务端两侧都补不回来 → 进行中的会话从侧边栏"消失"运行态（会话本身还在列表，只是看不出在跑）。

**TS v1（过渡引擎）**
- 有五值状态枚举（`idle`/`running`/`awaiting_approval`/`awaiting_question`/`aborted`）经 `event.session.status_changed` 广播——v2 已用正交四元组取代（枚举值迁移会丢并发迁移，PR #102）；v1 随退役线消化，无对齐义务。

**消费端（网页）**
- 侧边栏"运行中"标记（spinner）判定 = 主对话回合活跃（本地在途 OR 本地回合活跃 OR 会话行 `mainTurnActive` 三源并集，`useMirriWebClient.ts:1810`）；`event.session.work_changed` → 会话行 busy/mainTurnActive 更新（`eventReducer.ts:461`）。刷新后的兜底链：列表行 `mainTurnActive` → 快照 `snap.session.mainTurnActive ?? (inFlightTurn ≠ null && busy)`——rust 两侧字段均缺，兜底链全部落空。
| 能力 | 终态 | 现状 |
|---|---|---|
| 置顶标记 | state.json + 索引 + wire | 全链路缺失 |
| 会话固化顺序 | 桶内 order.json + 索引缓存 + `session.reorder` | 全链路缺失 |
| 工作区固化顺序 | workspace-order.json + `workspace.reorder` | 全链路缺失 |
| 列表呈现顺序 | 引擎统一分层序 | created_at 倒序，无分层 |
| 网页交互 | 置顶动作 + 拖拽（原生） | 无 |
| TS 对齐 | 契约共享、实现跟进 | 无实现（按 SDR-003 进 TS 排期） |
| fork 完整性（E5） | 独立会话 + 「Fork of 原标题」命名（SDR-012）+ 带已完成对话/重置运行中（SDR-007）+ 失败回滚 + 全量成员复制（SDR-011） | **Rust 管道已落地**（2026-10-09 F1-F6：turnId 锚点截取 + 过滤表（cron/forked/goal 生命周期重置）+ 全量成员 journal/blobs/agents manifest 复制 + `Fork of 源标题` 命名 + forkedFrom 血缘 + 原子落位零残留 + 锚点校验护栏（ADR-F4 修订版）——V1-V8 验收全钉，见 rust-core-design ROADMAP 1.9）；TS v2 复制全量 wire（含未收尾回合，无原子截断、无分叉点参数、无忙碌护栏）+ **双跑式克隆定时任务**（SDR-007 要求改为不带）+ `Fork: <源名>` 命名（SDR-012 定为「Fork of」——v2 待小改，随 1.3 退役线）；**TS 侧 fork 契约无 turnId**（klient/v2/node-sdk schema 均未收——消费端对齐项）；**父子关系（createChild 的 v2 parent linkage）未建模**（rust createChild 复用 fork 管道、`Child:` 命名，无 forkedFrom）；目标重置/挂起交互不带/上报重投递/附件共享不清理均未实现 |
| 分叉的缓存计费（US5.5） | 归属标识随新会话身份（SDR-010 修订裁决）+ 请求前缀稳定（截取管道保证）；首命中不做保证 | v2 `cacheKey` = 当前会话 id（`profileService.ts:549`）、v1 同构（`core-impl.ts:1171`）——已符合修订裁决（无对齐变更）；Rust 无 cache key 配线通路（待补） |
| fork 界面入口与落点（US5.1 界面条款） | 双端入口 + 落点跳转新会话 + 溯源可见 | 入口与落点已上线（§1.5a：TUI `/fork`、web 双菜单、fork 后跳转）；**血缘可见缺失**（forkedFrom 界面未消费） |
| 分叉点选择 UX（US5.4 界面条款） | 回合终点入口（web 悬停/聚焦）+ 分叉点选择器（TUI）+ 忙碌/失败诚实反馈 | 全部未实现（新需求；TUI 选择器可复用 undo-selector 模式，§1.5a） |
| btw 分支（E5） | 会话内旁路分支：工具禁用、追问复用、随会话持久化 | wire `btw.start` 已登记；rust 会话内有 btw fork 概念（会话级 agent），旁路语义与持久化待核 |
| recency 规则（SDR-004） | 仅对话活动/主动恢复刷新活跃时间 | `update_metadata` 无条件刷新 `updated_at`（改名跳顶，待修） |
| 对话撤销（U1） | 统一语义：整轮撤销 + 落盘保真（ADR-006：count 按折叠器消息条数口径）+ 忙碌护栏 + 压缩边界即停与诚实反馈（US-U1~U3） | 三引擎分叉（§1.6）：v1/v2 撤销落盘但记录口径不同（v1 锚点数、v2 undo op + 尺寸 ops）；v2 另有检查点深度限制与忙碌护栏；rust 仅内存锚点回退（**不落盘**、无护栏）——undo 后重开复活被撤内容；`context.undo` 的 count 在 v1 记录（锚点数）与 rust fold（消息条数）两侧口径不同 |
| 会话运行状态（US4.4） | 列表行/快照带四元组 + `work_changed` durable 全局事件 + 重建只认进程内在途回合注册表（不做日志探测兜底，ADR-009）；进程死亡即清零 | rust 两侧均缺（§1.8）：SessionView 无四元组、索引行 busy 恒 false、懒构建 runtime 外的 live 会话刷新后退化为索引行、`work_changed` 无生产者（聚合器已移植但零接线）；v2 即目标形态；v1 五值枚举随退役消化 |

fork/btw/createChild 的 TS 参照事实：命名 `Fork: <源名>` / `Child: <源名>`（`sessionLifecycleService.ts:484,537`）；btw = 主 agent 的轻量分支，工具全禁用、文本作答、追问复用同一分支（`session/btw/btw.ts`）；createChild = fork + `parent_session_id` 溯源标记（TS 用 custom 字段承载）。

## 3. 痛点与风险（开放项，入 ROADMAP）

1. **TS 元数据写回透传未验证**：TS 引擎改写 `state.json`（如改名）时是否保留 `pinned` 未知——TS 对齐批次首要验证项；在此之前，同一数据被两引擎交替写有置顶标记丢失风险。
2. **搜索与归档可见性**：搜索当前包含归档会话（持久化域 context §3.3），与置顶/归档压过关系的完整语义待契约评审时统一裁决。
3. **`archivedOnly` 列表参数**：网页设置页在用、引擎未实现（持久化域 ROADMAP 开放项 #1）——与本域 `session.list` 扩展同批做可一次定稿。
4. **拖拽可访问性**：原生 HTML5 拖放对键盘/读屏用户不友好——置顶/取消置顶必须始终是可点按钮（拖拽只是加速路径），验收在需求 US1.1/US1.2 已覆盖（操作入口不依赖拖拽）。
5. **fork 双跑为已上线行为**：TS v2 的 cron 克隆（§1.4）是已发布功能面；SDR-007 把终态定为"不带"——这是一次**行为变更**而非纯补齐，引擎对齐批次需显式移除克隆逻辑并验证无孤儿任务残留（删除级联不变）。
6. **Rust 撤销不落盘为行为缺口**：`session.undo` 仅改内存（§1.6），撤销后收起重开回到撤销前内容——用户可感知的信号失真（US-U2 立项动因；接口义务与 count 口径的裁决见架构 ADR-006，[session-management-architecture.md](session-management-architecture.md)）。附带口径分裂：`context.undo` 记录的 count 在 TS v1 写入（锚点数）与 rust fold 读取（消息条数）两侧语义不同，跨引擎重放同一日志的截断位置不一致——U1 实现批次一并裁决收敛（ROADMAP 开放项 8）。
7. **Rust 运行状态双侧缺失为已上线行为缺口**：网页刷新后进行中的会话丢"运行中"标记（§1.8）——用户分不清哪些还在跑。终态裁决见 SDR-013/ADR-009；实现批次见 ROADMAP W1。
