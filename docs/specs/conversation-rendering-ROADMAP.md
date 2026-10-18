# 对话渲染 — ROADMAP

> 本文只做**状态与批次管理**，不承载需求或设计内容（那些见 [conversation-rendering-requirements.md](conversation-rendering-requirements.md) / [conversation-rendering-architecture.md](conversation-rendering-architecture.md)）；现状证据见 [conversation-rendering-context.md](conversation-rendering-context.md)（快照 2026-09-13）。
> 跨域裁决纪律（同治理约定）：任何待裁决项，先写清背景、痛点、选项与优劣，再交用户裁决；不允许裸问。

## 覆盖状态

| 能力块（需求锚点） | 文档化 | 现状（详见 context §4.2） |
|---|---|---|
| 快照原子性 + 水位续接（US1.1/US4.1） | ✅ 本批 | TS 完整；Rust 快照骨架在、六要素缺三 |
| 消息身份与时间（US1.2） | ✅ 本批 | TS 推导供给；Rust 缺失（G5） |
| 历史分页（US1.3） | ✅ 本批 | TS REST 完整（含已知游标陷阱）；Rust 无 REST 面、无 role（G6） |
| 流式契约 + 工具生命周期（US2.1/2.2） | ✅ 本批 | TS 完整；Rust 工具结果事件无输出（G4） |
| 回合边界与结局（US2.3） | ✅ 本批 | TS 完整；Rust 事件缺 agentId（G8） |
| 步级用量（US2.4） | ✅ 本批 | 双方已落（G11） |
| 状态聚合（US2.5） | ✅ 本批 | TS 结构化；Rust 调试串占位（G7） |
| 在途回合续看（US3） | ✅ 本批 | TS 完整；Rust 恒 null（G1/G2/G3） |
| 断线重连一致性（US4） | ✅ 本批 | TS 完整；Rust 游标重放链在、resync 语义待核 |
| 转录保真（US5） | ✅ 本批 | TS 完整；Rust 撤销不持久（G9）、子代理不落盘（G10，跨域） |

## 批次（缺陷修复排期，按用户影响排序）

| 批次 | 内容 | 缺口锚点 | 状态 |
|---|---|---|---|
| CR1 工具结果输出 | 引擎事件补输出字段 + 投影输出 + web 消费（web 原已消费 `output` 字段，零改动） | G4 | ✅ 2026-09-12（`ToolOutput` untagged 序列化对齐 TS 线形状；`tool.result` 事件带输出） |
| CR2 在途回合捕获 | `SessionArtifacts` 挂 `InFlightTurnShadow`（易失影子状态聚合）+ 快照 `in_flight_turn`/`pending_approvals`/`pending_questions` 填充 + web `toAppSnapshot` 映射与 `turnId` 类型放宽 | G1/G2 | ✅ 2026-09-12（待交互行暂无 `expires_at`——引擎无对应物，web 容缺省） |
| CR3 消息身份与读面统一 | `msg-<index>` → `msg-<entry_seq>`（记录序号锚定，压缩/撤销/重启稳定）；补 `createdAt`（推导保序）；快照/分页统一走共享行构造 `message_list_row`；游标失效返回空终止页（优于 TS 回锅行为的有意改进） | G5 | ✅ 2026-09-12（铸造式持久身份仍为提案 P3） |
| CR4 状态聚合结构化 | `agent.status.updated` 真负载（阶段/上下文水位/用量切片）+ 影子升级为常驻聚合服务 | G7 | ✅ 2026-09-18（`feat/cr4-cr5-agent-status-attribution` 3a26d83d..8a836a77：8 相位 phase + contextTokens/maxContextTokens/contextUsage 水位 + sessionUsage 切片真负载，生命周期门控发布（Disposed 抑制）；影子维持 turn 执行器内累计 + ActivityTracker 发布，常驻聚合服务不升格——裁决见下方开放项 1） |
| CR5 事件归因补齐 | turn/tool/compaction/background 事件补 agentId；成败对称 | G8 | ✅ 2026-09-18（`feat/cr4-cr5-agent-status-attribution` 3a26d83d..8a836a77：`turn.*`/`tool.call.*`/`compaction.*`/`background.task.*`（`info.agentId`）带 agentId 归因，`TurnCompleted`/`TurnFailed` 成败对称；protocol schema / node-sdk 翻译 / web 投影路由同批接线） |
| CR6 撤销持久 | `session.undo` 写 `context.undo` op + 恢复重放（恢复语义依赖持久化域） | G9 | ⬜ 待排（跨域：会话管理域 Epic U） |
| CR7 快照 title | 快照会话视图带名称 | G12 | ✅ 2026-09-12（随 CR2 一并落地） |
| CR8 身份分片记录流 | 子代理/btw 记录流落盘（持久化域 ADR-006 终态） | G10 | ⬜ 待排（跨域：持久化域 ADR-006 / 子代理域） |
| CR9 历史读面 REST 对齐 | REST GET messages + role 过滤 | G6 | ⬜ 待排（游标空终止已随 CR3 落地） |
| CR10 全量转录读面 | 压缩后从日志重建全量转录（当前读面读内存 history，压缩后旧对话消失） | G14 | ⬜ 待排（需从 wire 日志流式折叠的读路径，对齐 TS messageLegacy） |
| CR11 易失流偏移对齐 | `assistant/thinking.delta` 事件带回合内累计偏移（快照播种后续流的重叠/空洞检测） | （CR2 已知窗口） | ⬜ 待排（引擎侧需逐回合累计状态） |
| CR12 渲染契约接线 | 服务端消息行投影到 TS 协议 content part 形状（tool_use/tool_result/thinking/image/file，G15/G16，磁盘拼写不动）；前端快照路径全量映射（G17）；三层测试保障：rpc 行构造单测 + wire 重放等价不变量 + zod 契约 e2e（G18）；附带 R3 游标告警条件收紧；附带 G19 think/text 记录顺序对齐流式到达序（浏览器验收发现） | G15–G19 | ✅ 2026-09-13 批次已落地（含浏览器实际重渲染验收：旧会话副本 + 新会话 live/刷新，工具卡片/思考块/顺序/无泄漏全过） |
| CR13 插话回合内折叠渲染 | 来源标识新增"用户·插话"变体（准入侧打标，SDR-001 / 架构 ADR-010，机制侧见回合调度域 ADR-003）；消费端折叠规则（插话消息不切回合、完整气泡 + 角标）+ 乐观回显同标识；协议契约与本地类型对齐 | US2.3 细化 | ⬜ 待排 |

依赖：CR1–CR3 相互独立可并行（本批已全部落地）；CR9 的游标语义依赖 CR3（已落地）；CR8 依赖持久化域 ADR-006 批次；CR12 内契约测试（重放等价/契约 e2e）依赖消息行投影形状落地，形状以 TS 协议 schema 为最终裁判（Task 2 可并行）。

### 已裁决后续项（2026-09-13 登记，本批不实施）

| 后续项 | 要点 | 登记处 |
|---|---|---|
| 快照 session view 补 usage | session 视图无 usage 四桶，靠旁路恢复 | context.md §4.2 已裁决后续项 |
| subagents 名册 | 快照恒空（G3 延伸），跨域 | 同上 |
| 消息真实时间戳 | createdAt 为推导值（G5 延伸） | 同上 |
| 气泡合并辅助字段 | 消息行无 promptId/status/durationMs | 同上 |
| turn_id 类型统一 | approval 行 string/number 既知偏差，契约测试豁免（ADR-009） | 同上；架构 ADR-009 |

## 开放项（待裁决，均含背景/选项）

### 开放项 1：影子状态的宿主位置（CR2 前置裁决——本批以最小实现先行）

**背景**：在途回合捕获需要一处"当前回合易失影子状态"（已流出文本/思考累计、运行中工具最近进度）。可选宿主：turn 执行器内部（每次 turn 新建）、会话级聚合服务（跨 turn 存活、对齐 TS activityView 形态）、读时即时扫描事件缓冲。

**痛点**：turn 内部实现最直接但快照读取需要穿过执行层；会话级服务最贴近 TS 参照但要处理与 turn 生命周期的同步；读时扫描零状态但易失事件不落缓冲、扫不到。

**本批处置**：按"turn 执行器内累计 + 快照读取"最小闭环实施（快照读取路径已可达 turn 状态）；升级为常驻聚合服务（对齐 TS activityView，含 agent.activity 事件）挂 CR4 一并裁决。

**裁决（2026-09-18，CR4/CR5 批次 `feat/cr4-cr5-agent-status-attribution` 落地时）**：维持"turn 执行器内累计 + ActivityTracker 发布"方案——影子状态不升格为常驻聚合服务（对齐 TS activityView 的形态收益不抵新增同步面，CR4 最小闭环决策）；`agent.status.updated` 真负载经 ActivityTracker 发布点接线。裁决载体：`.superpowers/sdd/2026-09-16-cr4-cr5-agent-status-attribution/`（progress.md 台账）。

### 开放项 2：消息身份的推导 vs 铸造（CR3 的终态方向）

**背景**：架构 ADR-004 已裁决"数据状态内稳定"为底线；推导身份（记录序号锚定）满足底线，铸造身份（记录落定时分配、永不漂移）免除对表但改写侧契约。

**选项**：A. 推导身份（记录序号锚定，undo 后随迁 + 重新对表兜底——TS 同构）；B. 铸造身份（写侧持久化消息 id，跨 undo 稳定——提案 P3 的落地）；C. 先 A 后 B（A 落地解除 G5，B 另立批次）。

**本批处置**：C 的 A 段已实施（`msg-<n>` 锚定到记录序号 entry_seq——压缩/撤销不回收编号、重放确定性重建，重启/undo 后随数据状态稳定；游标失效返回空终止页）；B 段挂提案 P3 待裁决。

## 与其他域的登记关系

- 持久化域 ADR-006（身份分片记录流）← CR8 的终态依据（本域不重复设计）。
- 会话管理域 Epic U（撤销持久语义）← CR6 的语义依据。
- `rust-core-design/ROADMAP.md` P7（事件投影悬置项）← CR1/CR4/CR5 的既有登记处（本域批次的域视角汇总，不重复展开）。
- `plan/rust-v2-method-parity.md` ← 方法 parity 唯一事实源。
