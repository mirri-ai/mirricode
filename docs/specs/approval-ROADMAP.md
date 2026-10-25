# 权限与审批（approval）— ROADMAP

> 本域状态与开放项的唯一维护点。需求/设计内容见 [approval-requirements.md](approval-requirements.md) / [approval-architecture.md](approval-architecture.md)；现状代码事实见 [approval-context.md](approval-context.md)（快照 2026-09-12）。

## 1. 覆盖状态（2026-09-12 首建基线）

| 能力块 | 覆盖状态 | 备注 |
|---|---|---|
| 权限模式（manual/auto/yolo）语义与切换 | ✅ 已文档化 | requirements US1；含 ACP 四档、启动参数、config 默认值 |
| 模式链序语义（yolo 保留高危问询 / auto 全放行） | ✅ 已文档化 | ADR-003；此前无文档记录的回归风险点 |
| 策略链 13 裁决面与优先序 | ✅ 已文档化 | architecture §2.3 |
| 审批往返（事件、遥测、worker 指引、ask 延续） | ✅ 已文档化 | architecture §3 |
| 会话级批准记忆（去重、恢复、wire 迁移、会话内跨代理共享——SDR-003 修订） | ✅ 已文档化 | requirements US5；迁移细节在 context §1.5 |
| 用户权限规则（[permission] TOML、pattern、校验） | ✅ 已文档化 | requirements US6；刻面生成引用 tool-calling |
| 特殊审批流（计划审查/目标启动/蜂群启动） | ✅ 已文档化 | requirements US7 |
| 问询用户闭环（AskUserQuestion） | ✅ 已文档化 | requirements US8；归属裁决 SDR-001；关闭终态 SDR-005（终止回合、归还输入权，2026-10-13，落地项挂 §3 开放项 10） |
| ACP 问询面（复用权限请求面 + 多题/多选降级） | ✅ 已文档化 | requirements US8/US9.2；context §2.2 |
| 跨端计划拒绝出口差异（TUI 无 Reject and Exit） | ✅ 已如实记录 | requirements US7.1；context §5 缺口 7 |
| TUI 审批面板/排队/命令 | ✅ 已文档化 | requirements US3 |
| web 审批卡片/REST/WS 状态 | ✅ 已文档化 | requirements US9.1/US9.5 |
| ACP 权限请求/模式 | ✅ 已文档化 | requirements US9.2/US1.4 |
| SDK handler / 无头缺省 | ✅ 已文档化 | requirements US9.3/US9.4 |
| 错误/取消/超时/崩溃恢复路径 | ✅ 已文档化 | requirements US10；ADR-002 |
| 外部钩子（PermissionRequest/Result） | ✅ 已文档化 | requirements US11 |
| v1 legacy 差异（60s 过期、parent 链、协议面） | ✅ 已摘要 | architecture §7；context §3 |
| **B9 Rust 轨（2026-09-13）**：会话级批准跨代理共享、用户级规则接线 + `approval.listRules/addRule/removeRule` CRUD、`approvalRule` 刻面下发、`expires_at` 引擎侧零派生 | ✅ 已文档化 | context §7（增量快照）；裁决 4.b（用户级规则不进 wire log，`PermissionRulesAdd` 保持 dead）一并落档 |
| **v3 模式传递（2026-09-16，SDR-004/ADR-004）**：单一事实源、先落地后裁决、全量广播、默认值仅种子 | ✅ 已文档化（终态） | requirements SDR-004 / architecture ADR-004 / US1.2 修订；**v3 实现存三处缺口（context §8），修复项挂 §3 开放项 8——已于 2026-09-16 全部落地（§3 开放项 8 闭环）** |

## 2. 跨域术语（**已注册**，glossary v8 起陆续登记；本表保留作域内映射台账）

| 术语 | glossary 条目 | 说明 |
|---|---|---|
| 权限模式 | AP1 `permission mode`（manual/auto/yolo） | 出现于引擎、TUI 命令、ACP 模式、config、wire；与「计划模式」组合已消歧 |
| 会话级批准 | AP2 `session-scoped approval`（scope: 'session'） | 出现于审批响应、规则记忆、TUI/web/ACP 三端文案；2026-09-12（B0）随 SDR-003 修订为会话内跨代理共享 |
| 审批刻面 | AP3 `approval rule pattern`（工具侧生成） | 规范定义在 tool-calling 域文档化（「审批刻面」），AP3 与之对齐 |
| 裁决 | AP4 `permission decision`（approve/deny/ask） | 引擎内部与遥测共用 |

## 3. 开放项（需人工确认 / 后续补齐）

1. **桌面端（mirri-desktop）审批消费面**：未发现独立审批实现，推测经 SDK/web 面消费——需以其域 recon 为准核对（开放，低优先级）。
2. ~~跨域术语注册~~：**已注册（glossary v8）**——§2 所列四术语即 glossary AP1–AP4，登记完成（AP2 于 2026-09-12/B0 随 SDR-003 修订）。
3. **敏感文件内建清单的归属确认**：清单实现在 `tool/path-access.ts`，触发面在本域——若 tool-calling/execution-environment 域后续文档化该清单，本域 US2 保持引用即可；若无人文档化，建议补入 tool-calling。
4. **v1 协议文档（SCHEMAS.md §6.1/6.2）的归宿**：v1 审批/问询协议形态在代码头注释中自引用 SCHEMAS.md，本域 context 仅摘要——如 Rust 侧需完整 v1 wire 契约，需单独盘点。
5. **增强提案裁决**（requirements §4，六项）：**进度（2026-09-12，B0）**——①过期语义已由 SDR-003 裁决（诚实化：不派生到期值、无真实到期信息不渲染倒计时）；②「始终允许」升级（全局 P-012）与 ③规则管理 UI 核心（全局 P-013）纳入本程序 B9 落地；④显式批量批准（P-014）、⑤审批审计导出（P-015）、⑥敏感文件清单可配置（P-016）**已裁决推迟**（总计划 DEC-2：提案推迟，待产品侧另行立项）。
6. **跨端交互不一致是否统一**（TS 现状如实记录，是否统一待裁决）：(a) 计划审查的「Reject and Exit」仅 ACP/web 提供，TUI 拒绝一律保持计划模式（context §5 缺口 7）；(b) goal 启动确认 manual/yolo 均弹、swarm 启动确认仅 manual 弹（缺口 8）；(c) ACP 问询多题降级只问第一题、多选降级单选（缺口 6）；(d) ACP 问询无「其他」自由文本作答入口（仅预设选项与 Skip），终端面板可自由输入。**已裁决推迟（2026-09-12，总计划 DEC-2）**：四项对应全局缺口 D-09～D-12——TUI/ACP-only 项无法以 rust+web 验收，且与「减少 TS 改动」冲突，TUI 将随 Rust 核心切换整体重估；统一裁决届时另做（若做需追加 SDR）。
7. ~~「审批刻面」术语注册~~：**已注册（glossary v8，AP3）**——跨 approval × tool-calling 两域的规范词条已登记（生成语义由 tool-calling-architecture.md:110 定义，AP3 消费侧对齐）。
8. **v3 模式传递三缺口修复（SDR-004 落地，2026-09-16 挂账）**：验收以 requirements US1.2 修订条目为准，证据与代码事实见 context §8——**已闭环（2026-09-16）**：三子项均落地，web/rust 全量验证通过，commit 见各子项标注。
   1. **web**：首条消息路径落地 composer 所选模式（`createDraftSession`/`startSessionAndSendPrompt` 对齐 skill 路径的 `persistSessionProfile({permissionMode})`，先落地后发首条消息）——**已落地（2026-09-16，`19a7f64d`；服务端模式镜像 `80c121e4`）**；
   2. **rust**：`agent.setPermission`（`dispatch_set_permission`）向会话内全部存活代理广播（对齐 v2 `broadcastPermissionMode`；子代理派生继承保持为时序防御）——**已落地（2026-09-16，`253a90c`；resume 恢复路径一致性地补 `e9e7cc6`）**；
   3. **rust**：`default_permission_mode` 接入为新会话初始姿态种子（`PermissionScopeConfig` 构建处消费；`ConfigChange::Permission` 热更新语义需一并裁定——按 ADR-004 裁决 5：热推送只作用于新会话种子，不覆盖进行中会话）——**已落地（2026-09-16，`8f3f1c9`）**。
9. **多端并开的权限模式推送（终审 Important #1b，2026-09-16 挂账）**：`DomainEvent::PermissionModeChanged`（`rust/crates/mirri-proto/src/events.rs:144`）尚未投影为 WS 事件——模式经其他入口/另一客户端变更时，被动端收不到实时推送，只能靠回合结束/切换/重连的 `/status` 对账收敛（web 镜像通道已按「服务端权威」实现，SDR-004 e；复用事件推送消除对账延迟的方案留待产品侧裁决）。不改代码。
10. **问询关闭终态落地（SDR-005/ADR-007，2026-10-13 挂账）**：关闭语义裁定为「终止回合、归还输入权」（requirements US8 终态、ADR-007 回传形态与机制）。落地项：a) TS v2 `ask-user.ts` 关闭结果携带行为指令 + 引擎侧回合终止（**剩余**）；b) Rust v3 dismiss 分支改造——**已落地（2026-10-13，b/d/e 三项）**：阻塞路径 `dismissed_result()` `stop_turn: true` 终止回合（`rust/crates/mirri-core/src/tools/ask_user.rs`，经 `turn_step.rs` 既有 `stop_turn` 消费机制）；后台路径关闭 → 任务取消定局 + 同一指令通知（`task_notification.rs` 共享 `DISMISSED_NOTE`/`DISMISSED_INSTRUCTION` 常量）；防重问护栏在 `resolve_execution` 入参校验层拒绝原样重问（`session/gates.rs` dismissed-text ring，会话级 256 上限、逐字匹配、answer 不入环、sticky——改述被作答后原样重问原始文本仍被拒）。无处理器宿主（headless/SDK 空回调）不记录关闭文本，护栏不触发（无用户可骚扰，可接受）；c) web/desktop 问询卡片关闭路径（含可选原因输入面）对接（**剩余**）；现状差距见 context §9。TUI 面关闭交互的 UX 调整随 Rust 核心切换整体重估（口径同开放项 6）。
11. **问询投递连接韧性落地（SDR-006/ADR-008，2026-10-24 挂账）**：投递通道断开时问询从「立即错误回传」改为「挂起等待重连，上限 5 分钟（可配置）」——事件起源 session-028fcdba 复盘（连接断开后 `question.ask` 7ms 内快速失败两次，用户无感知，全靠模型自觉降级，现状快照见 context §10）。落地项：a) `AskUserQuestionTool` 回调改为投递时槽位重解析 + 通道类错误等待环（阻塞/后台两路径，`rust/crates/mirri-core/src/tools/ask_user.rs`）；b) WS 回调通道类错误签名文档化（`ReverseRpcCallback::call` 错误约定，`rust/crates/mirri-core/src/reverse_rpc.rs`）；c) 服务级等待上限配置（缺省 5 分钟；「从未附着」场景不受上限影响——结构性区分见 ADR-008）；d) approval.ask 的同类投递路径补「主动重发 + 等待上限」（**已具备保持待决语义，补齐另裁决——不随本项交付**）；e) question gate 的重连卡片重推（对称 approval 的 list_pending 重推，**不在本项交付范围，另行裁决**）。

## 4. 评审记录

- **第 1 轮（2026-09-12）**：design-reviewer 12 条（1 blocker：计划审查 auto 行为误述；3 major：swarm yolo 误述、ACP 问询面缺失、危险模式计数）、architecture-reviewer 6 条（计划审查 auto、8≠9 高危模式、TUI/ACP 计划拒绝序差异、13≠12 展示形态、context 指代、行号微漂移）。全部核实属实，已修复（行号 ±5 内漂移按 reviewer 建议留待下次快照统一校准）。
- **第 2 轮（2026-09-12）**：上轮 18 条全部 resolved；两位 reviewer 复审通过修复并各提出新 finding——design-reviewer：ACP 问询缺「其他/自由文本入口未披露」（minor）；architecture-reviewer：context 对 modal-coordinator 同主替换分支的可达性结论写反（major，系第 1 轮修复引入）、问询桥接行号区间截短（minor）。均已修复。
- **第 3 轮（2026-09-12，终审）**：两位 reviewer 确认全部 resolved，**零 comments**。累计 3 轮、21 条 findings 全部闭环。
- 遗留治理项：「审批刻面」等跨域术语的 glossary 注册（见 §2/§3，发起方统一合并时处理）。**后记（2026-09-12，B0）：该项已完成注册（glossary v8，AP1–AP4，见 §2）——本行作为评审当时状态的历史记录保留，现行状态以 §2 为准。**

## 5. 落地程序范围标注（baseline-gap-landing 程序 · 2026-09-12）

> 依据总计划 [`plan/baseline-gap-landing.md`](../../plan/baseline-gap-landing.md)（§一 DEC-2/DEC-6 范围裁决、§二 批次定义、§四 进度跟踪）。本节只做状态与范围管理。

**已裁决推迟（本程序排除，DEC-2）**：

- D-09（计划拒绝跨端统一）、D-10（goal/swarm 确认不对称）、D-11/D-12（ACP 问询降级/自由文本）——TUI/ACP-only 项无法以 rust+web 验收，TUI 将随 Rust 核心切换整体重估（见 §3 开放项 6）。
- P-014（批量批准）、P-015（审计导出）、P-016（敏感清单配置）——提案推迟，待产品侧另行立项（见 §3 开放项 5）。

**落地中（本程序进行中的缺口项与批次）**：

| 批次 | 缺口项 |
|---|---|
| B9 审批规则管理与诚实性 | D-34（web 审批规则面）、P-012（「始终允许」一键升级用户级规则）、P-013 核心（规则管理 UI）、D-13（审批过期诚实化——SDR-003 已落档）、D-07（会话级批准跨代理共享——SDR-003 已落档，glossary AP2 同批修订） |

**B9 进度（2026-09-13）**：Rust 轨已完成——D-07（`SessionApprovals` 会话共享）、D-34 引擎侧（规则接线 + CRUD RPC 三方法 + 热重载推送 + `session.get` 快照诚实化）、`approvalRule` 刻面下发（ask 载荷 / listPending / interaction）、`expires_at` 引擎侧核验零派生（context §7）。web 轨（P-012 升级入口、P-013 核心面板、D-13 web wire 清除）由 B9-Web 轨另行落地，落地后本表勾选。

**本批次（B0）已处理**：SDR-003（会话级批准跨代理共享 + 过期诚实化）落档；US5/US10 正文同步修订为终态。
