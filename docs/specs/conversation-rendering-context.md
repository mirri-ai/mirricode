# 对话渲染 — 现状盘点（context）

> 快照日期：**2026-09-13**（本文所有代码事实以此日仓库状态为准；本文描述现状、不含目标与裁决——那些在 [conversation-rendering-requirements.md](conversation-rendering-requirements.md) / [conversation-rendering-architecture.md)，状态与排期在 [conversation-rendering-ROADMAP.md](conversation-rendering-ROADMAP.md)）。
> 领域：会话日志（S5）→ 对话视图（S6）的投影关系。三份现状：TS v2 引擎+kap-server+web（参照基线）、TUI（参照消费端）、Rust（对齐对象）。

## 1. TS v2 引擎（记录层与投影机制）

### 1.1 记录格式与不变量

- 每身份一份 `wire.jsonl`（`{sessionScope}/agents/{agentId}/wire.jsonl`），首行 `metadata` 记录（协议版本，当前 1.5）；每行一条扁平化操作记录。`packages/agent-core-v2/src/wire/record.ts:14-61`；scope 派生 `workspace/sessionLifecycle/internal/addressing.ts:16-27`。
- 追加即三段处理：折叠进模型 → 追加日志 → 发布总线事件（仅 live；恢复 silent）。`packages/agent-core-v2/src/wire/wireService.ts:244-268`；派发同步串行 + 防环上限。`wireService.ts:105-125`。
- 恢复重放与实时折叠共用同一折叠函数（`foldLoopEvent`，`agent/contextMemory/loopEventFold.ts:1-39`）；未知/畸形记录跳过不失败。`wireService.ts:213-242`。
- 大媒体附件落盘前脱水为 `blobref:` 引用、恢复后重水化。`wire/model.ts:13-27`、`wireService.ts:279-325`。
- 记录类型全集（UI 渲染相关核心）：回合括号 `turn.prompt/steer/cancel/ended`（`agent/loop/turnOps.ts:58-89`）；消息与回合流 `context.append_message/append_loop_event`（step.begin / content.part / tool.call / tool.result / step.end，`agent/contextMemory/contextOps.ts:147-155`、`loopEventFold.ts:51-103`）；压缩 `context.apply_compaction` + `full_compaction.*`；撤销 `context.undo`；用量 `usage.record`；人机交互 `interaction.request/resolved`。完整清单含 goal/plan/permission/profile/task/cron 等约 30 族。

### 1.2 实时事件面

- 回合生命周期：`turn.started`（携带可显示来源的用户输入文本——消息记录不是事件，文本借此上浮，`agent/loop/turnEvents.ts:5-13, 31-114`）、`turn.step.started/completed/retrying/interrupted`、`turn.ended`（结局+原因+时长）。
- 易失流（只走总线不落日志）：`assistant.delta` / `thinking.delta` / `tool.call.delta`，`agent/loop/loopService.ts:1108-1175`；响应完成后批量落 `content.part`（`loopService.ts:869-885`），中断时 drain 已积累分片（`loopService.ts:887-905`）。
- 工具面：`tool.call.started`（名称+参数+展示提示）、`tool.progress`、`tool.result`（输出+失败标记+名称归位）。`agent/toolExecutor/toolExecutorEvents.ts:9-33`、`toolExecutorService.ts:528-609`。
- 聚合视图：`agent.activity.updated`（阶段/活跃工具/待审批/重试，可丢弃重建）。`agent/activityView/activityView.ts:7-76`、`activityViewService.ts:88-160`。
- 中断兜底：未闭合工具调用折叠为带失败标记的合成结果（`loopEventFold.ts:48-49, 230-252`）；断流且带工具调用的回合记调用 + 合成失败结果（`loopService.ts:917-1001`）。

### 1.3 恢复读路径（三份折叠器）

| 路径 | 机制 | 引用 |
|---|---|---|
| 模型视图 | `WireService.restore` 重放 → `getContext` 返回折叠后上下文（压缩坍缩视图） | `wireService.ts:135-206`、`agent/rpc/rpcService.ts:231-236` |
| 全量转录（REST） | 流式读日志 + `contextTranscript` 折叠（保留压缩前全历史、重放 undo/clear）+ live tail 合并；消息 id 为位置推导 `msg_{session}_{seq}`、createdAt 推导 | `app/messageLegacy/messageLegacyService.ts:14-18, 72-100, 157-165`、`agent/contextMemory/contextTranscript.ts:167-240`、`messageProjection.ts:26-133` |
| TUI 回放 | node-sdk 冷恢复日志 → **v1 引擎的 ReplayBuilder** 折叠为回放记录流（v2 无自有 replay builder，靠"磁盘形状与 v1 逐字节兼容"约束）`[v1]` | `packages/node-sdk/src/sdk-rpc-client-v2.ts:690-785`、`v2/resume-replay.ts:7-85` |

已知结构脆弱点：三份折叠器（`loopEventFold` / `contextTranscript` / v1 ReplayBuilder）手工保持语义一致（中断兜底、压缩标记、undo 语义）；REST 全历史投影与 `getContext` 压缩坍缩投影有意分叉。

### 1.4 用量

- 持久：`usage.record` 折叠进会话累计（`agent/usage/usageOps.ts:40-67`）；**每轮累计器不持久化，恢复后为空**（`usageOps.ts:8-11`）。
- 步级：`step.end` 记录与 `turn.step.completed` 事件均带用量 + 5 个时延字段（`loopService.ts:1016-1032, 1066-1090`）。

## 2. TS kap-server（契约层）与 mirri-web（消费层）

### 2.1 REST 读面

- `GET /sessions/{sid}/snapshot`：`{ as_of_seq, epoch, session, messages（升序最近 100 条, has_more）, in_flight_turn{turn_id, assistant_text, thinking_text, running_tools[], current_prompt_id}, subagents, pending_approvals, pending_questions }`。`packages/kap-server/src/routes/snapshot.ts:58, 86-207`、`protocol/rest-snapshot.ts:25-93`。
- `GET /sessions/{sid}/messages`：方向游标（`before_id`/`after_id` 互斥）+ `page_size`（1–100）+ `role` 过滤，newest-first。`routes/messages.ts:83-111`、`protocol/rest-message.ts:12-26`；单条 `GET .../messages/{mid}`（`messages.ts:114-137`）。
- `POST /sessions/{sid}:undo` 响应带消息与状态，web 成功后直接重新快照。`routes/sessions.ts:712-741`。
- 已知契约缺陷：`before_id`/`after_id` 指向不存在消息时返回**从头整页**（`messageLegacyService.ts:76-90`）——与 sessions 列表对未知游标返回空终止的行为不一致（`sessions.ts:433-437`），是可复现的翻页回锅陷阱；`role` 过滤在分页后（`messageLegacyService.ts:92-97`），过滤后页可空但 `has_more=true`。

### 2.2 WS 事件面（协议 v2）

- 游标 `{seq, epoch}`（seq = 持久日志位点，epoch = 世代）。`packages/kap-server/src/protocol/ws-control.ts:20-38`（TS 侧协议定义参照）。
- 订阅/续订/退订/心跳；服务端能力协商（批处理/压缩）。易失帧不推进 seq、带回合内偏移。`ws-control.ts:46-54, 555-714`。
- 重放：每会话内存尾部（默认 1000）+ 磁盘日志两级；溢出/世代变更 → `resync_required`（带原因与当前水位）。`transport/ws/v1/sessionEventBroadcaster.ts:200, 635-673`。
- 事件词汇 51 类（`protocol/events-zod.ts:915-967`，2026-09-12 清点）。

### 2.3 web 消费链与渲染

- 管线：WS 客户端（重连退避、游标续订）→ 客户端投影器（原始事件 → 应用事件；快照播种在途回合**必须先于订阅**）→ rAF 批处理器（分片合并、后台标签定时兜底）→ 纯 reducer → 视图模型折叠（消息 → 回合/卡片）→ 渲染。`apps/mirri-web/src/api/daemon/agentEventProjector.ts:1577-1604`、`composables/client/eventBatcher.ts:33-80`、`eventReducer.ts:482-535`。
- 打开流程：快照 → 陈旧快照防护（水位比对重试一次）→ 安装（保留旧页防闪）→ **先播种后订阅** → 从水位续订。`composables/useMirriWebClient.ts:1370-1517`；重开强制全量快照（易失流不可重放）。`useMirriWebClient.ts:1584-1597`。
- 去重：乐观回声三级匹配（`eventReducer.ts:191-225, 522-531`）；流式副本 vs 正式副本内容签名子集覆盖（`messagesToTurns.ts:547-585`）+ 易失痕迹合并（`:724-736`）。
- 订阅容量：LRU 4 会话，被退订者标记过期游标、重开走全量快照。`useMirriWebClient.ts:1525-1576`。
- 渲染形态：流式 markdown（增量渲染 + KaTeX/Mermaid worker 卸载）、思考块、工具卡（进度行/错误态/diff 面板）、附件 chip、审批卡（十类）、压缩分隔条（reducer 在压缩完成时落一条持久 divider 消息，`eventReducer.ts:436-473`）、错误/中止态、undo 后重新快照（`useWorkspaceState.ts:2657-2659`）、系统注入过滤（`messagesToTurns.ts:494-501`）、向上懒加载（sentinel + 手动重试，`ChatPane.vue:133-177, 534-546`）。
- 已知竞态（代码注释记录型，非结构修复）：快照在途覆盖乐观消息窗口（`useMirriWebClient.ts:1588-1594`）；重订阅不重置投影器依赖顺序保证（`client.ts:1717-1724`）；全局事件推进已退订会话游标导致跳变（`:1525-1576` 注释）。

### 2.4 TS 侧自身已知缺口（参照系的诚实披露）

- **无界面窗口化**：全部已加载回合整段渲染（`ChatPane.vue:551`）——需求 §7 提案 P1。
- **易失流崩溃窗口**：流式中途崩溃已流出内容不保证留存（仅中断 drain 兜底）——提案 P2。
- **消息身份是推导值**（`messageProjection.ts:26-29`）——提案 P3。
- **web 不呈现 shell 事件**（`agentEventProjector.ts:1422-1429`）——提案 P4。
- **TUI 回放丢弃媒体分片**（`apps/mirri-code/src/tui/utils/message-replay.ts:305-318`）——提案 P5。
- `turn.ended` 不进任何模型状态（恢复方需重读日志，`turnOps.ts:4-9`）；每轮用量恢复后丢失（`usageOps.ts:8-11`）；重试上下文不在快照播种内（`agentEventProjector.ts:1059-1087`）。

## 3. TUI（参照消费端，`apps/mirri-code/src/tui`）

- 实时：本地回显用户消息 → 事件流 → 节流合并控制器（`streaming-ui.ts:454-491`）→ 组件树；文本在工具卡开始前定型（`:778-780`）；思考流式尾部滚动窗 + 定型折叠（`thinking.ts:103-137`）；工具卡三态 bullet、流式参数预览、聚类 diff、工具名专用 renderer + 默认截断（`tool-call.ts:1412-1505, 1936-2091`）；连续同类调用分组（`streaming-ui.ts:790-902`）。
- 恢复：回放记录流（最近 10 回合上限，`utils/message-replay.ts:22, 131-141`）→ **复用实时渲染钩子**（`session-replay.ts:342-403`）；转录窗口保留 15 回合 + 图片内存回收（`utils/transcript-window.ts:29-41`）。
- 撤销：本地剪除 + 输入回填 + 压缩边界提示（`commands/undo.ts:44-160, 341-349`）。
- `[v1]` 标记：TUI 恢复路径经 node-sdk 使用 v1 引擎 ReplayBuilder（§1.3）。

## 4. Rust 现状（对齐对象，`rust/crates`）

### 4.1 已对齐部分

- 记录类型全集基本对齐 TS（`mirri-proto/src/wire.rs:47-704`，含压缩保留字段三方言兼容）；`turn.ended` 带结局与时长；`Unknown` 前向兼容。
- 追加日志原语（torn-tail 容忍、逐条 sync、`mirri-persistence/src/append_log.rs:1-32`）；版本链 2.0（v3_sessions 根拆分标记）。
- 事件投影链完整：`SessionEvent`（44+ 变体）→ `event_projection.rs` 统一 `event.*` 词汇（44 方法封闭词汇表）→ mirri-server 每会话环形缓冲（持久事件打 seq+epoch，易失走 seq 0）→ `session.subscribe` 游标重放；`session.snapshot` 的 `as_of_seq` 与同一 channel 对齐（`mirri-server/src/rpc.rs:1536-1542`）。事件词汇与 TS 契约有对账测试（contract-e2e fixture）。
- 恢复：`resume_session_with_limits`（`manager/restore.rs:70-200`）重放折叠 + `reconstruct_context_memory`（`replay.rs:57-171`，含中断工具合成结果、blobref 重水化）。
- 写侧关键点：用户消息 + 回合括号（`mirri-embedded/src/embedded_server/message_handlers.rs:473-479, 599-688`）；循环事件（`mirri-core/src/loop_engine/turn_step.rs:715-725`）；用量（`turn_step.rs:492`）；压缩（`turn_flow/mod.rs:1409`）。

### 4.2 与 TS 基线的差距矩阵（本域视角；2026-09-12 批次 CR1–CR3/CR7 已修复的行标注 ✅）

| # | 能力（需求锚点） | TS 基线 | Rust 现状 | 引用 |
|---|---|---|---|---|
| G1 | 快照在途回合（US3.1） | `in_flight_turn` 完整（文本/思考/运行中工具/提示锚） | ✅ CR2 已修：`SessionArtifacts.in_flight_shadow`（会话级易失影子状态，主身份过滤、按步重置、滞后即重置）供数 | `mirri-core/src/session/in_flight_shadow.rs`、`rpc.rs` 快照填充 |
| G2 | 快照待交互（US3.2） | `pending_approvals/questions` 实数据 | ✅ CR2 已修：读 gates 的 `list_pending()`（行形状对齐 TS 审批/提问请求；`expires_at` 无引擎对应物暂缺省） | `rpc.rs` 快照填充、`session/gates.rs:349-356, 631-638` |
| G3 | 快照子代理名册（US3.1） | `subagents` 实数据 | **恒 `[]`** | `rpc.rs:1478-1479` |
| G4 | 工具结果事件带输出（US2.2） | `tool.result` 携输出 | ✅ CR1 已修：`ToolCallCompleted` 携 `ToolOutput`（untagged 序列化 = 字符串/分片数组） | `mirri-core/src/loop_engine/turn_step.rs`、`session/{events,live_relay}.rs`、`event_projection.rs` |
| G5 | 消息身份与时间（US1.2） | 推导 id + 推导 createdAt，快照/分页行形状同源 | ✅ CR3 已修：`msg-<entry_seq>`（记录序号锚定，压缩/撤销/重启稳定）+ `createdAt`（保序推导）+ 共享行构造 `message_list_row`；快照 `{items}`/分页 `{messages}` 顶层形状保留（TSV 登记形状）但行字段一致 | `rpc.rs` `message_list_row`/`resolve_message_position` |
| G6 | 历史分页契约（US1.3） | REST `before_id/after_id/page_size(≤100)/role` | WS-RPC `beforeId/afterId/pageSize(≤200)`，**无 role 过滤**；REST 无历史读路由（只有 POST 发消息）；游标失效 ✅ CR3 已改为空终止页（优于 TS 回锅的有意改进） | `rpc.rs:1658`、`mirri-server/src/http.rs:43-117` |
| G7 | 状态聚合（US2.5） | 结构化活动视图 + 状态事件 | ✅ CR4 已修：`agent.status.updated` 真负载（8 相位 phase / contextTokens+maxContextTokens+contextUsage 水位 / sessionUsage 切片），生命周期门控发布（Disposed 抑制） | `mirri-core/src/session/agent_status.rs`、`event_projection.rs` |
| G8 | 事件归因对称（US5.4） | 事件按 agent 分通道扇出 | ✅ CR5 已修：`turn.*`/`tool.call.*`/`compaction.*`/`background.task.*`（`info.agentId`）事件带 agentId 归因；`TurnCompleted`/`TurnFailed` 成败对称携带 agent_id | `mirri-core/src/session/events.rs`、`event_projection.rs` |
| G9 | 撤销持久（US5.2） | `context.undo` 落日志 | `session.undo` 只改内存，**不写 `context.undo` op**——undo 后恢复复活被撤消息 | `mirri-embedded/src/embedded_server/session_handlers.rs:1892-1926`（reducer 分支存在：`mirri-proto/src/reducer.rs:225`） |
| G10 | 身份分片记录流（US5.4 前提） | 每身份独立日志（含 btw） | 单流；**子代理/btw 对话零持久化**；fork 不复制日志 | `manager/restore.rs:244-246`、`subagent/host.rs`（无 record_op） |
| G12 | 快照 title（US1.1 附带） | 会话视图带名称 | ✅ CR7 已修：快照会话视图带元数据 title | `rpc.rs` 快照填充 |
| G13 | 记录写侧完整性（ADR-001 前提） | 同步追加 | `record_op` **best-effort 吞错**（含 deferred 缓冲）；已消费的 steer 指令不补落 `context.append_message`（rust-core ROADMAP F-T3-2 登记） | `mirri-core/src/session/session_scope.rs:145-160` |
| G14 | 转录全量（US5.1） | REST 读面流式重读日志，保留压缩前全部历史（压缩落位为标记消息） | 读面读 `context_memory().history()`，**压缩原地改写该历史**——压缩后旧对话从读面消失（仅剩摘要与保留尾），无 TS 式"从日志重建全量转录"的读路径 | `mirri-core/src/context_memory/mod.rs:822`（`self.history = new_history`）、`mirri-server/src/rpc.rs` `message.list`/`session.snapshot` 的历史来源 |
| G15 | 快照/消息行工具调用关联（R1 服务端） | content part 判别符与字段：`tool_use{tool_call_id,tool_name,input}` / `tool_result{tool_call_id,output,is_error}`（TS 协议 messageSchema 权威） | ❌ 序列化层丢 `tool_calls`/`tool_call_id`——assistant 行只剩正文、tool 行只剩裸文本；context memory 实际存储（replay 会重建 tool_calls）；快照与 message.list 共用行构造，两读面同病 → 本批修复 | `mirri-server/src/rpc.rs` `message_content_to_json`/`message_list_row`；`mirri-core/src/context_memory/mod.rs`（replay 重建） |
| G16 | 思考/媒体 part 形状（R1 形状） | `thinking{thinking}` / `image{source{kind,...}}` / `file{fileId,name,mediaType,size}` | ❌ 读面直出 ContentPart 序列化拼写（`think{text}`、`image_url` 包装）与协议判别符+字段名双不符；工具媒体结果 → web unknown → 重渲染丢 → 本批修复（投影于 rpc 序列化边界，磁盘拼写不动） | `mirri-server/src/rpc.rs`（ContentPart 序列化）；`packages/agent-core-v2/src/agent/contextMemory/protocolMessage.ts`（协议权威） |
| G17 | 前端快照路径 part 映射（R1 前端） | `toAppMessageContent` 全量映射为同款 App part（mappers.ts 参照） | ❌ `rpcContentToAppContent` 只映射 text，其余降级 `{type:'unknown'}`；App 类型 `toolUse`/`toolResult` 已定义未接线 → 本批修复 | `apps/mirri-web/src/api/daemon/ws-rpc-api.ts`；`apps/mirri-web/src/api/types.ts`（App 内容类型） |
| G18 | 快照契约 schema 校验（接线保障） | node-sdk rust-engine-contract-e2e 过 klient zod schema + 形状快照 | ❌ `session.snapshot` serve-only 被排除在契约套件外，零 schema 校验——R1 类"输出过不了 TS messageSchema"即从此缺口漏过 → 本批补 web e2e 契约测试（zod 校验真实 serve 输出，`in_flight_turn` 字段按既知偏差豁免） | `packages/protocol/src/rest/snapshot.ts:80`（`sessionSnapshotResponseSchema`）；node-sdk rust-engine-contract-e2e 排除面 |
| G19 | think/text part 记录顺序（浏览器验收发现） | wire 记录序 = 流式到达序（reasoning 先于 content，live 投影即按到达序渲染） | ❌ `finalize_step` 固定先记 `ContentPart` 再记 `ThinkPart`——重渲染后思考块显示在正文之后，与 live 显示（思考在前）相反 → 本批修复（ThinkPart 先记录，验收：刷新后顺序与 live 一致） | `mirri-core/src/loop_engine/turn_step.rs`（`finalize_step` step_events 构造）；守护测试 `should_record_the_think_part_before_the_text_part_when_llm_returns_both` |

> 已对齐项（不列入差距）：步级用量（US2.4）——`usage.record` 落盘 + 步级事件带用量（`wire_token_usage` 同形，`event_projection.rs:904-916`）；创建时权限模式落盘（`session_handlers.rs:368-383` 对非默认模式写 `permission.set_mode`）。

> 已裁决的后续项（2026-09-13 登记，**本批不实施**，换批次再排；均为既有已知差距的延伸登记，现状证据见上文 §2/§4.2 对应行）：
>
> - 快照 session view 补 usage：session 视图无 usage 四桶，靠 `session.list` 行 + `session.status` 旁路恢复；`turnCount` 仅投影器内存，刷新归零。
> - subagents 名册：快照 `subagents` 恒空（G3）、web 硬编码 `[]`；仅 detached 任务可靠 `task.list` 轮询恢复，前台子代理流式进度不可恢复（跨域：子代理/持久化域）。
> - 消息真实时间戳：`createdAt` 为推导值（会话创建时间 + position），非真实时间（G5 延伸）。
> - 气泡合并辅助字段：消息行无 `promptId`/`status`/`durationMs`，turn 边界靠启发式。
> - turn_id 类型统一：approval 行 TS 契约声明 number、rust 发 `turn-<uuid>` 字符串——既知偏差，契约测试对该字段豁免（架构 ADR-009 的豁免依据）。
> - 工具结果注入/压缩后提醒的重放守护：wire 重放等价不变量测试当前覆盖 per-step 注入（tools_added，浏览器验收与 ACP 回归均实证在场）；SkillTool 式工具结果注入与压缩后后台任务提醒的落盘路径无专门夹具，补覆盖待排。
> - 游标部分驱逐盲区：ring 只驱逐前缀时（cursor 之后的记录仍有留存）不触发 resync 告警——旧有行为，非本批回归；收紧需把 ring 最低保留 seq 纳入 `cursor_replay_bypassed` 判断。

### 4.3 Rust 侧已知裁决/悬置（引用不重复）

事件投影域悬置项（工具结果输出、状态模型、compaction turnId、事件 agentId、SubagentSuspended 无发射方等）登记于 `docs/specs/rust-core-design/ROADMAP.md` P7 段；方法 parity 以 `plan/rust-v2-method-parity.md` 的 `authoritative-methods.tsv` 为唯一事实源；身份分片记录流的终态设计见持久化域 ADR-006（`session-persistence-architecture.md`）。

## 5. web 与 Rust 的连接现状

- web 已连接 mirri-server：WS JSON-RPC `${origin}/api/v1/rpc` + REST 混合。`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:110`。
- 历史读全走 WS RPC：`session.snapshot`（`:1294`）+ `message.list`（`:620-622`，role 过滤被丢弃）；事件流 `session.subscribe`（`:985-995`）。
- web 投影器只折叠主身份帧（`MAIN_AGENT_TRANSCRIPT_FRAMES`，`agentEventProjector.ts:39-70`）；子代理帧走任务面板不进转录。
- 由于 G1（在途回合恒 null），web 打开进行中会话时播种面拿不到数据——半成品回复不可见，只能等回合结束后的持久记录补齐。

## 6. 缺陷清单汇总（按用户影响排序；2026-09-12 批次 CR1–CR3/CR7 修复后）

1. ~~**进行中回复重开不可见**（G1/G2）~~ ✅ 已修（CR2）：影子状态供数快照在途回合与待交互项；G3（子代理名册）仍恒空——重开时子代理卡仍缺失。
2. ~~**工具卡"完成但无输出"**（G4）~~ ✅ 已修（CR1）：`tool.result` 事件带输出，web 原有消费即渲染。
3. ~~**消息无时间、身份漂移**（G5）~~ ✅ 已修（CR3）：entry_seq 锚定身份 + createdAt + 双读面行形状统一。
4. **撤销不持久**（G9）：undo 后重开/换端，被撤轮次复活——视图与真实历史分叉。
5. ~~**状态占位与归因缺失**（G7/G8）~~ ✅ 已修（CR4/CR5，2026-09-18）：`agent.status.updated` 真负载供数 web 上下文水位/用量展示；事件族带 agentId 归因，子身份活动不再误归主对话。
6. **子代理对话不落盘**（G10）：重开后子代理历史不可恢复（跨域：持久化域 ADR-006 终态，本域只列影响）。
7. **压缩后转录坍缩**（G14）：读面读内存 history，压缩后旧对话从读面消失（全量转录读路径缺失，CR10）。
