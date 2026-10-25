# 事件投递 — 现状盘点（context）

> 快照日期：2026-10-23 · 本文档只记录代码事实与痛点，不含目标与决策（决策见本域 requirements / architecture）。
> 本文是事件投递域（event-delivery）现状侧的唯一事实源。

## 1. 服务端推送架构（rust v3，web 实际连接形态）

### 1.1 部署形态

桌面端 = `mirri web`（`rust/crates/mirri-cli/src/main.rs:22` 起）→ `EmbeddedServer`（`rust/crates/mirri-embedded/`）→ HTTP/WS 由 `mirri-server` 承载（`rust/crates/mirri-server/src/ws_rpc.rs`）。SPA 静态资源由 server 内嵌（`web_embed.rs`）。

### 1.2 订阅模型（mirri-server ws_rpc.rs）

- `session.subscribe`（`ws_rpc.rs:899`）：**按连接**订阅单个会话，可选 cursor（seq/epoch）重放。订阅建立 per-session forwarder（`spawn_event_forwarder`，`ws_rpc.rs:1669`）。
- durable 事件：打 seq、入 per-session 环形缓冲（容量 256，`EVENT_LOG_CAPACITY`，`ws_rpc.rs:114`），可重放。
- volatile 事件（`mirri-core/src/session/event_projection.rs:985` 定义的 7 类：assistant.delta、thinking.delta、agent.status.updated、tool.call.delta、tool.progress、shell.started、shell.output）：不落缓冲、不重放、订阅期实时转发。
- 服务端无 delta 合并/节流：仅把同一写循环内待发通知打包成一个 JSON 数组帧（`write_outbound`，`ws_rpc.rs:1556`）。

### 1.3 全局广播（嵌入式引擎侧）

- `EngineAdapter::subscribe_events`（`rust/crates/mirri-embedded/src/embedded_server/mod.rs:4891`）：engine→server 的单条通道，**该通道上每个 session 的全部事件（含 delta）对所有 subscriber 可见**（`event_relay.rs:93-173`，`try_send` 逐条转发）。
- `event.session.work_changed`：SDR-013/ADR-009 已裁决全局扇出，**嵌入式全局流已实现并有测试**（`embedded_server/tests.rs:13579` `should_broadcast_work_changed_after_each_turn_boundary_on_the_global_stream`）。
- `event.workspace.*`：服务器级事件，广播给所有 subscriber。
- **反向 RPC**（approval.ask/question.ask，阻塞 turn 等待回答）：`WsReverseRpcCallback`（`ws_rpc.rs:717`）挂在 session artifacts 上，last-subscriber-wins，仅订阅该会话的连接可收到（`ws_rpc.rs:1066-1077`）。

### 1.4 phase 帧频率（agent.status.updated）

`ActivityTracker` 的每个 phase 迁移记录方法都调用 `publish_status()`（`rust/crates/mirri-core/src/turn_flow/activity.rs:184` 起）：on_turn_started / on_turn_ended / on_llm_request / on_llm_response / on_tool_call / on_tool_call_end / on_retry / on_approval_pending / on_approval_resolved / on_ending / on_ending_cleared。一个正常回合（多次工具调用）每会话产生十至数十帧。

## 2. 客户端消费链（apps/mirri-web）

### 2.1 连接与订阅

- 单条懒加载 WS（`useMirriWebClient.ts:1181` connectEventsIfNeeded）。
- `MAX_WS_SUBSCRIPTIONS = 4`（`useMirriWebClient.ts:1714`）：MRU 顺序保留最近打开的 4 个会话订阅，active 永远保留；被驱逐会话记 `sessionsWithStaleCursor`，重开走 snapshot 重建（`reopenSession`，`:1827`）。注释自述该 cap 是"UI sluggish once I have a lot of sessions"的止血补丁。
- 帧分类（`ws.ts` classifyFrame）：projected 协议帧走 `onWireEvent` → `toAppEvent`；raw agent 帧走 `onRawAgentEvent` → 客户端 projector（`agentEventProjector.ts`）。**收到非订阅会话的帧不做前置丢弃**，交给 reducer（见 2.3）。

### 2.2 事件批处理（eventBatcher.ts）

- rAF 合并 + coalesce：白名单 `RENDER_EVENT_TYPES = {assistantDelta, agentDelta, toolOutput, taskProgress}`（`eventBatcher.ts:11-16`），相邻同 offset delta 拼接，超大 delta 分片，每帧上限 100 项 / 32KB，50ms setTimeout 兜底。
- **`sessionUsageUpdated`、`sessionWorkChanged` 等不在白名单**——每帧独立穿透批处理。

### 2.3 reducer 与写回（性能痛点核心）

- `reduceAppEvent` 对每个事件 `cloneState` 克隆全部 16 个 map（`eventReducer.ts:136-160`）；仅 `sessions` 数组做了引用复用。
- `applyEvent`（`useMirriWebClient.ts:891-929`）**无条件把所有字段写回 `rawState`**（不做引用比较）。Vue 响应式按"属性被赋新引用"判失效。
- 后果：任何会话的任何事件（含 inactive 会话的 delta）都使 `messagesBySession`/`tasksBySession` 等换新引用 → active 会话的 `turns`/`tasks`/`swarms` computed 失效 → **active 会话全量重派生**（`turns` 每次 `messagesToTurns` 全量重建，`useMirriWebClient.ts:2134`）。

### 2.4 高频事件的具体写回形态

- `assistantDelta`（`eventReducer.ts:747-782`）：整消息数组 map 克隆 + 目标消息 content 数组克隆 + 文本拼接。
- `taskProgress`（`eventReducer.ts:934-958`）：整 `tasksBySession[sid]` 数组 map 克隆，每个 task 的 outputLines 追加。
- `sessionUsageUpdated`（`eventReducer.ts:521-529`）：整 `sessions` 数组 map 克隆（仅 usage/model 字段变化）。每次 `agent.status.updated` phase 帧都触发（`agentEventProjector.ts:1395-1423`）。
- `messageCreated`（`eventReducer.ts:624-633`）：每条新消息把该会话 `updatedAt` bump（`createdAt > s.updatedAt` 时）→ 触发 sidebar 重排序。

### 2.5 sidebar 派生链

- `workspaceGroups`（`useMirriWebClient.ts:2705-2733`）：`rawState.sessions.toSorted(...)`（比较函数内 `new Date()` 逐次解析）+ 每 session 重建 view 对象 + `formatTime`。30s 一次 `sessionTimeClock` 强制重算（`:1878`）。
- 会话内排序 = 纯 recency（`updatedAt` 降序），无置顶/固化协议落地（session-management 域 US2/SDR-001..004 的 rust 侧实现未落地——排序是 web 端本地派生）。
- 排序跳动的直接机制：并行运行会话的每条 assistant 消息 `messageCreated` 都 bump `updatedAt` → 行跳顶。

### 2.6 已有减负措施（历史优化）

- eventBatcher（见 2.2）。
- task 轮询减负：`useTaskPoller.ts` 只轮询 activeOnly、1s 间隔、4 并发（历史教训注释："整列表轮询曾把渲染进程冻住"）。
- `taskClock`（`useTaskPoller.ts:273-288`）：running task 存在时每 1s tick 强制 `tasks` computed 重算。

## 3. 痛点清单（2026-10 用户实测）

1. **渲染进程 CPU 110%**（Activity Monitor 实测，`Mirri Code Desktop Helper (Renderer)`）：多主任务并行 + 各自子代理时 UI 卡顿。
2. **多主任务乘法效应**：订阅上限 4 会话的流式事件全量到达；每个事件都触发 active 会话的 turns/tasks/swarms 全量重派生（2.3）。
3. **sidebar 行跳动**：并行运行会话反复重排序（2.5），用户"想点的那行跑了"。
4. **approval/question 触达缺口**：反向 RPC 仅订阅会话可收到（1.3），被 LRU 驱逐的会话挂起审批时前端无感知。
5. **流式期间 shiki 全量重高亮**：streaming Markdown 强制 shiki renderer + `allowBatchRender=false`（`Markdown.vue:98-119`），未完成 turn 不享受降级（`markdownPerformance.ts:16-40` 只覆盖已完成 turn）。
6. **无虚拟化**：ChatPane turns 全量 v-for；TasksPane 输出行全量渲染。

## 4. 相关域已有裁决（不可冲突）

- session-management SDR-004（什么算"使用"）、SDR-002（新条目插入普通区顶部）、US4.4/SDR-013/ADR-009（运行状态双侧可见、work_changed 全局扇出）。
- conversation-rendering ADR-002（原子快照+水位游标+订阅重放）、ADR-003（事件双轨：durable 带游标 / volatile 回合内偏移）。
