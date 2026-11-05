# 定时任务（cron-scheduling）ROADMAP

> 状态/开放项唯一源。需求见 [cron-scheduling-requirements.md](cron-scheduling-requirements.md)（阶段一）与 [cron-workspace-scope-requirements.md](cron-workspace-scope-requirements.md)（阶段二）；架构见 [cron-scheduling-architecture.md](cron-scheduling-architecture.md)；现状见 [cron-scheduling-context.md](cron-scheduling-context.md)。本文只做状态管理，不承载需求或设计。

## 覆盖状态（2026-11-01 实施完成更新）

| 阶段 | Epic | 主题 | 状态 |
|---|---|---|---|
| 一 | A | 任务来源与创建面（含 SDR-003 循环默认改判、SDR-006 代建门槛、SDR-007 血统防线——SDR-106 收窄为循环任务 + A.6a 自计时豁免 + A.6b 血统链累计上限） | ✅ 已实现（批 1：TaskOrigin/max_fires/fire_count/血统累计/门槛策略） |
| 一 | B | 存活规则（TTL fire-refresh，SDR-005；B.3 未触发一次性任务的会话终结清理） | ✅ 已实现（批 2：fire-refresh 基准改造） |
| 一 | C | 调度体系与忙闲一致性（含 SDR-004 trigger gate；SDR-010 session scope 归属与 #180 诚实性口径；C.2a 迟到标记） | ✅ 已实现（批 2：统一调度器、held 侧化、trigger gate、迟到标记） |
| 一 | D | 无人值守执行姿态（SDR-002） | ✅ 已实现（批 1：unattended ask-deny + TurnOriginContext 管道 + 子代理传播） |
| 一 | E | 管理面与总开关（D-M6 兑现、SDR-008 聚合可见性） | ✅ 已实现（批 3：cron.add RPC + 聚合面板；开关由并行会话兑现 #194） |
| 一 | F | 收尾与状态如实 | ✅ 已实现（批 1-3 随批落地：回执/列表投影/事实表） |
| 二 | A | 工作区归属与独立存活 | ✅ 已实现（批 4a：TaskScope/WorkspaceCronStore/双面调度） |
| 二 | B | 执行形态（每次运行新会话，SDR-101；悬空清理 D-M5 b） | ✅ 已实现（批 4b/5：RunSpawner + 7 天清理 + 崩溃孤儿收敛） |
| 二 | C | 管理面与运行记录 | ✅ 已实现（批 4b/5：cron.runs.list/run.open + 面板运行历史） |
| 二 | D | 创建审批（SDR-105 提议-审批、never-auto-approve 含检查顺序硬化 B1 与异步回执闭环 D.0e；批 4 计划评审：审批走既有 approval 通道/proposals.list 纯查询、D.0d 一次性工作区提议亦拒；D-M7 被取代）+ 运行时权限姿态三档（SDR-103，含时区显式展示 D.5、ReadOnly 写类判定 ADR-015） | ✅ 已实现（批 4b：CronPropose + 12 格矩阵 + 全链路 e2e；批 5：ADR-015 readonly 执法 + asked_ops 留痕 + 姿态选择器 UI；时区显式呈现批 5 表单）｜**2026-11-04：SDR-126/ADR-022 收敛为单次工具审批，待确认区与旁路审批下线（进行中）** |
| 二 | E | 全局可见性与统一入口（SDR-104 a/b 项） | ✅ 已实现（批 3/4a/5：聚合视图双口径 + 工作区分组 + 提议审批卡） |
| — | 提案 P-CS1..P-CS6 | 超出本批的增强 | ✅ 已登记（各需求文档 §11/§10） |
| — | 用户直建表单（独立需求文档 cron-task-creation-ux：VU1-18、SDR-107~113） | 表单=Session 全套环境+结构化日程（六档+Custom 弹窗+interval）、三时刻预览、环境五项透传（ADR-016~019） | ✅ 已实现（批 6/7/8：9b30ed7fb / 49661eb99 / 15ccd4661） |

> 实施记录（2026-11-01）：阶段一批 1-3（commit 89dd4f09f / 9cd6d897e / 2ecf9b323）、阶段二批 4a/4b/5（e5145c2ed / 674d4ee41 / 640dcf947 + dd5ef915c）均已合入 `latest`；每批经独立代码评审（含修复循环）。已知后续项：usage 用量统计未接入（RunRecord 如实标注「—」）；V20 通知摘要为最后一条 assistant 消息截断（D-M6 实施裁决）；O-1（SDR-003 changeset 级别）仍待用户复核。
>
> 实施记录（2026-11-01，改善批次）：见下方「改善批次」表与实施记录段。
>
> 实施记录（2026-11-01，表单交付）：ScheduledTaskPropose 更名+感知修复（7c5425568）；用户直建表单批 6/7/8 全部合入（后端字段/求值内核/preview RPC → SchedulePicker → 三区块表单+常驻态三时刻），全局验收 rust 5203 通过 / web 2066 通过 / TS 192 通过 / clippy 零警告。已知后续项：SchedulePicker 时间下拉 15 分钟粒度 vs 任意分钟（批 7 遗留，context §4.2 登记）；日历选择器为 date input 最小实现（月历网格登记为前置项）。
>
> 实施记录（2026-11-02，认知走查改善批）：见下方「走查改善批（2026-11-02）」表。

## 走查改善批（2026-11-02）

需求见 [cron-task-creation-ux-requirements.md](cron-task-creation-ux-requirements.md) v1.5/v1.6（新增 SDR-114~120，验收 VU19~VU25）。一次认知走查的落地批次，同一发布列车，changeset 在本批生成（minor）。

| 主题 | 需求 | 状态 |
|---|---|---|
| 表单校验时机后置到「触犯之后」 | SDR-114（VU19） | ✅ |
| 配置选择器与主操作视觉分区 + 编辑态保存常驻 | SDR-115 / SDR-118（VU20 / VU23） | ✅ |
| 「每 1 天/周」读作「每天/每周」；时间选择器分钟粒度放开为全集 | SDR-116 / SDR-117（VU21 / VU22） | ✅ |
| 终态词集与语义区分：自然结束读「已结束 · 按计划结束」，未运行读「尚未运行过」 | SDR-120（VU25） | ✅ |
| 卡片元信息左聚拢；预测标题去重（表单侧持有，时间轴不再自印） | SDR-119（VU24）+ 走查 P2-1 | ✅ |

> 实施记录（走查改善批，2026-11-02）：五项 UI 行为改动均已落地并由渲染断言与源码 pin 覆盖（去重与聚拢两条以源码 pin 钉住，理由同前：样式表与「模板里不再存在的元素」渲染树读不到）。
>
> **开放项（本批登记）**：自然结束内部仍有两种收尾路径——「到达结束日期」与「次数预算用尽」——当前统一读作「已结束 · 按计划结束」，两者是否需要在卡片/详情上细分呈现，待用户诉求再定（不细分不违反 SDR-120，细分需回到需求侧立裁决）。

## 改善批次（cron-improvement，2026-11-01）

需求见 [cron-improvement-requirements.md](cron-improvement-requirements.md)。四个批次为同一发布列车，changeset 只在批 4 生成（minor）。

| 批次 | 主题 | commits | 状态 |
|---|---|---|---|
| 1 | 工具面重构：`SetTimer` 新建、`CreateScheduledTask`（原 ScheduledTaskPropose）、删除 CronCreate/CronList/CronDelete、proposal 持久层下线 | `3e40ac8ce` + 评审修复 `00b741d93` | ✅ |
| 2 | 后端能力：`prev_fire_for`（A5 理论上次）、`cron.update/pause/resume`（A9）、权限三档 `permissionMode`（A4）、工作区级 `cron.pendingApprovals`（A13） | `d498dda3b` + 评审修复 `94da2460f` | ✅ |
| 3 | 前端翻新：ScheduledTaskPanel 列表/表单/详情、ScheduleTimeline 预测时间轴、待确认区、通知跳转、wire 客户端收口 | `5595a7124` `f46f19e0b` `a2e398bed` `b31b1fa52`（`cron.proposals.list` 删除随 `e38701785`） | ✅ |
| 4 | 清理、文案、治理回写：`unattended` 全链路退役、i18n 键名修复、结构测试改渲染断言、changeset、文档工具面更新 | 见本节实施记录 | ✅ |

> 实施记录（改善批 1-4，2026-11-01）：
>
> **A4 三档执法的落地口径**：任务的 `permissionMode` 是唯一执法依据——`manual` 的问询停在审批通道等待（不再被拒绝），`auto` 走常规自动批，`yolo` 从不问。`permissionPosture` 降为内核换算细节，只用于读取批 2 之前的旧记录。批 4 取消了「未声明 mode 的旧记录按 `unattended` 推导是否拒绝问询」这条路径：这类任务按「没有声明执法依据」处理，其问询仍被拒绝（否则 manual 的问询会停在无人应答的通道里），但这只是旧记录的读法，不是产品概念。
>
> **`unattended` 概念退役**：任务记录、`cron.add` 入参、列表行、`cron.fired` 的 origin、`<cron-fire>` 信封全部移除该字段；旧记录里残留的键读取时忽略、写回时丢弃（用回归测试钉死）。`SetTimer` 创建的任务按会话的计时常量执行（超过 3 次的预算需审批），不再通过该字段表达。
>
> **gate 不持久化豁免**：审批待办（A13 待确认区的事实源）不落盘——服务重启后未决待办丢失。沿用既有审批域行为与 A13「引擎不设过期」口径，记为已知边界，不在本批修复。
>
> **预测三时刻**：`cron.preview` 新增 `previousT0`；interval 臂复用正向求值的锚定与日钳制路径，`now` 恰为触发点时 prev 含 now。一次性规则无理论过去，前端显示「—」。
>
> **t9 flake 的处理结论（不是放宽超时）**：会话标题用例 `should_close_the_abort_residue_before_the_next_cron_turn` 在满载并发下偶发 5 秒超时。诊断结论是**真实投递缺陷**：`cron.trigger` 命中忙碌回合时把 fire 挂进 held 队列，而唯一能排空它的只有调度器 tick——没有 tick 的服务器（测试服务器即为此类）永远不投递，run-now 回执承诺的运行不会发生。修复：trigger 的忙碌分支为这次 fire 挂一个**独立于该回合**的补投任务，flow 一放行就把 held 清空并投递；同时把「单 fire 注入并清空 hold」抽成共享体，调度器与补投走同一条路径。验证：修复前同样压力（4 路并发 × 6 轮）失败 3/12 组，修复后 24/24 组全绿，用例本身与超时预算都未改动。补投的回归用例 `should_redeliver_a_held_run_now_fire_without_a_scheduler_tick` 直接钉住「无 tick 也必投」。
>
> **结构测试改渲染断言**：原先用正则扫 SFC 源码断言页面结构的测试，改为真实渲染（服务端渲染器）后读渲染树；仅「栅格列数」（样式表）与「某字段不存在于模板」两类保留源码读取，理由写在用例旁。

## 评审记录

| 轮次 | 评审者 | 范围 | 结论 |
|---|---|---|---|
| 0 | 建域自检（R1–R9） | 全套件 | 自查通过（见需求文档各 Epic 验收句式）；外部 reviewer 轮待调度者安排 |
| 1 | 设计评审（2026-10-30） | 全套件 | 8 项修正采纳（2 Blocker：#179 门槛/血统遗漏；3 Major：可见性/工具入口 scope/悬空清理；3 Minor：四禁/编号/时区）——本批修复即其落地（SDR-006~008、SDR-104、ADR-010~012） |
| 2 | 用户裁决（2026-10-31） | 定位与权限模型 | 3 项裁决落档（SDR-009/010/105、ADR-013）：阶段定性分离；session scope 为 agent 计时正确归属、#180 聚焦诚实性；工作区任务创建改提议-审批（推翻 D-M7、澄清 SDR-103 范围） |
| 3 | 双视角评审（2026-10-31：产品视角 + 权限机制视角） | 全套件 | 1 Blocker + 4 Major + 3 Minor 全数落档（B1 never-auto-approve 优先级与十二格推演；M1 turn_origin 子代理传播；M2 scope 硬判定/工具面无 scope 参数；M3 自计时豁免收窄血统防线 → SDR-106；M4 产出物摘要随通知；M5 提议回执闭环 + TTL + 不可信呈现；Minor：迟到标记 / 未触发清理 / 架构编辑残留）——SDR-106、SDR-105 补充、ADR-001/013 修订、ADR-014、glossary v35 同批 |
| 4 | 审批矩阵落档审查（2026-10-31） | 全套件 | 文档完整性修订（非新裁决）：6 项缺口补齐——G1 创建路径审批总览表（阶段一 §2 单一入口）；G2 成本可折叠性对比（阶段二 D-M8）；G3 V16d 姿态可见性验收；G4 CronPropose 定名（架构 §0/§6/§9.3/ADR-014）；G5 提议入口形状入阶段二 D.0；G6 十二格口径修正 |
| 5 | 批 4 计划评审（2026-10-31） | 全套件 | 2 Major + 5 Minor 裁决落档——Major 1：审批通道归属维持 ADR-013（decide 走既有 approval.ask/decide 通道，proposals.list 为纯查询面 + 回退条款，架构 §9.3 实施裁决段）；Major 2：工具能力类别声明 ADR-015（泛化 plan 白名单，只读/非只读二分）；Minor：D.0a 提议字段清单显式化、A.4 in-flight 运行终止记录原因、D-M6 摘要降级（最后一条 assistant 消息截断，V20 口径同步）、D.0d 一次性工作区提议亦拒（机械简单性） |

## 依赖关系（两阶段：共享基建、定位独立——SDR-009）

两阶段是**不同角度的两类需求**（阶段一 = agent 自计时能力，阶段二 = 用户委托的例行工作），不是递进层；阶段二**复用**阶段一立下的技术基建（权限接线、调度时钟、存活规则、数据模型扩展），但产品定位独立陈述：

```text
阶段一（cron-attended——agent 自计时能力，session scope 为正确归属）
  ├─ D 数据模型扩展（origin/max_fires/fire_count/unattended/scope 字段）
  ├─ D 权限接线（turn_origin plumb-through + Ask 拒绝泛化）   ← 共享基建核心，先行
  ├─ D 统一调度器（时钟上提 + held 任务侧化）
  └─ D 开关兑现 + 文档对齐（SDR-003/B6 工具描述对齐在 #177 快修批，不在此处）
阶段二（cron-workspace-scope——用户委托的例行工作；复用共享基建，独立定位）
  ├─ scope 上提 + 工作区存储
  ├─ RunSpawner（执行会话形态）
  ├─ 提议-审批通道（SDR-105：never-auto-approve 策略位，复用阶段一的权限接线）
  └─ 面板扩展（运行记录 / 姿态选择 / 成本聚合）
```

## 开放项（需人工/后续裁决）

- **O-1** SDR-003（循环默认改判）的 changeset 级别：默认按 minor 处理，**待用户复核确认**后生成。
- **O-2** 阶段一与 #177 快修批次（held-buffer purge、工具描述文案对齐）的合并顺序——快修先行可减少本域实施时的行为噪声，建议快修先落地。
- **O-3** 阶段二 P-CS6（进程外调度/守护形态）是否立项——超出「服务内调度」边界，需产品判断。
- **O-4** subagent 域 E11/E12 与本域的文档归属修订（拆域后 subagent 域内 cron 章节是否改为引用本域）——建议在本域第一批实施落地时一并走彼域 SDR。
- **O-5** 会话级累计触发预算的通用强制门（#179 开放问题 1）：初版不做（D-M8 裁决）——通用强制门是否加严待阶段一实施后有数据再议；血统链上的累计风险已有专项兜底（SDR-106 血统链累计上限：cron 血统创建的一次性任务按会话累计计数、超限拒绝——非 cron 血统的会话内累计仍待数据）。
- **O-6** D-M 编号跨阶段撞号（阶段一 D-M7/D-M8 与阶段二 D-M7/D-M8 各自同号）：当前靠「阶段一/二 D-Mx」前缀消歧（架构文档头部已声明编号独立），建议阶段二首批实施时统一加前缀或改独立编号系——纯编辑整理，无裁决依赖。
- **O-7** 工作区任务**产出物的完整投递渠道**（运行产出发送到外部即时通讯 / 邮件等）：本域只裁决「产出摘要随终态通知携带」（D-M6），外部投递涉及各渠道自身的鉴权与频控语义，超出本域边界——待产品判断后另立裁决（可参考通知域 P-022 的分级路径）。

- **O-8** 创建表单的权限模式选择器：改善需求 A4 要求表单呈现 `manual/auto/yolo` 三档并删除第四档（read-only），批 3 的表单当前未呈现该选择器，wire 的 `permissionMode`（以及 `concurrency`）由前端创建入口未发送、服务端按默认处理；表单改接该字段与删除服务端的旧姿态入参需一次决策（会改变「表单显示的」与「内核执行的」是否同名）。在此前文档按「接口仍接受旧姿态字段」如实描述。

## 实施批建议（plan 拆分输入，非状态）

1. 数据模型 + 权限接线（阶段一 Epic A/D 的机制底座：字段、turn_origin、Ask 拒绝泛化）——先行，测试覆盖最重；
2. 统一调度器 + held 任务侧化 + trigger gate（Epic B/C）——依赖批 1；
3. cron.add RPC + 面板补全 + 开关兑现（Epic E/F）——依赖批 1，可与批 2 并行；
4. 阶段二 scope/存储/RunSpawner/提议-审批通道（依赖批 1-3 全部；提议通道复用批 1 的权限接线）；
5. 阶段二面板扩展（运行记录/姿态/成本）——依赖批 4。
