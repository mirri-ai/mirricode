# 协作域 ROADMAP

> 状态/开放项唯一维护点（按 [AGENTS_CN.md](AGENTS_CN.md) 治理：本文件只做状态与开放项管理，不承载需求/设计内容）。快照：2026-09-12。
> 配套：[collaboration-requirements.md](collaboration-requirements.md)（E1–E7 设计目标 + E8 TS 支持基线）· [collaboration-architecture.md](collaboration-architecture.md)（A 现状机制 / B 团队模型提案骨架）· [collaboration-context.md](collaboration-context.md)（现状代码事实）。

## 1. 文档四件套状态

| 文档 | 状态 |
|---|---|
| collaboration-requirements.md | 已有（E1–E7 + SDR-001~003）；2026-09-12 追加 E8（TS 支持基线）、§13 未来增强提案、SDR-004 |
| collaboration-architecture.md | 2026-09-12 新建（A 现状机制 / B 团队模型提案骨架，ADR-001） |
| collaboration-context.md | 已有（§1–§4 原文保留）；2026-09-12 增量补录 §5–§8（引擎细节/消费端/校对矩阵/v1 小结） |
| collaboration-ROADMAP.md | 本文（2026-09-12 新建） |

## 2. 能力覆盖矩阵

| 能力 | requirements 条目 | 代码现状（context） | 状态 |
|---|---|---|---|
| 蜂群模式开启/关闭/幂等/指引/收回 | US8.1 | §5（TS v2 已支持） | 已文档化 |
| 单任务蜂群命令（含"蜂群已结束"标记） | US8.2 | §6.1（web 已对齐：`/swarm <task>` 统一确认 + 「已结束」divider 由 web 推导，实时转录增强；TUI 仍为 task 触发源自动退出——端点差异保留披露） | 已落地（B10，web） |
| 手动权限下蜂群启动确认 | US8.3 | §6.1（web 四条开启路径——Mode 菜单/裸 `/swarm`/`on`/`<task>`——均经 `enableSwarmMode` 确认；TUI 全部开启路径均确认） | 已落地（B10，web 对齐 TUI） |
| 模式可见性（pill/徽标/面板/移动端） | US8.4 | §6 | 已文档化 |
| 蜂群模式跨重启恢复 | US8.5 | §5（wire Op journal 回放） | 已文档化 |
| 蜂群批次独占 | US8.6 | §5（批次 veto） | 已文档化 |
| 模式偏好按会话独立记忆 | US8.7 | §6（仅 web localStorage；TUI 无偏好持久化——见 O8） | 已文档化（端点差异已披露） |
| 团队组建 E1 / 共享任务板 E2 / 角色通信 E3 / 仲裁 E4 / 团队观测 E5 / 团队持久化 E6 | E1.x–E6.x | 无实现（无 peer 通信/共享任务板/角色协作语义） | 设计增强，待 Q1–Q5 裁决后实施 |
| 模型统一 E7（subagent 定位 / swarm 归宿） | E7.1/E7.2 | 待裁决 | 裁决性 US |
| 协作工具模式门控 | SDR-003 | Rust 侧已落地（context §2）；**TS 侧无对应实现** | 见 O5 |
| 蜂群结果结构化回流 / 成员阶段汇报 / 终端协作入口 / 模式字段一致性 | §13 P-1~P-4 | 无实现 | 提案（免现状对齐检查） |

## 3. 开放项

| # | 项 | 说明 | 需谁决策 |
|---|---|---|---|
| O1 | 团队模型裁决 Q1–Q5 | requirements §11：任务驱动模型 / 消息上下文成本 / 团队-会话关系 / mirri-collaboration 协议骨架处置 / lead 必要性。**范围注（2026-09-12，DEC-5/ADR-002）**：19 个团队模型 US 的产品评审完成前不在本程序实施，评审通过后另立项 | 用户 |
| O2 | 嵌套子代理 Rust 差距 | TS v2 子代理可再派子代理（coder 与主 agent profile 工具表含 Agent/AgentSwarm，context §5）；Rust `SessionSubagentHost` 子 agent 无嵌套（context §2）。**落地中（本程序 B4，缺口 D-46；subagent 域 Q2 口径已勘误）** | Rust 团队 |
| O3 | 权限默认放行的跨域核对 | Agent/AgentSwarm/TaskList/TaskOutput 在默认策略自动放行（context §5）——与子代理派发域/工具调用域的权限条目交叉，需确认对应域已覆盖、不重复登记 | 发起方 |
| O4 | ~~web collaboration 开关在 v2 后端无引擎效果~~ | 连 v2 时 `collaboration_mode` 字段被引擎 schema 剥离，开关纯客户端 UI 状态（context §6）。**已裁决（ADR-002，2026-09-12）**：按后端能力诚实呈现——v2 隐藏/禁用、Rust 侧保留。**已落地（2026-09-13，B10）**：`lib/backendCapabilities.ts`（仅 v3）+ `collaborationSupported` 透传，Composer 菜单/pill、移动端设置、/status 面板全隐藏；setter 与回读路径（`sessionUsageUpdated`/`refreshSessionStatus`）同 gate（context §6.1） | ~~用户/消费端~~ 已裁决并落地 |
| O5 | mirri-collaboration crate 处置 | ~~rust-core-design N2「补完或冻结」二选一~~ **已裁决（ADR-002，2026-09-12）**：冻结为实验 Draft——保留协议骨架与 SDR-003 模式门控，不在本程序补完多 agent 调度；协作域 19 US 产品评审后另立项（届时回收骨架）；同时覆盖 SDR-003 的 TS 侧缺口登记（TS 引擎无协作工具组，维持现状缺口记录） | ~~Rust 团队/用户~~ 已裁决 |
| O6 | ~~glossary 注册（本域禁改共享文件，列此待发起方同批合并）~~ | **已注册（glossary v8，2026-09-12）**：「蜂群模式（swarm 模式）」= CO1、「协作模式（collaboration 模式）」= CO2，均登记并指向本域 spec；本条保留作注册台账 | 发起方 |
| O7 | ~~既有文档勘误修订~~ | ~~context §7-C 登记的三处（§1 swarm wire 表述 / §3 v1 退役表述 / requirements 头部配套文档注记）~~ **已处理（2026-09-12，B0）**：三处勘误落地——§1/§3 就地标注勘误（删除线保留原文）、requirements 头部修正为现行链接；§7-C 表保留作勘误台账 | ~~发起方~~ 已处理 |
| O8 | TUI 模式偏好持久化缺失 | TUI 无跨会话蜂群偏好记忆（模式随会话状态回读，无"无会话暂存/首会话转移"）。**已裁决推迟（2026-09-12，DEC-2：TUI-only 项 D-38，无法 rust+web 验收，随 Rust 核心切换整体重估；与提案 P-3 一并处理）** | ~~用户~~ 已裁决推迟 |
| O9 | ~~v3 `message.send` 丢弃 per-prompt 模式字段~~ | web v3 通道下单次请求的模式字段被丢弃、只能走独立模式 RPC（context §6）——与 §13 P-4 提案关联。**已落地（2026-09-13，B10，D-26）**：按批次裁决 4「显式化」——`ws-rpc-api.ts` submitPrompt 以 `satisfies` 钉住列表显式 `void` 7 个 per-prompt 模式字段（可见可查、字段消失即编译失败），模式切换唯一通道为 `updateSession` 路由的 swarm.enter/exit、collaboration.enter/exit 等独立 RPC；v1/v2 REST 通道不受影响（context §6.1）。P-4（per-prompt 模式字段 wire 一致性提案）仍留提案区 | ~~用户/消费端~~ 已落地 |
| O10 | ~~web 蜂群开启路径不一致~~ | ① web 斜杠 `on`/`<task>` 变体在 manual 权限下无确认（裸 `/swarm` 与模式菜单有确认）；② web 模式设置为 fire-and-forget，失败仅通用告警、开关乐观翻转不回退；③ web `/swarm <任务>` 进的是手动蜂群（无 task 触发源），回合结束不自动退出、无"已结束"标记（context §6）。**已落地（2026-09-13，B10，D-24/D-25/D-26）**：① 四条开启路径统一经 `enableSwarmMode` 确认；② 乐观翻转 + 失败回退（map/storage）+ 专用失败标题；③ `<task>` = 手动蜂群 + 首条指令，「已结束」divider 由 web 推导（主回合结束 + 无 running 子代理，挂 turn-end 与子代理任务生命周期双触发），开关不自动翻转（context §6.1） | ~~用户/消费端~~ 已落地 |
| O11 | ~~跨域嵌套现状口径矛盾~~ | ~~subagent-requirements.md Q2 将"单层派发"标注为现状（实为 Rust 现状）~~ **已解除（2026-09-12，B0）**：subagent 域 Q2 口径已勘误（TS 参照基线可嵌套、单层为 Rust 差距，见 subagent-requirements §11 勘误注与 context G7）；本条不再阻塞 O2 裁决 | ~~发起方/子代理域~~ 已解除 |

## 4. 引用（只引用，不重复）

- Rust 侧设计与迁移跟踪：[rust-core-design/](rust-core-design/)（ROADMAP 1.7「多 agent 团队协作域：需求已立待架构」、N2「collaboration 二选一：补完或冻结」、context.md 的 swarm/subagent 并入记录）；`plan/rust-tool-calling.md`（Rust 独有 7 个 Collaboration 工具的登记清单）、`plan/rust-v2-method-parity.md`（N2 条目）。
- 相邻域：子代理派发域（[subagent-ROADMAP.md](subagent-ROADMAP.md)，批量派发并发/续作/后台任务）、对话渲染域（[conversation-rendering-ROADMAP.md](conversation-rendering-ROADMAP.md)，蜂群卡片投影）、会话管理域（[session-management-ROADMAP.md](session-management-ROADMAP.md)，偏好归属与删除清理）。

## 5. 落地程序范围标注（baseline-gap-landing 程序 · 2026-09-12）

> 依据总计划 [`plan/baseline-gap-landing.md`](../../plan/baseline-gap-landing.md)（§一 DEC-2/DEC-5 范围裁决、§二 批次定义、§四 进度跟踪）。本节只做状态与范围管理。

**已裁决推迟（本程序排除，DEC-2）**：

- TUI 协作模式入口（提案 P-3）与 TUI 模式偏好持久化（D-38）——TUI/ACP-only 项无法以 rust+web 验收，且与「减少 TS 改动」冲突；TUI 将随 Rust 核心切换整体重估（O8 同步标注）。
- 团队模型实施（E1–E7，19 US）——DEC-5/ADR-002：mirri-collaboration 冻结为实验 Draft，待产品评审后另立项（O1/O5 同步标注）。

**落地中（本程序进行中的缺口项与批次）**：

| 批次 | 缺口项 |
|---|---|
| B4 子代理深度与独立记录（子代理域批次，本条为协作域侧登记） | D-46（嵌套子代理——O2 的落地项） |

**本批次（B0）已处理**：O7（三处勘误落地）、O11（subagent 域 Q2 口径勘误）、ADR-002 落档（O4/O5 裁决）。

**本批次（B10）已落地（2026-09-13）**：O4/O9/O10 关闭（collaboration 后端能力呈现、v3 per-prompt 模式字段显式丢弃、swarm 四路径确认统一 + 失败回退 + `/swarm <task>`「已结束」divider——实现事实见 context §6.1；rust+web 实跑验收场景 A1-A7 留待统一回归批次 B12 执行）。
