# 多 Agent 督导域 — 现状盘点（Context）

> 快照日期：2026-09-14。本文只描述现状与差距，不含目标与决策（决策见 requirements/architecture 的 SDR/ADR；排期见 ROADMAP）。
> 数据源：web 端（apps/mirri-web）与 rust 端（rust/crates）源码通读；真实会话 `session-6778695f-9c00-4247-93a7-dde1be76fdea` 的落盘数据实证；外部产品调研报告的分拣结论。

## 1. 域起源：真实用户反馈

本域源于一次真实交互（2026-09-14，会话 `session-6778695f`，蜂群委托两个成员 scaffold/tests 创建 TypeScript Hello World 项目）：

- **实时渲染**时，AgentSwarm 卡片在主会话消息区**自动展开**，成员的工具调用记录、流式输出直接进入对话区视野——用户反馈"这会污染主会话区域展示的整洁性；不是所有用户都对 sub-agent 的 activity 感兴趣"。
- **刷新后**，同样的内容消失、卡片收起——用户反馈刷新后的显示反而符合预期。

即：**实时与回看两种观感不一致，且实时观感是用户不接受的那种**。这构成本域 SDR-002（对话区零过程细节）与 US1.1（实时与回看观感一致）的直接动机。

## 2. 呈现层现状：两套并行演化的委托卡片

### 2.1 单个 `Agent` 委托（已接近目标形态）

`apps/mirri-web/src/components/chat/tool-calls/AgentTool.vue`：

- 主区一行收起（状态 · 摘要 · "Sub Agent" 徽章 · 型号徽章 · 耗时）；
- "打开详情"按钮（`tasks.openDetail`）跳右侧面板；无可看管任务时按钮自动隐藏（`canOpenAgent` 守卫，:80-88）——即需求 US1.3 的入口显隐判定雏形；
- 展开态显示任务原文与最终结果输出，无过程细节。

右侧详情：`apps/mirri-web/src/components/chat/AgentDetailPanel.vue`（单成员）——身份（类型/型号/阶段徽章）、任务原文、实时输出（流式）、进度区（"Calling X" 分组 + 输出折叠：阈值 8 行、头 5 尾 2 + "…N more"）、结果、跟随底部滚动（用户上翻即停）。即目标 L1/L2 的可用基线。

### 2.2 `AgentSwarm` 委托（偏离形态）

`apps/mirri-web/src/components/chat/tool-calls/SwarmTool.vue`：

- **运行时自动展开**（:124-127 注释自述 "Running swarms start expanded so live progress is visible without a click"；`open = ref(status==='running' || inProgress>0)`）——即 §1 用户反馈的直接根因；
- 展开态内嵌：phase 总览条（N/M 大数字 + 分段条 + 图例）+ 成员手风琴（状态点/名字/型号/当前活动一行/阶段，行内再展开全量输出 `row.body`）；
- `openAgent` 事件已声明（:40）但模板未使用——右侧面板接线半成品；
- 落定/刷新后 `status !== 'running'`，默认收起——解释了刷新后观感反而"干净"。

### 2.3 优点对照（统一设计的输入）

| 能力 | 单 Agent 路径 | Swarm 路径 |
|---|---|---|
| 主区收起 + 面板路由 | ✅ | ❌（自动展开内嵌） |
| 深挖叙事（调用分组/折叠/跟随滚动） | ✅（面板内） | ❌（成员体为裸文本） |
| 入口守卫（无数据即隐藏） | ✅ | ❌ |
| 聚合一眼总览（N/M + 分布） | —（N=1 不需要） | ✅（但位置在展开态，应上移 head） |
| 成员当前活动一行 | ❌ | ✅ |
| 成员身份行（名/型号/阶段） | — | ✅ |
| 刷新后成员名单（来自结果解析） | ❌（面板空） | ✅（`buildSwarmCardRows` 合并 live 成员与解析结果） |

### 2.4 入口与面板接线现状

- `openAgent` 事件链已全线贯通：`ToolCall.vue` → `ToolGroup.vue` → `ChatPane.vue` → `App.vue` `openAgentPanel` → 右侧共享槽位；
- `useDetailPanel.ts` `resolveSubagentId`（:135-145）：按任务 id 或 `parentToolCallId` 解析，蜂群场景只会命中**第一个成员**——无成员选择器（N>1 缺口）；
- 底部任务坞（TasksPane）成员行可点入面板；
- 右侧槽位与文件预览/思考面板互斥共享（布局规则已符合 ADR-001）。

## 3. 数据链路现状

### 3.1 实时（live）

- rust 侧：子代理宿主把成员的 `turn.step.*` / `assistant.delta` / `tool.*` 帧发布到共享会话事件通道（`rust/crates/mirri-core/src/subagent/host.rs:783-791`），投影层逐帧标注 `agentId`（`session/event_projection.rs`，主代理为 `"main"`）；契约测试钉死该语义（`mirri-server/tests/e2e_mock_test.rs:534`）。
- web 侧：`apps/mirri-web/src/api/daemon/agentEventProjector.ts` 以 `MAIN_AGENT_TRANSCRIPT_FRAMES` 白名单 + `agentId !== 'main'` 判定（:43-62, :634-661）将成员帧从转录路径剥离，改投 `projectSubagentProgress` → `taskCreated`/`taskProgress` → 任务卡片/坞/面板。**转录零污染的架构已就位**（本域要解决的是卡片层的呈现，不是事件层）。
- **过程信号为合成字符串**：`subagentProgressText`（:224-249）把成员事件转成 "Calling Read: path" 类文本行，`AgentDetailPanel.groupProgress`（:39-57）再按 `Calling ` 前缀**启发式解析**重建结构——即架构 §4.1 指出的 N6 违例现状（双向文本化，有损且脆弱）。

### 3.2 刷新/回看（snapshot）

- `message.list` / `session.snapshot` 只读**主代理**的上下文历史（`rust/crates/mirri-server/src/rpc.rs:2040`，`resolve_session_agent(session, None)`）——主转录含 AgentSwarm 工具卡（输入 + 最终聚合结果），成员过程不进主历史；
- 成员数据**已完整落盘**（事件溯源是既成事实）：会话目录下 `agents/<agentId>/wire.jsonl` 独立记录每个成员的全过程。`session-6778695f` 实证：两个成员 journal 各含 13/10 组 `tool.call`+`tool.result`、41/65 个 `content.part`、8 个 `think.part`；
- 但**读面缺失**：快照 `subagents` 名册服务端硬编码空数组、web 端也硬编码 `[]`（conversation-rendering 域差距矩阵 G3/G10，其 context 有 file:line 锚点）；web 侧 `keepLiveSubagents`/`mergeSnapshotSubagents`（`lib/taskMerge.ts`）已为名册合并预留逻辑，名册来即活；
- 结果侧部分恢复：Swarm 卡片刷新后成员名单/终态来自解析 `<agent_swarm_result>` 文本（`lib/parseSwarmResult.ts`）——又是文本解析通道（N6 违例）。

### 3.3 面板叙事区渲染现状（2026-10-18 快照，ADR-009 的动机证据）

- 成员聚焦视图（`apps/mirri-web/src/components/chat/MemberFocus.vue`）全部为纯文本插值渲染：任务原文、流式输出、工具进度行（`Calling …` 合成文本）、结果摘要无富文本/卡片结构，与主会话的内容块渲染组件族（富文本、推理块、工具卡）零复用。
- 回看侧存在文本压平归一层（`lib/memberHistory.ts`）：成员 journal 读面（`message.list` + `agentId` 参数，与主会话消息同构）被压成 `kind + text` 扁平事件，工具名/参数/结果/成败状态（`isError`）丢失，且调用行格式与实时路不一致。
- 事件与读面的结构已足以支撑富渲染（实时帧携带完整工具事件字段、journal 读面与主会话消息同构），缺口在呈现层消费方式，不在引擎契约。

## 4. 能力三轴现状

| 轴 | 现状 | 缺口 |
|---|---|---|
| 观测（实时） | 事件流、agentId 路由、单成员面板深挖、蜂群行内总览（位置错误） | 成员选择器、四分类、diff 链接、稳定色、虚拟滚动、结构化事件（现为字符串） |
| 观测（回看） | 主转录的工具卡 + 结果；成员 journal 落盘完整 | 名册与过程读面（G3/G10）、面板刷新后为空 |
| 可控 | 停止（TaskStop，任务坞/任务视图工具可达）、审批门、蜂群启动前审批（collaboration US8.3）、swarm 成员经 TaskManager 注册可被 `task.cancel` 单独停止（引擎已接线，验收 e2e 覆盖） | 暂停/恢复（引擎无能力）；运行中定向指令 |
| 干预 | 事后续作（`resume(agent_id)`，引擎已具备）、BTW 侧聊（SideChatPanel，UI 先例） | 运行中定向指令（in-flight steering，引擎无能力）、面板指令落位 |

协作模式伏笔：wire 协议已预留 `collaboration_mode` 字段（`apps/mirri-web/src/api/daemon/wire.ts:248` 一带；`agent.status.updated` 透传 `collaborationMode` 开关，`agentEventProjector.ts:966-968`）；rust 侧 7 个 `Collaboration*` 工具注册但默认隐藏（collaboration SDR-003）。S7 升格的协议前提已铺。

## 5. 与相邻域差距矩阵的衔接

- conversation-rendering 域：G3（快照名册恒空）、G10（身份分片记录流/子代理对话零持久化读面）——本域 §3.2/架构 §4.2 的数据侧落点；
- subagent 域：US7.2（生命周期事件界面呈现）、E12（后台任务面板、US12.5 web 对等呈现）——本域是其呈现侧的分层化重构；该域 context 差距矩阵 G1–G36 中的呈现类条目以本域为承接面；
- collaboration 域：E5（团队观测）、P-1（结构化回流）、P-2（成员阶段汇报）——与架构 §4.1 同通道；E8（蜂群基线）的界面呈现以 SDR-002 为上位约束。

## 6. 外部产品调研结论摘要（2026-09-14 收到的研究报告）

调研对象：Devin（左右分栏双心智）、Manus（渐进披露/任务指派心智）、Swarm-IDE（流式拓扑/扁平介入/LLM history 消黑箱）、Hermes Studio（多 Agent Crew）、AgentHUD、ClawProwl。

分拣结论（裁决见 SDR/ADR，此处只列事实性要点）：

- 双心智分层、渐进披露、透明建信任、可中断性四条原则与本域根需求一致，吸收为设计原则；
- Layer 2（聚焦层）内容设计（调用卡片化、diff 入口、事件四分类、干预输入位）被 S2/S3/§3 吸收；
- 全景仪表盘形态（进度百分比 + ETA + 成员卡墙常驻）被 SDR-003/004 分期化改造（百分比/ETA 拒绝；卡墙归协作模式升格）；
- 拓扑图/时间线泳道归 apps/vis（SDR-005）；
- 暂停/恢复与扁平介入为引擎侧前置项（SDR-006）；
- 该报告的技术选型建议（React Flow/D3/Canvas/Zustand 等）基于 React 生态假设，与本仓库 Vue 3 + composable 架构不适用；其"事件溯源"数据模型建议与引擎既成事实一致（每 agent 独立 journal）。

## 7. 待核实项

- [x] 审批请求事件是否携带发起成员标识（wire 契约层面）——已核实并落地：引擎审批请求携带发起成员身份，web 审批卡渲染 `sub agent · <name>` 徽章（P2-T3 实现，browser e2e 验证主代理审批无徽章 / 成员审批有徽章）
- [ ] `agent.status.updated` 是否对成员帧逐成员发出（影响 L0 活动行的数据源选择：事件 vs 任务状态轮询）
- [ ] 终端端（TUI）委托呈现现状基线（N5 端点对等需要该基线，属 subagent 域 US12.5 的侦察范围）

## 8. 已知问题（2026-09-15 真实使用验收发现）

- **成员工具事件泄漏到主会话转录（违反 SDR-002，P0）**：swarm 运行期间，成员的实时工具调用（写文件、跑命令）作为工具行渲染进了主会话消息区；刷新后消失（主 journal 干净，重渲染只读主 journal——与域起源反馈"实时出现、刷新消失"同型，是修掉 Swarm 监控台通道后的残留通道）。实证会话：`wd_tx1` 下 `session-5dc984e0`（主 journal 仅 SetSessionTitle / AgentSwarm / 总结；泄漏的 5 行工具调用分属两个成员 journal）。定性：引擎数据面正确、右侧面板正确；web 转录构建层（实时事件 → 消息列表）未按发起成员过滤工具事件。验收盲区：Phase 1/2 清单只断言了卡片与面板的正向形态，缺"主转录不出现成员工具行"的否定断言。修复：引擎 tool.call.started / tool.result / tool.progress 帧携带 agentId，web 既有按 agent 路由生效（成员事件只进成员行）；in-flight shadow 同步过滤到主代理；e2e 补否定断言。
- **（关联）主会话区委托块粒度**：当前整批收敛为一张一行卡片，批次内各成员的状态在对话区不可见（须点开面板）；用户裁决改为按实际启动成员数显示收起成员行。已落地（P2+2，见 SDR-007 与 ROADMAP 增补批次）。

## 9. 成员状态信号失真（2026-09-16 快照：rust 核心 + web 蜂群会话）

真实使用暴露"成员仍在工作/等待审批，界面显示失败"的复合缺陷（需求动机见 SDR-008；引擎侧设计承接见 subagent 域 ADR-011）：

- **等待审批无状态信号**：引擎侧成员挂起语义只有限流退避一种（`rust/crates/mirri-core/src/subagent/swarm_batch.rs:56` 的 `RATE_LIMIT_SUSPENDED_REASON`；`subagent/mod.rs:627` 注释自述仅限流语境）；审批等待发生在 turn 执行前的权限钩子内（`rust/crates/mirri-core/src/turn_flow/hooks.rs:929-986` 的反向 RPC/gate 竞速），期间不发任何生命周期事件。web 侧成员阶段词汇与投影（`apps/mirri-web/src/api/daemon/agentEventProjector.ts:1131-1139` 的 `subagent.suspended` → suspended）均只有限流语义；`approvalsBySession` 虽已携带成员身份（supervision B1 落地），但成员行/卡片/选择区均不做关联渲染——等待审批的成员对外恒呈"运行中"。
- **等待时间计入时长预算**：成员 attempt 的时长闸门为挂钟计时（`swarm_batch.rs:577-591` `wrap_attempt`；缺省 2 小时，`mirri-core/src/config/mod.rs:744` `resolve_subagent_timeout`），审批等待期间不停表——长审批必然演变为 "Subagent timed out" 假失败（`registry.tasks.fail` → web `taskCompleted(status:'failed')` → 成员行显示失败，而用户视角是"还在工作/等我审批"）。
- **状态双行分叉**：rust 引擎把蜂群成员注册进任务管理面（`swarm_batch.rs:135-139` `SwarmTaskRegistry`，US5.1——"task.list 可观测"），REST 任务行以任务 id 为键；web 实时行以 agent id 为键，fold 依赖 `backgroundTaskId`（`apps/mirri-web/src/lib/taskMerge.ts:8-20`），而前台蜂群成员无该字段——该处注释"REST /tasks 永不返回前台 swarm 成员"是 TS 引擎时代假设，rust 引擎下已过时。`phaseForTask`（`apps/mirri-web/src/composables/swarmGroups.ts:27-35`）令 REST 行的 failed/cancelled 权威覆盖 `subagentPhase` 且终态粘滞、cancelled 混入 failed 档。
- **审批通道与连接耦合**：WS 断开时 pending 反向 RPC 被清空（`rust/crates/mirri-server/src/ws_rpc.rs:1819`），审批请求按"用户已取消"收尾（`hooks.rs:1029-1064`：投递失败 → gate withdraw + `Timeout` → "cancelled by user" 否决工具调用）——用户从未表态，审批卡随断连消失（subagent 域 ADR-009 修订记录登记的后续项 Q3）。
