# 回合调度域 — ROADMAP（状态与开放项）

> 本文件是本域开放项与执行状态的唯一维护点（按 `docs/specs/AGENTS.md` 治理约定）；需求/设计/现状分别住在 [turn-scheduling-requirements.md](turn-scheduling-requirements.md)、[turn-scheduling-architecture.md](turn-scheduling-architecture.md)、[turn-scheduling-context.md](turn-scheduling-context.md)。

## 1. 文档覆盖状态

| 能力面 | 需求 | 架构 | 现状盘点 | 备注 |
|---|---|---|---|---|
| E1 回合生命周期与单活动回合 | ✅（v1 基线 2026-09-13；US1.4 空响应守卫 SDR-002 2026-10-09） | ✅（ADR-002） | ✅（§3.1 空响应静默完成痛点） | |
| E2 准入策略（四类封闭集） | ✅ | ✅ | ✅ | 陷阱：四分支非两分支已披露 |
| E3 插话（steer） | ✅（US3.1–3.3；插话来源标识 ADR-003 2026-10-13） | ✅（ADR-003；§5 契约表插话行独立标识） | ✅ | 来源标识机制侧落地挂 O7 |
| E4 统一异步唤醒源契约 | ✅（SDR-001/ADR-001 契约正式化） | ✅ | ✅（接入面全景表） | 契约文档化完成，既有源合规审计见 O1 |
| E5 取消与中断 | ✅（US5.3–5.6 僵死看门狗四原则 SDR-003 2026-10-15；用户决策无限等待 SDR-004 2026-10-16） | ✅（ADR-004；ADR-005） | ✅（§3.2 两起误杀证据；§3.3 用户决策误杀证据） | 机制侧已落地（O8，2026-10-16）；ADR-005 待落地（O9） |
| E6 步数上限 | ✅ | ✅ | ✅ | |
| NFR N1–N5 | ✅ | ✅ | ✅ | |

## 2. 待人工确认的开放项

| # | 开放项 | 来源 | 建议去向 |
|---|---|---|---|
| O1 | 统一唤醒源契约（ADR-001）为文档化契约，非代码约束——既有源（任务通知/cron/goal 续航/stop hook）各自构造 StepRequest，接入面靠惯例一致；是否引入代码层强制（如统一工厂/注册表）待裁决 | SDR-001/ADR-001 | Rust 侧落地时一并裁决（新引擎可直接以契约为接口设计输入） |
| O2 | Rust 无 v2 式 admission 分类队列（复刻 v1 steer 缓冲形态，turn_flow/steering.rs）——四类准入/mergeable 折入/跨 turn 存活无对应物 | context §3 | Rust parity 缺口，落地批次待排 |
| O3 | `TurnEndReason` 含 `'blocked'` 但 v2 从不产生（保留位；goal 的 blocked 走 UpdateGoal 工具路径）——wire 契约与运行时行为的口径差 | context §4 | 裁决：保留（向后兼容）或在 Rust 侧收敛枚举 |
| O4 | ~~goal 等待挂起的机制侧（续航发放暂停接口、挂起期间唤醒源即唯一驱动）已由本域架构 §4 挂起接口（ADR-001）落位，**未实现**——随 goal 域 E5 落地批次排期~~ | subagent 域 SDR-011/ADR-008（机制侧收编） | **已落地（2026-09-14，goal 域 E5 批次）**：机制侧以"goal 服务在 turn.ended 消费点不发射续航提示"实现——未新增回合类型/准入类别/挂起接口（比 §4 设计的显式暂停接口更简，详见 goal-architecture ADR-002）；看门狗为新增定时唤醒源，复用 StepRequest 惯例（无新接入面） |
| O5 | 唤醒源优先级是否分级（裁决点 Q1）维持先来先跑缺省 | requirements §9 | 触发时再裁决 |
| O6 | **steer 跨回合存活的承诺与实现矛盾**：stepRequestQueue 头注（:10-13）声称 "agent-scoped requests (steers) carry into the next turn"，但 `releaseActiveTurn`（loopService.ts:565-575）在回合结束时无条件 cancel 全部排队/运行中步骤（含非 turnScoped 的插话）——回合正常完成时队列已耗尽故无残留（承诺空转成立）；**异常终止路径（钩子停/步数上限/失败/取消）的残留插话实际被丢弃**，与头注意图矛盾且无测试锁定。US3.2/architecture §4.3 已按现状如实化（异常终止随回合终止）。裁决方向：a) 改代码（回合结束时把未 aborted 的非 turnScoped 步骤迁入暂存队列）b) 维持现状契约（文档已如实化，头注待修） | 本轮评审（TS-1，2026-09-13） | Rust 落地时裁决；Rust 侧按 b 起步（与现状对齐） |
| O7 | **插话来源标识独立**（ADR-003，2026-10-13 登记）：准入侧为折入运行中回合的插话打"用户·插话"标识（消费端呈现契约见对话渲染域 ADR-010/CR13 批次）——机制侧改动待排期 | 对话渲染域 SDR-001（用户插话切断回合的呈现缺陷） | 与对话渲染域 CR13 同批落地 |
| O8 | ~~**僵死看门狗重设计落地**（ADR-004，2026-10-15 登记）~~ **已落地（2026-10-16，worktree `feat-watchdog-redesign` squash 合回 `feat/latest`，提交 `2def121`）**：四件套——a) 活动信号判据（`wait_for_completion_with` 心跳推进上报活体、看门狗 progress 通道接入）b) 取消直达等待点（wait select 回合取消令牌 + `cancel_stalled` 来源标记）c) 两级告警日志（`StallTurnKey` 上下文）d) stalled 词汇全链路（`TurnStopReason::Stalled` → wire/reducer/activity outcome → web `Stalled` 徽标 + i18n + 类型联合 + 未知 reason 归非完成族）。覆盖范围：直驱回合；子代理回合不纳入（由任务心跳/成员超时兜底，待子代理域提出再裁决）。**补记（2026-10-18，`session-f1e24e04` 事故）**：a 项活动信号判据存在一处信号源头断供——前台超时转后台的续跑循环（`hand_over_to_background`）不打任务心跳，静默命令（无 stdout 的长测试）的心跳即刻变陈旧，子代理 `TaskOutput` 阻塞等待无活体可报，父回合被误判 stalled；已修复（续跑循环补进程心跳 ticker，与 `drive_with_process_heartbeat` 同姿态），单元 + 事故一比一端到端（`should_keep_the_parent_turn_alive_when_a_foreground_subagent_waits_on_a_background_task_converted_from_a_foreground_timeout`）双守护。"子代理等待的后台任务心跳经共享 TaskManager listener 喂父回合 stall 时钟"由此生效；子代理回合自身的 stall watchdog 仍不纳入（维持原裁决） | SDR-003/ADR-004（2026-10-15 两起 Stopped 事故复盘）；session-f1e24e04 补记 | 引擎侧与 web 消费端同批落地；后续项：对话渲染域"已停滞"悬停归因文案（跨域待办）、P-028 恢复提示（未裁决提案） |
| O9 | **等待用户决策无限等待落地**（ADR-005，2026-10-16 登记）：三件套——a) 看门狗未决用户决策豁免（`rust/crates/mirri-embedded`：静默到期查审批门/提问门未决计数，有未决即重置计时）b) ACP 用户决策请求移除固定超时（`rust/crates/mirri-acp`：权限请求/用户征询改无限等待，断连走拒绝兜底；机器类请求维持既有超时）c) 看门狗取消路径归因字样改引擎停滞语义（消除"用户取消"误归因）。注意：提问工具阻塞路径的工具层保活（`drive_ask_with_keepalive`，2026-10-16 已合入）保留为纵深防御，不因豁免移除 | SDR-004/ADR-005（权限审批误杀 + ACP 60s 超时复盘，证据 context §3.3） | Rust 引擎批次落地（TDD：豁免保回合/无豁免回归/ACP 无限等待三组测试） |
| O10 | ~~**僵死看门狗三代架构：推模型改拉模式**（P-029 提案，2026-10-18 登记）~~ **已裁决通过（2026-10-18，用户裁决 1-A）**：判据从"全局静默 + 各等待点自觉喂信号"改为"回合等待状态机（WaitKind 封闭枚举 + Unknown 兜底）+ 向被等待对象 owner 拉取四态活体事实（Advancing/Settled/Stalled/QueryFailed）"；模型响应类静默判据废除（模型侧归 HTTP 读空闲超时，引擎侧归第三层长窗）；双窗结构（类静默窗 + 第三层长窗）不串联；两级收尾继承。**裁决参数（用户裁决 2）**：兜底窗口 30 分钟统一（不分窗）；QueryFailed N=3（WaitingGate 豁免降级）；诊断 dump 落 tracing 结构化日志；ACP 同架构不同配置；子代理前台直驱维持不纳入；WaitKind 六类 + Unknown 起步；Unknown 窗 ≥600s 约束。ADR-006 已落 architecture、SDR-005 已修 US5.5/US5.6、glossary L4-L7 已登记（2026-10-18）。**已落地（2026-10-18，worktree `feat-adr006-pull-watchdog` squash 合回 `feat/latest`，提交 `f037479`）**：四件套——a) 等待框架（mirri-core `turn_flow/wait_registry.rs`：WaitKind 封闭枚举 + 集合语义 + RAII 登记，各 await 点经框架语法接线）b) owner 四态查询（Advancing/Settled/Stalled/QueryFailed，N=3 降级，WaitingGate 豁免；delta touch 补齐中流活体）c) 双窗分层（classic 窗 + 30 分钟 backstop，Advancing 解除武装、到期先 re-probe、dump 诊断前置两级收尾）d) 等待链传播（父的 WaitingSubagent 回落查子 registry——fork/转后台事故形态；liveness listener 因跨层传播不完整**保留**为兜底，I4 粗粒度已知限制记录在案）。两轮 design-review（C1 dispatch 接线死代码、C2 backstop 不解除、I2 tool 否决权 bug 均修复）；pull-only e2e（push 全死纯拉存活）+ 事故 e2e 家族（fork/转后台/steer-stalled/goal-pause 双窗语义更新）全绿。**遗留（O10 挂账）**：推喂线全量退役（LlmPending 心跳、任务心跳、ask keepalive 维持纵深防御——多引擎宿主无 dispatch watchdog；liveness listener 待等待链覆盖证明完整后退役）、skill_reference reformat 噪声教训（后续大改动拆分提交） | P-029（本域三代提案；用户裁决） | 已落地；遗留项见上 |

## 3. 变更历史

- 2026-09-13：建域四件套（SDR-001/ADR-001 统一异步唤醒源契约收编；用户裁决与 goal 域同期建域、机制/产品分离）。现状盘点基于 v2 `agent/loop/` 全量勘察（快照 2026-09-13）。
- 2026-10-09：SDR-002/ADR-002 空响应守卫（US1.4：模型空手而归的回合不得静默记为完成）——由真实会话 `session-b1a4d19b-*` 的空响应静默完成缺陷触发（证据见 context §3.1）；Rust 侧同批次落地（步级重试 + 显式失败收尾，`rust/crates/mirri-core` loop_engine）。
- 2026-10-13：ADR-003 插话来源标识独立为"用户·插话"（准入侧打标；由对话渲染域 SDR-001 的插话切断回合呈现缺陷触发，呈现契约在对话渲染域 ADR-010）——登记 O7，机制侧落地与对话渲染域 CR13 同批。
- 2026-10-15：SDR-003/ADR-004 僵死看门狗重设计（活动信号判据、取消直达等待点、触发可见、结局词汇拆分）——由同日两个会话（`session-aad4bb23-*` / `session-6d39689c-*`）同因"TaskOutput 阻塞等待被 300s 静默看门狗误杀、终止全程不可归因"触发（证据 context §3.2）；登记 O8，跨域恢复提案 P-028。
- 2026-10-16：SDR-004/ADR-005 等待用户决策的无限等待（看门狗未决用户决策豁免、ACP 用户决策请求免超时、归因终态）——由权限审批误杀与 ACP 固定超时复盘触发（证据 context §3.3；提问工具保活已先行合入作纵深防御）；登记 O9。
- 2026-10-18：O8 补记（ADR-004 batch a 缺口修复）——`session-f1e24e04` 事故（前台子代理等待"前台超时转后台"的静默任务 600s，父回合 turn 21 被误判 stalled、子代理连带 aborted）定位为 `hand_over_to_background` 续跑循环不打进程心跳（信号源头断供）；修复为续跑循环补心跳 ticker（与 spawn 路径 `drive_with_process_heartbeat` 同姿态），单元 + 端到端双测试守护。分析报告：`plan/stall-watchdog-liveness-gaps.md`。
- 2026-10-18：P-029/O10 僵死看门狗三代架构提案登记（推模型心跳 → 拉模式探活；WaitKind 状态机 + owner 四态查询 + 长窗口兜底）——由 ADR-004 落地后 48 小时内三起新形态误杀复盘触发（证据 context §3.5；含 fdf98268 turn 47/61、f1e24e04 turn 21、7049e890 子代理 turn 8）。**未裁决**：6 项开放问题（兜底窗口值、QueryFailed 降级 N 值、诊断落点、ACP 适用性、子代理纳入时机、WaitKind 粒度）。
- 2026-10-18：P-029/O10 裁决通过（用户裁决 1-A/2 全采纳/3 选 a）——ADR-006 落 architecture（§4.5 正文改写为拉模式形态）、SDR-005 修 US5.5/US5.6 措辞、glossary v24 登记 L4-L7（等待类别/owner/推进/类静默窗）、提案文档注册为域内 design subclass。**实现未排期**：等待框架 + owner 查询接口 + 喂线退役，迁移期推拉双判据并存。
- 2026-10-18：O10 实现落地（用户 /goal 落地指令）——ADR-006 拉模式看门狗四件套 squash 合回 feat/latest（f037479）：等待框架 + owner 四态查询 + 双窗分层 + 等待链传播；两轮 review 修复（C1 dispatch registry 接线死代码、C2 backstop 武装后不解、C3 静默模型语义裁决为 Degraded 长窗、I2 无界 tool 等待的 Advancing 否决权 bug、delta touch 补齐）；测试资产：11 单测 + watchdog 分层测试 + pull-only e2e + 事故 e2e 家族（fork/转后台/steer/goal 六个测试按双窗语义更新）。遗留挂账：推喂线全量退役（纵深防御保留）、liveness listener 退役待等待链覆盖完整。
