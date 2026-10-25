# 事件投递 — 架构设计

> 本文是事件投递域（event-delivery）技术侧的唯一事实源（内部规格文档，不进文档站构建，中文单语）。
> 需求见 [event-delivery-requirements.md](event-delivery-requirements.md)；现状盘点见 [event-delivery-context.md](event-delivery-context.md)。
> 引用既有裁决：会话管理域 SDR-014（使用口径）、SDR-013/ADR-009（运行状态四元组、work_changed）；对话渲染域 ADR-002/003（快照+游标+重放、事件双轨）——本域不改动其契约。

## 1. 目标与设计原则

1. **投递分级以消费者状态为准**，不以连接形态为准（SDR-001）。
2. **位置与状态解耦**（SDR-002）：排序键只随"使用"事件变化（口径 = 会话管理域 SDR-014），状态走徽章通道。
3. **每帧一次提交、失效面按会话隔离**（SDR-003）。
4. 渐进落地：前端可先行的部分（渲染层）不阻塞服务端分级（协议层），每阶段独立可验收、可回退。
5. 不改变对话渲染域已裁决的快照+游标+重放一致性模型——分级只作用于实时面。

## 2. 分级投递模型（SDR-001 的技术形状）

### 2.1 事件分级目录

「现状」列标注既有事件与需新增的事件（新增项须登记 mirri-proto 协议面）。

| 级别 | 事件（wire 名） | 现状 | 投递规则 |
|---|---|---|---|
| **互动关键级** | `event.approval.requested/resolved` | 已有（durable，仅随 per-session 订阅投递，未全局路由） | 广播给**所有连接**，不依赖订阅；刷新后由重放/读面恢复 |
| | `event.question.requested/answered/dismissed` | 已有 | 同上 |
| | 回合失败摘要事件（`event.turn.failed`，含 session/turn id/reason 摘要负载，不含完整 delta） | **新增**（当前失败结局折叠进 `turn.ended` 的 reason 字段） | 同上 |
| | `event.background.task.terminated` | 已有（durable，仅随 per-session 订阅投递，未全局路由） | 同上 |
| **状态摘要级** | `event.session.work_changed` | 已有（引擎层全局扇出已实现，**WS 连接层未接线**，见 §2.2） | 广播给**所有连接**；durable + 全局扇出（ADR-009） |
| | `event.session.updated/deleted`、`event.session.meta.updated`、`event.workspace.*` | 已有 | 广播给**所有连接** |
| **内容流级** | `event.assistant.delta`、`event.thinking.delta`、`event.tool.call.delta`、`event.tool.progress`、`event.agent.status.updated`、`event.tool.call.started`、`event.tool.result`、`event.turn.started/ended`（正常结局）、`event.subagent.*`、`event.shell.*`、`event.turn.step.*`、`event.compaction.*`、`event.goal.updated` | 已有 | **仅投递给订阅了该会话且模式为 full 的连接**；durable 类照常落日志（重放语义不变——切换回会话时由快照+重放恢复） |

判定落在服务端：分级目录是**服务端常量表**，客户端只声明消费模式，不参与裁决。

### 2.2 连接级全局通道（新增组件）

**现状边界（关键）**：`event.session.work_changed` 的全局扇出目前只在**引擎→调用方**那条嵌入式流上实现（`EngineAdapter::subscribe_events`，event_relay 广播全部 session 的全部事件）；web 实际连接的 `mirri-server` WS 层（`ws_rpc.rs`）**没有组件消费这条全局流**——连接上只有 per-session forwarder。因此"广播给所有连接"需要新增 ServerState 级全局通道组件：

- **单一全局订阅**：ServerState 持有一次 `engine.subscribe_events()` 订阅（OnceLock 守护单次 spawn，对齐现有 `mcp_event_forwarder` 模式——`state.rs` 的 mcp_event_tx/OnceLock 先例）。
- **集中过滤再扇出**：全局 forwarder task 按分级目录过滤（只放行互动关键级 + 状态摘要级），经一条 `broadcast::Sender<JsonRpcNotification>` 扇出；每个连接 `run_connection` 起一个 per-connection drain task（subscribe 时 join，断开时 drop）——**每连接不各自订阅引擎全局流**（否则每连接都解析全量 JSON delta，成本随连接数二次增长）。
- **广播通道容量与 lag 策略**：互动关键级/状态摘要级均为低频 durable 事件，通道容量保守即可；drain lag 时连接走既有 resync 路径（广播通道 lag 丢弃 → 客户端 seq-gap 检测 → 快照重建），不引入新机制。

### 2.3 订阅模式与协议扩展

`session.subscribe` params 增加可选 `mode: "full" | "digest"`（默认 `full`，向后兼容）：

- `full`：现状行为——该会话的**内容流级**事件（per-session forwarder 只放行内容流级，见下条排他规则）。
- `digest`：该会话只收互动关键级 + 状态摘要级（内容流级被 forwarder 过滤）。
- **通道职责排他**：互动关键级与状态摘要级**唯一**由全局通道（§2.2）承载——per-session forwarder（两种模式）一律过滤掉这两级，不重复投递（避免同一连接收到双份 work_changed，durable seq 的重复投递对客户端 seq 推进/gap 检测的影响不定义）。"full 不做过滤"的语义限定为内容流级。
- **replay 同样按模式过滤**：digest 订阅带 cursor 的重放也按目录过滤（不灌入内容事件）；digest 会话的 cursor 落后于真实日志水位是**设计内状态**（被过滤的 durable 事件 seq 未被客户端观察），恢复一律走快照重建——digest 重开必须按 `sessionsWithStaleCursor` 语义处理（快照重建，不做 cursor 续传）。
- 互动关键级与状态摘要级**绕过订阅与模式**：经 §2.2 全局通道投递，不论该会话是否被订阅、模式如何。取消订阅后仍持续到达。
- **反向 RPC 挂接只认 full 订阅**：`subscribe_session` 在 `mode=digest` 时**跳过** `reverse_rpc_callback` 挂接（避免后到的 digest 订阅以 last-subscriber-wins 抢走回答权——例如端 1 正 full 查看会话 B，端 2 仅为保活对 B 发 digest 订阅，不应夺走 `approval.ask` 路由）。降级路径（full→unsubscribe→digest re-subscribe）依赖 slot 残留语义保持回答能力：unsubscribe 不清 callback slot——**依赖的代码事实**（`unsubscribe_session` 只 abort forwarder 不动 slot；连接 teardown 不清槽位有 approval 域 ADR-008 锚定，`session.unsubscribe` 的槽位语义**未裁决**），本连接仍能收到 ask，配合全局通知引导用户切回；W2 落地时把 unsubscribe 的槽位语义固化为契约（approval 域补 ADR 或本域登记）。此依赖登记进 §6 边界表。
- MRU 4 上限**退役**（W3 批次）：digest 订阅成本 = 状态迁移帧，接近零；full 订阅任意时刻至多一个。

### 2.4 digest 的客户端适配（配套机制改造）

digest 模式不完整事件流与三个现有机制的冲突及处置：

1. **seq-gap 检测**（`reconcileOnSeqGap`）：guard 从"在 wsSubscriptionOrder"改为"在 full 订阅集合"——digest 会话天然有 seq 洞（内容事件落日志打 seq 但被过滤），否则每次全局 work_changed 到达都触发快照重建风暴。
2. **turn watchdog**（`checkTurnWatchdog`）：同样只对 full 订阅会话生效——digest 会话"turnActive=true + 内容流静默"是正常态，不是死信号。
3. **turn-end 副作用**：digest 收不到正常结局的 `turn.ended` → 投影不出 `turnActiveChanged` → `onMainTurnEnd` 的队列 drain 不跑。非 active 会话的排队消息 drain 改挂 `sessionWorkChanged`（busy/mainTurnActive → false 分支）兜底；moon/完成通知等 UI 面副作用对非 active 会话本就不可见，无义务。
4. **reducer 状态一致性**：digest 期间 `messageCreated` 等被过滤——无害，因为升级 full 走快照全量覆盖（`reopenSession` → `syncSessionFromSnapshot`）；`turnActive` 由 `sessionWorkChanged` 熄灭（既有 reducer case）；digest 重开强制快照重建（第 3 条 replay 过滤）。

### 2.5 前端消费状态机

web 客户端维护"每会话消费模式"表：

- active 会话 → `full` 订阅（打开时快照+游标建立；切换走既有重放恢复语义）。
- 非 active 的已加载会话 → `digest` 订阅（徽章数据源；不占流式带宽）。
- 未加载会话 → 无订阅，仅靠全局通道的两级事件维持列表行。
- 打开 digest 会话（快照重建）→ 升级为 full；切走 → 降级回 digest。
- `retainWsSubscription`/eviction/`sessionsWithStaleCursor` 整套 LRU 逻辑重写为 full/digest 双集合结构（MRU cap 移除后 `wsSubscriptionOrder` 退化为 full 集合追踪 + digest 集合追踪）。

## 3. 渲染层修复（SDR-003 的技术形状；W1 批次，不依赖 §2）

### 3.1 失效面隔离：选择性写回（nested-key 硬约束）

**实现约束（决定成败）**：写回必须用 **nested-key 赋值**（`rawState.messagesBySession[sid] = next`），**禁止整 map 替换**（`rawState.messagesBySession = newMap`）。Vue 3 的依赖追踪粒度是 `(target, key)`：整 map 替换 trigger `(rawState, 'messagesBySession')`——所有读过该属性的 effect 全部失效（含 active 会话的 `turns`，无论哪个会话的事件）；nested 赋值只 trigger `(mapProxy, sid)`——只有依赖该 sid 的 effect 失效。因此：

- `applyEvent` 的写回从"reducer 快照 → 整字段回填"改为"reducer 快照 → 逐字段引用比较 → 变化者以 nested-key 形态写回"。
- 现有 mutation funnel 家族（`setSessionMessages`/`updateSessionMessages` 等整 map spread 形态）改造为 nested 赋值形态，funnel 文档注释同步更新。
- `cloneState` 的 map 克隆改为"未修改字段沿用原引用"（map 层引用复用 + 目标会话条目数组替换）。`advanceSeq` 对 `lastSeqBySession` 的每事件写入同样用 nested 赋值（该 map 目前只在非响应式读取中使用，风险低但一并规范）。
- **旁路写回纳入影响面**：`sessionUsageUpdated` 分支对 `swarmModeBySession`/`planModeBySession`/`collaborationModeBySession`/`thinkingBySession` 的无条件 spread 替换、`processEvent` 对 `promptIdBySession` 的写回——全部改为值比较 + nested 赋值。
- **已知例外**：`planReviewByToolCallId` 是跨会话共享 map（按 toolCallId 键），任何会话的 plan review 事件都会弄脏 active 会话的 `turns`——低频，接受为已知例外并写入 reducer 的引用复用约定注释。

### 3.2 每帧一次提交：批处理白名单扩展

`sessionUsageUpdated`、`sessionWorkChanged`、`sessionMetaUpdated` 加入 eventBatcher 合并白名单（同帧合并、去重保留末帧）。后台会话的纯 usage 帧丢弃规则：非 active 会话且无 usage 展示组件挂载时在批处理器内丢弃（判定函数单点实现，不散落）。

### 3.3 排序键与状态解耦：派生层拆分

- 排序键从 `updatedAt` 改为 `lastUsedAt`（会话视图字段，服务端持有与更新，仅由使用事件刷新——产生点见 §3.3.1）。
- `messageCreated` 对 `updatedAt` 的 bump 逻辑移除（回合内内容产出不构成使用，会话管理域 SDR-014）；`updatedAt` 回归纯元数据时间戳。
- 徽章与排序派生分离：`workspaceGroups` 只依赖 `{id, lastUsedAt, title}` 投影排序（排序 computed 内缓存 epoch 数值，避免比较函数内逐次 `new Date()`）；行徽章组件按 sessionId 订阅自己的四元组与 30s 时钟，不经由 `workspaceGroups`。
- `formatTime`/`busy` 等内联计算从 `workspaceGroups` 移除，归位到行组件。

#### 3.3.1 lastUsedAt 的服务端产生点（W1 批次）

- **用户发送消息**：写在 message 提交 handler 内部（引擎收到用户消息的落点，非客户端事件）。
- **用户主动打开**：新增显式轻量 RPC `session.touch`——**由前端仅在用户交互路径（点击会话行）调用**。不能复用 `session.restore`/`session.snapshot` 作为产生点：restore-on-demand 会在任意连接首次读面（列表页打开、resync、快照同步）时自动触发，无法区分用户主动与否。
- `lastUsedAt` 持久化进会话记录，进 `session.list` 行与 snapshot 视图（多端一致，与四元组同路——ADR-009 先例）。

### 3.4 高频热点收敛

- `taskProgress` 写回按 task 粒度：只替换 `tasksBySession[sid]` 中目标 task 的条目（数组内定位替换而非整体 map 克隆），thinking/text 追加用字符串构建器每帧 flush 一次拼接。
- streaming Markdown 高亮降级：streaming turn 的代码块走 `pre` + 等宽着色，`turn.ended` 后一次性高亮（复用 `markdownPerformance.ts` 既有降级通道，扩展适用范围至 streaming）。**显式取舍**：turn 结束时 renderer 翻转导致的一次性重挂载视觉跳变被接受（markstream 以 renderer 值为 key，翻转即 remount；现状完成后 shiki→pre 降级翻转同样存在）。
- 虚拟化（长会话/长输出）：二期评估（先修失效面，虚拟化独立立项）。

## 4. 关键机制的动机（抽象问题陈述）

- **为什么分级目录在服务端**：客户端过滤救不了网络与解析成本；且"哪些事件属于哪级"是协议语义，两个客户端（web/TUI）必须一致，放在客户端会分裂。
- **为什么 digest 是订阅模式而不是新连接**：复用既有 per-session forwarder 与 cursor 机制，一条连接一个会话一个模式，无新连接生命周期管理。
- **为什么全局通道是 ServerState 级单订阅 + 集中过滤**：每连接各自订阅引擎全局流意味着每连接解析全量 JSON（含所有会话的 delta）——成本随连接数二次增长；单点过滤后扇出只传轻量帧。
- **为什么反向 RPC 挂接只认 full**：digest 订阅若挂接 callback，后到即抢走回答权（last-subscriber-wins），把 `approval.ask` 路由到没开面板的端；通知事件已解决触达，回答权只给真正在看的人。
- **为什么 MRU 4 退役**：该上限是"订阅=全量"结构错配下的止血补丁；digest 成本接近零后失去存在理由，且它正是 approval 触达缺口的直接成因。

## 5. 决策记录（ADR）

### ADR-001 服务端分级目录 + digest 订阅模式 + ServerState 级全局通道

- **编号与日期**：ADR-001 · 2026-10-23
- **背景与动机**：分级投递的裁决点在服务端（§4 动机）；现状 WS 层没有全局通道（work_changed 的扇出只在引擎→调用方流上，WS 连接层未接线——context §1.3），"广播给所有连接"需要新组件。
- **决策**：§2.1–§2.4 全部。分级目录为服务端常量表并登记 mirri-proto；`mode` 缺省 `full` 保证旧客户端零感知；全局通道 = ServerState 单次引擎订阅 + 目录过滤 + broadcast 扇出 + per-connection drain（OnceLock 模式对齐 mcp_event_forwarder 先例）；digest 配套改造（seq-gap/watchdog guard 改 full 集合、turn-end 队列 drain 兜底挂 work_changed、重放按模式过滤、重开走快照）。
- **否决的替代**：(a) 新开一条全局 digest 连接——多一条连接生命周期、重连恢复面翻倍；(b) 客户端声明事件白名单（per-connection filter 参数）——表达力过强，等于把协议语义交给客户端配置，两客户端易漂移；(c) 只保留 full 订阅 + 前端丢弃——网络/解析成本仍在（SDR-001 否决项 b）；(d) 每连接各自订阅引擎全局流——成本随连接数二次增长（每连接解析全量 delta）。
- **影响面**：§2 全节；mirri-proto 协议登记（新增 `event.turn.failed`；`session.subscribe` 的 `mode` 参数；`session.touch` RPC；`event.approval.requested/resolved`、`event.background.task.terminated`、`event.question.*`、`event.session.work_changed` 已在词表中，无需新增）；mirri-server `ws_rpc.rs`（forwarder 分级过滤 + 全局通道接线 + digest 跳过反向 RPC 挂接）；web `ws.ts`/`client.ts`/`useMirriWebClient.ts` 订阅面与配套机制。

### ADR-002 选择性写回（nested-key 硬约束）+ 批处理白名单扩展作为渲染层第一刀

- **编号与日期**：ADR-002 · 2026-10-23
- **背景与动机**：多主任务卡顿的第一放大器是"每事件全字段整 map 写回弄脏全部消费面"（context §2.3）；Vue 3 的 `(target, key)` 追踪粒度使 nested-key 赋值天然实现按会话隔离，但现有 funnel 全是整 map 替换形态，照旧实施引用比较形同虚设。
- **决策**：§3.1–§3.2 全部，含 nested-key 硬约束、funnel 改造、旁路写回收编、planReviewByToolCallId 已知例外。reducer 的引用复用约定写成 `eventReducer.ts` 模块级文档注释。
- **否决的替代**：(a) 每会话独立响应式根（SDR-003 否决项 b）；(b) immutable 库（immer）接管写回——引入依赖与学习成本，引用比较已足够；(c) 保持整 map 替换只做引用比较——cloneState 每次浅拷贝出新 map，比较永远失败，无效果。
- **影响面**：`eventReducer.ts`、`useMirriWebClient.ts`（applyEvent + funnel 家族 + sessionUsageUpdated/processEvent 旁路）、`eventBatcher.ts`。

### ADR-003 排序键独立为 lastUsedAt，产生点 = 消息提交 handler + session.touch

- **编号与日期**：ADR-003 · 2026-10-23
- **背景与动机**：排序跳动的根因是排序键与内容产出耦合（context §2.5）；SDR-014 收窄了使用口径，服务端需要与之匹配的产生点——且"用户主动打开"在服务端无法从现有 RPC 区分（restore-on-demand 自动触发，快照拉取既可能是用户点击也可能是 resync）。
- **决策**：§3.3 + §3.3.1。`lastUsedAt` 为服务端持有的会话视图字段，持久化进会话记录，更新点恰为使用清单两项（消息提交 handler、`session.touch` RPC——前端仅在用户点击会话行的路径调用）；进 `session.list` 行与 snapshot 视图。
- **否决的替代**：(a) 前端本地维护 lastUsedAt——刷新后丢（违背多端一致）；(b) 复用 updatedAt + 客户端过滤 bump——两个语义挤一个字段，重蹈 SDR-004 否决项的历史；(c) restore/snapshot 兼任 touch——无法区分用户主动与自动恢复，列表页打开就会 bump 所有行。
- **影响面**：`session.list`/`snapshot` 契约（新增字段 + `session.touch` RPC）；web `workspaceGroups` 派生与行组件；rust 消息提交 handler；mirri-proto 登记。

### ADR-004 流式高亮降级：未完成回合的代码块用浅色渲染，完成时一次性高亮

- **编号与日期**：ADR-004 · 2026-10-23
- **背景与动机**：streaming 期间每帧对全部代码块重新语法高亮是渲染单会话的最大热点（context §3.5）；完成前的中间态高亮用户无感知价值。
- **决策**：§3.4。streaming turn 的代码块走 `pre` + 等宽着色，`turn.ended` 后一次性高亮（复用 `markdownPerformance.ts` 降级通道，扩展至 streaming）。接受 turn 结束时 renderer 翻转的一次性 remount 视觉跳变（现状完成后降级翻转同样存在，跳变幅度不增加）。
- **否决的替代**：(a) 增量高亮（按行 diff）——shiki 不支持增量 API，自研成本不成比例；(b) 降帧率高亮（每 N 帧一次）——中间态闪烁，体验更差。
- **影响面**：`Markdown.vue`、`markdownPerformance.ts`、对话渲染域 §2.5 消费端管线（呈现层，不动转录语义）。

## 6. 与既有契约的边界

| 契约 | 本域动作 |
|---|---|
| 对话渲染 ADR-002（快照+游标+重放） | 不改。digest/full 切换走既有快照重建路径 |
| 对话渲染 ADR-003（事件双轨） | 不改 durable/volatile 划分；分级是投递面第三维 |
| 会话管理 SDR-013/ADR-009（work_changed 全局扇出） | 引擎层既有实现是起点；WS 层全局通道（§2.2）是其在连接面的接线，口径不变 |
| 会话管理 SDR-014（使用口径） | 消费该裁决；lastUsedAt 产生点与之对齐（§3.3.1） |
| 反向 RPC（last-subscriber-wins） | 不改语义；digest 订阅跳过挂接（防抢答）；降级路径依赖 slot 残留保持回答能力——连接 teardown 不清槽位有 approval 域 ADR-008 锚定，`session.unsubscribe` 的槽位语义未裁决，W2 落地时固化为契约 |
| MRU 4 订阅上限（前端止血补丁） | 退役（W3），由分级模型取代 |
