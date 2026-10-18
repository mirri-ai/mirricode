# 多 Agent 督导域 — ROADMAP

> 本文件是督导域执行状态与分期计划的唯一维护点（计划类文档，按 specs 治理约定自由更新）。需求终态见 [agent-supervision-requirements.md](agent-supervision-requirements.md)（下称 S1–S7 为其 Epic 编号）；现状差距见 [agent-supervision-context.md](agent-supervision-context.md)。
> 分期原则：**呈现层先行、引擎契约随后、协作升格殿后**——每一期独立可交付、可验收；前一期的任何组件不在后一期被重写（留缝不重写）。

## Phase 1 — 呈现统一（纯 web，零引擎改动）

> 交付 SDR-001/002/003 与 S1–S3 全部 P0/P1 条目。这一期是后续所有阶段的地基：卡片泛化于 N、面板泛化于成员集合、成员视图单一组件三处复用。

| 条目 | 内容 | 验收锚点 |
|---|---|---|
| P1-1 委托卡片统一 | `DelegationCard` 单一组件落地（替换 Agent/Swarm 两套卡片）：默认收起、head 聚合（N/M + 微型分布 + 当前活动行）、Open 按钮 + 显隐守卫、异常呼吸边框（reduced-motion 静态替代）、展开态轻量摘要（任务/型号/成员名单） | US1.1–1.5 |
| P1-2 面板泛化 | `SupervisionPanel`：成员选择器（N>1）+ `MemberFocus` 提取（从现有单成员面板抽出为共享组件）；`resolveSubagentId` 泛化为按 `parentToolCallId` 的成员集合解析 | US2.1–2.3、US2.8 |
| P1-3 成员视图增强 | 稳定色（批次内哈希分配）、事件四分类视觉（消费现有合成行，按来源分类渲染）、修改类调用挂 diff 入口（复用既有 diff 面板接线）、跟随滚动保持 | US2.4–2.6 |
| P1-4 叙事与性能 | 调用叙事折叠规则统一（阈值/头尾行）、长输出虚拟滚动 | US3.1、US2.7 |
| P1-7 叙事区内容块复用（ADR-009） | 面板叙事区复用主会话内容块组件族：实时路投影层维护结构化工具步骤、回看路消费 journal 读面归一为内容块（退役文本压平层）；Task/Result 富文本渲染 | ADR-009、US2.5、US3.1 |
| P1-5 操作区预留 | `MemberFocus` 操作区结构落位（停止入口接既有 TaskStop；暂停/指令落位不渲染控件） | US5.1、US6.2 |
| P1-6 移动端 | 卡片/面板在窄屏的降级形态 | N4/N5 |

依赖：无（消费现状已有数据源：live 事件流 + 任务 store + 结果解析）。

明确不在本期：刷新后过程恢复（数据读面未就绪，本期保持"入口按有无数据隐藏"的现状语义——US1.3 判定函数本期即落地，数据源 Phase 2 接入）；结构化事件（本期四分类对现有合成行按来源标注，语义正确性受限于现状通道）。

## Phase 2 — 引擎契约（观测完整 + 控制干预的语义裁决）

> 交付 S4.1 与 §4 契约的引擎侧落地。实现归属：subagent / collaboration / rust-core-design 域；本域持有验收。跨域提案先裁决（见 proposals 登记 P-025/P-026/P-027）。

| 条目 | 内容 | 归属域 | 验收锚点 |
|---|---|---|---|
| P2-1 结构化成员过程事件 | 四分类事件模型（thought/tool_call/tool_result/status/error），实时与回看同构；替代"Calling "字符串合成与文本解析（消费侧含 Swarm 结果结构化回流，合并 collaboration P-1/P-2） | rust-core / subagent | 架构 §4.1、N6 |
| P2-2 快照成员名册 + 过程读面 | 快照名册填实（消费 conversation-rendering G3 的落点）+ 成员 journal 读面；web 侧 `mergeSnapshotSubagents` 等预埋逻辑激活 | rust-core / conversation-rendering | US4.1 |
| P2-3 审批发起者身份 | 审批事件携带成员标识（先核实 context §7 待确认项） | subagent / approval | US5.3 |
| P2-4 暂停/恢复 | 成员级暂停信号、进展保留、恢复续接（提案裁决 → SDR → 实现） | subagent | US5.2 |
| P2-5 运行中定向指令 | in-flight steering 语义（与事后续作的关系、权限配合、生效时机）→ 面板指令落位激活 | subagent | US6.1 |
| P2-6 呈现层切换到结构化源 | web 投影层从合成行切换到事件通道；四分类/叙事/总览事件流统一消费 | 本域 | N6、US2.5 终态 |

依赖：P2-6 依赖 P2-1；US4.1 的完整验收依赖 P2-2；Phase 1 的组件零重写（数据源替换）。

## Phase 2 增补 — 验收后修订（2026-09-15）

> 来源：Phase 2 真实使用验收（tx1 会话）发现的两项——一项缺陷修复、一项用户裁决的呈现粒度修订（SDR-007）。

| 条目 | 内容 | 归属域 | 验收锚点 |
|---|---|---|---|
| P2+1 成员工具事件泄漏修复 | web 转录构建层按发起成员过滤实时工具事件：成员的 tool.call/tool.result 只进成员行（任务行 outputLines/聚焦视图），不得渲染进主会话转录；刷新前后观感一致（主 journal 本就不含成员调用）。补否定断言进 browser e2e（主转录不出现成员工具行），堵住验收盲区 | 本域 | SDR-002、US1.1 |
| P2+2 委托块成员行布局（SDR-007） | 委托卡片收起形态改版：批次头行 + 每实际启动成员一行（成员标识 · 状态 · 最新活动单行摘要），点行进右侧面板聚焦视图；未启动成员不占行；单成员委托无批次头聚合结构；同步更新既有 browser e2e 断言（卡片收起形态、单 Agent 零回归） | 本域 | SDR-007、US1.1、US1.2 |

依赖：P2+2 依赖 P2+1（成员行是成员事件的合法落点，先堵泄漏再改布局，避免成员行与泄漏行双轨并存）。

## Phase 3 — 协作升格（依赖 collaboration 域骨架）

> 交付 S7 与 S4.2 定位项。前置：collaboration E1–E3（团队组建/任务板/通信）+ Phase 2 的事件通道。

| 条目 | 内容 | 验收锚点 |
|---|---|---|
| P3-1 Agent 总览 | 任务坞升格 `AgentOverview`：成员卡墙（L0 卡 + 控制入口）+ 摘要级事件流（虚拟滚动、按成员/类型筛选） | US7.1、US7.2 |
| P3-2 干预常驻 | 总览成员卡与面板操作区激活定向指令/暂停入口（依赖 P2-4/5） | US7.3、US6.1 |
| P3-3 深度观测归位 | apps/vis 的多 agent 回放强化（拓扑/泳道，消费 §4.1 事件模型与成员 journal）；产品内关系摘要形态 | US4.2 |

## Phase 4 — 等待状态机（成员挂起原语与等待审批呈现，2026-09-16 立项）

> 交付 SDR-008 / US5.4 与架构 ADR-008。引擎侧语义裁决与实现归 subagent 域（其 ADR-011——成员挂起原语与等待用户状态）；本域持有呈现验收。现状证据见 context §9。文件级实施计划见 `plan/swarm-member-wait-state.md`（其 P1–P4 与本表 P4-1~P4-4 一一对应）。

| 条目 | 内容 | 归属域 | 验收锚点 |
|---|---|---|---|
| P4-1 呈现过渡（纯 web） | 成员行/卡片/选择区关联待决审批渲染「等待审批中」；聚合状态 attention 档；cancelled 与 failed 分档；任务行 fold 键归一 agentId——推导式实现**显式标注待删**（ADR-008 ④） | 本域 | US5.4（事件源条款除外）、US1.2 |
| P4-2 引擎挂起原语 | 结构化挂起原因（approval / question / rate_limit）、挂起/恢复事件配对、时长预算等待豁免、并发槽让渡 | subagent（ADR-011） | 架构 §4.6、subagent ADR-011 |
| P4-3 审批连接无关性 | 审批 gate 为待决事实源、通知面与连接解耦、重连恢复待决审批（关闭 subagent ADR-009 修订记录的 Q3 后续项） | subagent / approval | US5.4 刷新恢复条款、ADR-011 e |
| P4-4 呈现切换到事件源 | web 投影消费结构化挂起事件、删除 P4-1 的过渡推导、TUI 成员状态词汇对齐 | 本域 | US5.4 全量、N1/N6 |

依赖：P4-1 独立可先行（零引擎改动）；P4-2/P4-3 为引擎侧（P4-3 依赖 P4-2 的挂起原语与 gate 挂钩）；P4-4 依赖 P4-2/P4-3 落地后切换。

## 状态台账

| Phase | 状态 |
|---|---|
| Phase 1 | 已完成（2026-09-15，代码 dc526f72 起 5 提交；真实浏览器验收并入 Phase 2 收尾统一执行） |
| Phase 2 | 已完成（2026-09-15，代码 083ea322 起 6 提交 + 引擎成员停止补齐 + browser e2e 验收；P-025/026/027 deferred 待裁决） |
| Phase 2 增补 | 已完成（2026-09-15，P2+1 引擎工具事件归属修复 + P2+2 成员行布局，browser e2e 含泄漏否定断言） |
| Phase 3 | 未开始（外部依赖：collaboration 骨架） |
| Phase 4 | 已完成（2026-09-16，代码 1d2ad7d1 起 8 提交 + 1 修复提交，主分支 merge 至 0c5f59c0；Task 1–7 全部通过任务审查；兼容窗事实：web 投影双形态 reason 判别、node-sdk translate 透传（484632e5）、TUI JSON-string 解析；遗留 minor 见 SDD ledger `.superpowers/sdd/swarm-member-wait-state/progress.md`，最终全分支审查裁决） |

## 跨域登记指针

- proposals.md：P-025（成员级暂停/恢复）、P-026（运行中定向指令）、P-027（结构化成员过程事件通道，合并 collaboration P-1/P-2 语义）
- conversation-rendering 域：G3/G10 消费加压（P2-2）
- subagent 域：US7.2/E12 的呈现侧承接（Phase 1）；暂停/steering 的语义裁决（Phase 2）；ADR-011 成员挂起原语（Phase 4 引擎侧承接——其通用挂起原语亦是 P-025 用户级暂停/恢复若裁决通过时的复用底座）
- subagent 域（ADR-009 修订记录 Q3）：断线后 pending 审批恢复语义——由 Phase 4 的 P4-3 关闭
- glossary：SV1（委托）、SV2（督导）——随本域 v1 同批注册
