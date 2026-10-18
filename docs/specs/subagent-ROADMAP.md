# 子代理派发 / 后台任务域 — ROADMAP（状态与开放项）

> 本文件是本域开放项与执行状态的唯一维护点（按 `docs/specs/AGENTS.md` 治理约定）；需求/设计内容分别住在 [subagent-requirements.md](subagent-requirements.md) 与 [subagent-architecture.md](subagent-architecture.md)，现状盘点住在 [subagent-context.md](subagent-context.md)（结构快照 2026-09-09，增量快照 2026-09-12，等待语义快照 2026-09-13）。
> Rust 侧迁移的既有对齐记录见 `rust-core-design/` 与 `plan/rust-*.md`（本文件只引用不重复）。

## 1. 文档覆盖状态

| 能力面 | 需求 | 架构 | 现状盘点 | 备注 |
|---|---|---|---|---|
| E1 派发与角色/型号选择 | ✅ 已覆盖（v1 基线） | ✅ | ✅ | |
| E2 前台执行与结果 | ✅ | ✅ | ✅ | 2026-09-12 增补：摘要提炼（SDR-004/ADR-007） |
| E3 后台执行 | ✅ | ✅ | ✅ | 2026-09-12 增补：输出上限、宽限停止细节 |
| E4 续作 | ✅ | ✅ | ✅ | 2026-09-12 增补：批量续作名单映射（SDR-004/ADR-007） |
| E5 时长上限与中断 | ✅ | ✅ | ✅ | |
| E6 批量派发 | ✅ | ✅ | ✅ | |
| E7 观测与状态事件 | ✅ | ✅ | ✅ | |
| E8 边界与一致性 | ✅ | ✅ | ✅ | |
| E9 任务管理面（查询/输出/停止/三 kind 统一/无界面策略） | ✅（2026-09-12 增补） | ✅（ADR-004/006） | ✅（context §6.1/6.3） | SDR-003 |
| E14 重启孤儿治理（身份指纹/启动普查/处置策略/总寿命兜底/关闭决策点/资源可见性） | ✅（2026-10-14 增补，SDR-013；评审二轮修订） | ✅（ADR-012；评审二轮修订：调谐时序裁决、判定树三分支） | ✅（context §7，2026-10-14 增补；评审补加载调谐与超时执行者空档事实） | 处置缺省「仅报告」；keepAliveOnExit（G33）任务成为普查候选。**已落地（2026-10-15，feat/orphan-task-governance）**：US14.1–14.4 全量（指纹三元组落盘/加载调谐改普查先行/启动普查三分支判定/处置重验/总寿命 4h 兜底+单次豁免+keepAlive 默认豁免）；US14.5 关闭决策点沿用既有 stop_all 收敛+keepAlive 打标；US14.6 报告归档 `<sessions_root>/orphan-sweep/`（web 消费端待后续批）；mirri-proto 契约字段登记待后续批。**未如实登记的两处裁剪（2026-10-15 终评补记）**：① ADR-012 g（实例代际标识 epoch 落盘过滤）未落地——epoch 仅作报告元数据，并发姊妹实例共享 home 时 terminate 策略下存在理论误杀窗（缺省 ReportOnly 无杀伤力），代码级修复列为后续批首位；② US14.3 的 RPC 交互式处置入口未交付——当前处置仅经配置+重启普查触发，无运行时 RPC 干预面。**实现偏离记档**：ADR-012 c「普查器异步执行不阻塞就绪」实际实现为首 RPC 内联 await（OnceCell）——为满足 Task 3 的排序约束（普查必须先于本实例任务注册 settle），就绪延迟换正确性，属有意裁决 |
| E10 任务通知与对话闭环 | ✅（2026-09-12 增补） | ✅（ADR-003） | ✅（context §6.1） | |
| E11 定时任务（cron） | ✅（2026-09-12 增补） | ✅（ADR-005） | ✅（context §6.2） | |
| E12 界面呈现增补（面板/detach/播报/定时消息/web） | ✅（2026-09-12 增补） | 部分（呈现机制归对话渲染域，本域只定契约） | ✅（context §6.6） | |
| NFR N1–N15 | ✅（N9–N15 为 2026-09-12 增补） | ✅ | ✅ | |
| 等待语义（非阻塞契约盘点 + E13 等待与目标协同） | ✅（E13/US13.1–13.3 + N16，SDR-011 裁决 A+B 组合，2026-09-13） | ✅（ADR-008） | ✅（context §6.13，2026-09-13 增补） | 提案 P6/P7 已转正；goal 侧承接见 O11；**TS v2 已落地（2026-09-14，goal 域 E5 批次）**——US13.1（TaskWait）/US13.2（任务清单）/US13.3（挂起+看门狗）随批次落地（见 goal-ROADMAP O2 / goal-context §1.3） |

## 2. 待人工确认的开放项

| # | 开放项 | 来源 | 建议去向 |
|---|---|---|---|
| O1 | 裁决点 Q5–Q8（cron 时区、无界面默认策略、后台提问归属、会话关闭默认）需用户表态；ADR-005/006 中相关机制在表态前为对齐 TS 现状的缺省决策，表态后以新 ADR 修订 | requirements §18 | 表态后以新 ADR 承接。**进度（2026-09-12，B0）**：Q5 已由 SDR-005 裁决关闭；Q7 已由 SDR-003 裁决（归属切分）；Q6/Q8 仍开放（相关缺口核验在本程序 B1 落地，缺省决策维持） |
| O2 | ~~Rust cron 面（`cron.rs` + 三工具）与 TS 语义系统性差异：时区（UTC vs 本地）、无 jitter、无 coalescedCount、无 stale 自动删除——对齐或有意分歧待裁决~~ | context G26 | **已落地（2026-09-13，B5）**：默认本地时区 + 可选显式时区、jitter/coalesce/stale 对齐 TS（SDR-005/DEC-3）；fork 不携带补回归锁定（SDR-006）；实现事实见 context §6.2「Rust 落地」。遗留：~~idle-only 投递与 `<cron-fire>` 信封随 B2~~（已落地，2026-11 B2）、~~`/cron` 管理面随 B6~~（已落地，2026-11 B6，SDR-B6-01）与 env 族其余成员、50 上限/8KiB prompt/350 天 one-shot 校验已裁决推迟（范围外，见本表 §4 与基线计划 §1 边界） |
| O3 | ~~Rust 侧通知闭环缺失（G22/G23：终态通知作为对话轮、压缩后重注入）——是否按 ADR-003 在 mirri-core 落位待排期~~ | context G22/G23 | **已落地（2026-11，B2）**：G22/G23/G25/G27/G36 收敛（D-51/D-56/D-58/D-66），扫描式投递账本裁决落档 SDR-B2-01；实现事实见 context §6.2「Rust 落地（B2 批次）」 |
| O4 | 本域四份文档的文件名前缀为 `subagent-`，而域规范名为 `background-tasks`——是否重命名/加别名（涉及引用方：glossary、collaboration、rust-core-design） | 文件治理 | 用户裁决 |
| O5 | ~~跨域术语待注册（发起方统一合并，本域不直接编辑 glossary）~~ | **已注册（glossary v8，2026-09-12）**：定时任务（A9）、后台提问（A10）、任务通知（A11，合成 user 消息投递）、A5「后台任务」三类 kind 补充——均已登记并指向本域 spec；本条保留作注册台账 | SDR-003 | glossary.md |
| O6 | 新提案 P1–P5（cron 用户自助管理面、显式时区、通知降噪、跨会话任务总览、秒级/别名表达式）待裁决立项 | requirements §17 | proposals.md（由发起方登记）。**进度（2026-09-12，B0）**：P2 已并入 SDR-005（随 B5 落地）；P1 已随本程序 B6 落地（D-29/P-020，2026-11，SDR-B6-01）；P4 随本程序 B7 落地（D-30/D-31/P-023）；P3（P-022）与 P5（P-024）已裁决推迟（DEC-2，见 §4） |
| O7 | ~~E11/US11.4 与 ADR-005 的「fork 克隆定时任务」表述与既有跨域裁决矛盾~~ | 收口 review（2026-09-12，F-1） | **已解除（2026-09-12，B0）**：SDR-006 裁决 fork 不携带定时任务（对齐会话管理域 SDR-007），US11.4/ADR-005/glossary A9 已同步修订为终态 |
| O8 | `cron.fired` 事件与 `cron_job` origin 不区分手动触发与自然触发（B6 D-3/SDR-B6-01 的有意裁决——避免 event_projection/web projector/SDK translate/ACP 四端同步；当前零消费者）。若产品后续要求对话区/账本区分「手动触发」，需扩 origin（如加 `manual` 键）并四端同步 | SDR-B6-01（2026-11，B6） | 触发时再立 SDR；四端同步成本为已知代价声明 |
| O9 | ~~提案 P6（后台任务阻塞等待入口）与 P7（goal 续航与后台任务协调）待裁决~~ | requirements §17（2026-09-13 增补） | **已裁决（2026-09-13）**：A+B 组合采纳——等待挂起（主）+ 阻塞等待入口（辅），SDR-011/ADR-008 落档，P6/P7 转正为 E13（US13.1–13.3 + N16）；goal 域侧承接见 O11 |
| O10 | Rust HEAD 事实漂移：`agent_tool.rs` 后台派发结果文案仍为轮询指引（"Use TaskOutput to check progress and retrieve the result."，agent_tool.rs:502-525），与 TS v2 no-polling 契约相悖；no-polling 对齐修复在未合并分支 `feat/rust-tool-calling`（f51213ad，含 `suppress_notification` 参数）。合并/移植前 Rust 后台派发持续违背契约 | context §6.13（2026-09-13） | Rust 侧合并时机裁决；~~若 P6 采纳，其入口文案与本项一并处理~~（P6 已采纳为 E13/US13.1，本项随 ADR-008 落地时一并处理） |
| O11 | ~~目标（goal）域无规格落点——SDR-011/ADR-008 中目标循环侧的挂起态、看门狗、预算计费（挂起期不计回合预算）需该域承接落地~~ | SDR-011（2026-09-13） | **已解除（2026-09-13）**：goal 域四件套建立（`goal-{requirements,architecture,context,ROADMAP}.md`，SDR-001/ADR-001），等待挂起的产品语义由其 E5 正式承接；回合调度域（turn-scheduling）同期建立承载机制侧（ADR-001 统一唤醒源契约收编本域 ADR-008 机制侧）。落地排期见 goal-ROADMAP O2。**落地（2026-09-14）**：goal 域 E5 批次完成——US13.1（TaskWait）/US13.2（任务清单）/US13.3（挂起、看门狗、预算口径）全部随 TS v2 落地（commit 6c664e1d..bdce4843，见 goal-context §1.3）；goal-ROADMAP O2 已关闭 |
| O12 | 跨重启恢复的子代理无嵌套派发面（B4 评审轮 SF-5 记档）：`restore_subagents` 重建的 AgentScope 不带 `subagent_host`（其工具面无 Agent/AgentSwarm），恢复子代理可 resume 续作但不能再派发孙代理；`subagent.registered` 不携带 `nesting_depth`，重启后逐层重建嵌套 host 需按 `parentAgentId` 链推算深度。修复位置：embedded resume 装配处重建 per-child 嵌套 host + 工具注册 | SDR-B4-03 §4（2026-09-13，B4 评审轮） | 后续批次落地；rust+web 实跑（B12 R1–R3）不含该面，验收前不阻塞 |
| O13 | 子代理址通知的 journal 记录与账本跨重启错位（B4 复审二轮 F-1 记档）：子代理收件人的通知 turn 记录（`turn.prompt`/`context.append_message`/`turn.ended`）与 busy-park 的 `turn.steer` 写**主 journal**，送达/停靠账本却读**收件人**自己的记忆与 buffer——跨重启时通知重复投递、投递消息污染主代理上下文、崩溃停靠双收、`restore_task_notifications` 按主账本算 due（详见 SDR-B4-04 d）。非 B4 修复引入（pre-B4 同样错位）。修复面：通知 turn 记录经 `RoutedWireRecorder` 路由到收件人子 journal + restore 侧对账与 steer 重停按 addressee 解析（registry seed 先行，可行）| SDR-B4-04 d（2026-09-13，B4 复审二轮） | 后续批次落地；B12 rust+web 实跑（含跨重启场景）先行验证现象与验收口径 |
| O14 | ~~stall watchdog 超时（`ENGINE_TURN_STALL_TIMEOUT` 300s）无用户侧配置入口——`with_turn_stall_timeout` builder 已存在且五回合入口全量透传（2026-09-18 批次 ③），但生产构造路径（`mirri-cli/src/main.rs` 的 `from_parts`）恒用默认值，全仓库仅测试调用该 builder；config.toml/环境变量/CLI 参数均无此键。用户遇长 thinking 无流式输出的模型时无法调大。附带缺口：kosong `HTTP_READ_TIMEOUT` 与 stall watchdog 同为 300s 的赛跑关系——真静默挂死的流由 watchdog 先取消并包装成 `cancelled during llm stream`，丢失「provider 挂死」语义（read timeout 本应先触发变成可重试错误）。修复面：`[loop_control]` 加 `turnStallTimeoutMs`（参考既有 parse-only→wired 接线先例 `turn_flow_config_from`）+ read timeout 与 stall 的相对值裁决~~ | 2026-10-17（session-b05098aa 调查，变更历史同日条目） | **已解除（2026-10-18）**：`[loop_control] turnStallTimeoutMs` 落地（计划 `plan/rust-turn-stall-config.md` A/B/C 项）——config 解析（camelCase 主名 + snake_case alias，`0` = 禁用）、`from_parts` 接线（INFO 日志记生效值与来源）+ D5 告警（显式值 < 240s read timeout 时告警不拒绝）、kosong `HTTP_READ_TIMEOUT` 300s → 240s；上游同日将 `ENGINE_TURN_STALL_TIMEOUT` 默认值上调至 600s（commit `b01c980e1`，read timeout 先触发的同因裁决）进一步解除 read/stall 赛跑。用户文档见 `docs/{zh,en}/configuration/config-files.md` `[loop_control]` 节 |

## 3. 差距与回归速查（详见 context §3/§6.8）

- 既有差距矩阵 G1–G21（2026-09-09）；增量 G22–G36（2026-09-12），其中 G24（任务输出落盘）为 Rust 侧**已收敛**项的更新核实，G26（cron）系统性语义差异已于 2026-09-13 由 B5 批次对齐（见 O2）。
- 并行进展注记（2026-09-12）：Rust 侧 S0 正确性清偿批次已修复既有缺陷清单 §4 的全部 5 条（G13–G17/G19 相关，含回归测试；由该批次就地批注于 context §4）——G1–G21 中相应条目的 Rust 列以 §4 批注为准。
- 既有文档事实漂移 2 处已标注未改写：D1（配置节 `[agent_task]` → 现实现 `[task]`/`[background]`）、D2（agentTool 行号漂移）——**B0（2026-09-12）已登记为正式勘误条目生效**（context §6.0），以该节现值为准。
- B1（2026-10）：任务/子代理生命周期与安全批次落地——矩阵 G1/G6/G10/G11/G28/G31/G32/G33/G34/G35 收敛；配置面 typed 化同时纠偏了 wire 的 snake_case 透传漂移（D1 勘误的配置节现值即 `[task]`）。

## 4. 落地程序范围标注（baseline-gap-landing 程序 · 2026-09-12）

> 依据总计划 [`plan/baseline-gap-landing.md`](../../plan/baseline-gap-landing.md)（§一 DEC-2 范围排除、§二 批次定义、§四 进度跟踪）。本节只做状态与范围管理。

**已裁决推迟（本程序排除，DEC-2）**：

- cron 秒级/命名别名表达式（全局提案 P-024，即本域提案 P5）——分钟级已覆盖提醒/看护类场景，秒级徒增调度精度与解析复杂度，收益未证；SDR-005 已裁决不采纳。
- 通知分级与免打扰（提案 P3，全局 P-022）——提案推迟，待产品侧另行立项。

**落地中（本程序进行中的缺口项与批次）**：

| 批次 | 缺口项 |
|---|---|
| B1 任务/子代理生命周期与安全 | D-40（子代理超时接线）、D-45（swarm 每成员超时）、D-49（后台容量上限）、D-50（后台可用性门控）、D-62（会话关闭孤儿语义核验）、D-63（SIGTERM 宽限 + forceStop）、D-64（keepAliveOnExit）、D-65（`[task]`/`[background]` 配置面核验）、D-59（print 模式三策略）——**已落地（2026-10，B1）**：矩阵 G1/G6/G10/G11/G28/G31/G32/G33/G34/G35 全部收敛（context §3/§6.8/§6.10），SDR-007/008/009 落档，评审轮补录 SDR-010（容量口径/拒绝顺序/问题任务停止语义） |
| B2 通知闭环 | D-51（终态通知模型侧闭环）、D-56（压缩后活跃任务重注入）、D-58（后台提问）、D-66（question 类任务）——**已落地（2026-11，B2）**：G22/G23/G25/G27/G36 收敛；cron idle-only 投递与 `<cron-fire>` 信封（O2 遗留项）同批落地（SDR-B2-01 记扫描式账本裁决；实现事实见 context §6.2「Rust 落地（B2 批次）」） |
| B3 swarm 调度器 | D-42（起步限流斜坡）、D-43（429 限流重试）、D-44（结果三元状态）、D-52（成员描述）、D-53（排队项终态事件）、D-54（spawned 节奏）、D-60（resume_agent_ids 逐成员指令映射）——**已落地（2026-11，B3）**：G3/G4/G5/G18/G19（后半）/G20 收敛、G29 勘误 + 回归锁定；`subagent/swarm_batch.rs` 调度器（斜坡/取消三分/requeue/容量状态机）+ `TurnFlow::retry_one_turn` + RunningGuard 兜底终态；SDR-B3-01/02/03 落档；实现事实见 context §6.11；A12（rust+web 实跑 7 成员）留 B12 |
| B4 子代理深度与独立记录 | D-46（嵌套子代理，即 G7/裁决点 Q2 的落地）、D-47（继承面）、D-48（思考档位）、D-61（摘要提炼 summaryPolicy）、D-55（子代理独立 wire journal）——**已落地（2026-09-13，B4）**：G7/G8/G9/G21/G30 收敛；host 去 block_on 死锁重构（SDR-B4-01，parking_lot send_guard + tokio Mutex + drop 即取消语义变更）、journal 路由穷举 match（SDR-B4-02）、嵌套深度护栏 maxNestingDepth=5/0（SDR-B4-03）、自驱动 executor 桥接记档（SDR-B4-04）；B2/B3 面嵌套回归锁定（通知送达派发子代理、嵌套限流重入队）；**评审轮修复（双 reviewer 收口）**：子代理 journal 恒 append 收养不截断 + resume/续问 turn 落完整括号（SDR-B4-02 补记）、通知收件人改派发者语义（活跃注册表 subject→parent 解析 + 每收件人 due 过滤 + host busy 探测，SDR-B4-04 修订）、`[subagent] maxNestingDepth` 配置面接线（SDR-B4-03 f 项）、distill panic 隔离与 Drop 关停加固；恢复子代理无嵌套派发面记 O12；rust+web 实跑（两层真 provider、磁盘核验、跨重启 resume）留 B12 |
| B5 cron 语义对齐 | D-57 / P-021（时区/jitter/coalesce/stale，SDR-005/SDR-006 裁决落地）——**已落地（2026-09-13）** |
| B6 cron 管理面 | D-29 / P-020（cron.delete/cron.trigger RPC + web 管理 UI）——**已落地（2026-11，B6）**：两 RPC 五环（registry `both_kept` 213/198/188/173、TSV、EMB 路由、NAPI 通用、fixture）+ cross_backend cron 域首例；run-now 不动日程与无 manual 标记落档 SDR-B6-01；实现事实见 context §6.2「Rust 落地（B6 批次）」；web 面板与 `/cron` 命令由 B6 web 轨落地（管理面不含创建——创建仍走模型工具 CronCreate，D-6） |
| B7 web 任务总览与转后台 | D-30、D-31 / P-023 |

## 5. 变更历史

- 2026-10-14（重启孤儿治理设计批次）：起因 agent 后台发起的全量 rust 测试在服务重启后遗留无主进程、持续占用机器资源数十小时的实发事故（详见 context §7 触发事件）。requirements 增补 Epic E14（US14.1–14.6：进程身份指纹、启动普查、处置策略与安全边界、总寿命兜底、关闭决策点、资源可见性）+ SDR-013（指纹三元组 / 普查仅报告缺省 / 总寿命上限缺省 4h）；architecture 增补 ADR-012（普查器、指纹校验、跨平台启动时刻来源、组长死亡边界）；context 新增 §7 快照；glossary 同批注册 A15（进程身份指纹）。「退出保留」（keepAliveOnExit，G33 既有配置）任务成为普查候选；Q8 的 b 路线如未来裁决通过同走此衔接（本次不预裁 Q8）。
- 2026-10-15（E14 落地批次，feat/orphan-task-governance）：8 任务实施（每任务实现+评审+修复轮，全程 TDD）。落地物：`mirri-os` 指纹探测/组枚举/重验组终止 API（macOS libproc + Linux /proc）；`AgentTask.fingerprint` 档案落盘 + 带指纹 Running 档案不再被加载调谐（普查先行）；bash 双路径采集（后台直启+前台转后台）；`[task]` 配置 `orphanDisposePolicy`/`maxTaskLifetimeS`；`orphan_sweep.rs` 三分支判定（LiveOrphan/已死含 pid 复用/组内残留）+ 报告归档 + ReportAndTerminate 重验处置；embedded 装配（首 RPC 前完成 census——OnceCell 收紧并发、60s 磁盘寿命清扫经 spawn_blocking）；`lifetime_exempt` 单次豁免 + keepAlive 关闭路径默认打标豁免。遗留（ledger）：once-guard 重试语义、4h 缺省值产品确认、首 RPC 延迟实测、`!` 命令路径指纹采集、walk glob 收窄、web/desktop 呈现面与 mirri-proto 契约登记（消费端批）。

- 2026-09-09：v1 基线三件套（requirements/architecture/context）建立；SDR-001/002、ADR-001/002。
- 2026-09-12：第 2 轮侦察增量——SDR-003/004 扩域与语义增补（任务管理面/定时任务/后台提问；摘要可用性与续作名单映射）、E9–E12、N9–N15、Q5–Q8、ADR-003~007、context §6（增量快照 2026-09-12）、新建本 ROADMAP；评审两轮（design + architecture）修复至零 comments。
- 2026-09-12（B0 治理批次）：SDR-005（cron 时区/防扎堆/过期口径，Q5 关闭）、SDR-006（fork 不携带定时任务，O7 解除）落档；requirements Q2「单层=现状」口径勘误（TS 基线可嵌套，单层为 Rust 差距 G7，协作域 O11 同步解除）；context D1/D2 勘误条目生效；ADR-005 fork/时区表述定案修订；新增 §4 落地程序范围标注。
- 2026-10（B1 任务/子代理生命周期与安全）：D-40/45/49/50/59/62/63/64/65 落地（矩阵 G1/G6/G10/G11/G28/G31/G32/G33/G34/G35 收敛，实现事实见 context §6.10）；SDR-007（persisted-killed ≡ lost，不新增 wire 状态）、SDR-008（双层宽限）、SDR-009（print 缺省内存覆写）落档；`session.waitForBackgroundTasksOnPrint`/`session.handlePrintMainTurnCompleted` 由 not_implemented/恒 continue 转为实现（TSV 注记同步）；评审轮补录 SDR-010（容量口径扩展到全部 detached 生产者、spawn 前容量预检、问题任务无信号即时停止、steer 软上限文档化）。
- 2026-09-13（等待语义批次）：起因 goal 模式轮询循环的 token 空耗。context 新增 §6.13 等待语义专项盘点（TS v2 无阻塞等待入口 / v1 休眠 block 参数与描述矛盾 / Rust HEAD 存活 block+timeout / goal 续航 prompt 与任务系统零交互的结构根因）；requirements §17 增补提案 P6（阻塞等待入口）与 P7（goal 续航任务协调）；本表新增 O9/O10。
- 2026-09-13（等待语义裁决）：用户裁决 A+B 组合——SDR-011（等待挂起为主 + 阻塞等待入口为辅 + 缓解件随手带上）落档；P6/P7 转正为 Epic E13（US13.1–13.3）+ N16；ADR-008 承接技术形态（等待参数复用 v1/Rust 先例、批量=并行等待、挂起=续航暂停发放、唤醒=通知自起新回合零合并机制）；glossary 同批注册 A12（阻塞等待入口）/A13（等待挂起）；O9 关闭，新增 O11（goal 域承接落点）。
- 2026-09-13（B4 评审轮收口）：双 reviewer（code + design）各一轮，8 项 should-fix 全数处置——S1/SF-1 子代理 journal 重启后首轮 resume 截断（create→open_for_append 收养语义 + 二次重启回归）、S2/SF-2 resume/续问 turn 缺 dispatch 括号（补 `turn.prompt`/`context.append_message`）、SF-3/N4 通知收件人=派发者（活跃注册表 `subagent.registered` 实时 fold + subject→parent 解析 + 每收件人 due 过滤跨 tick 收敛）、S3 子代理收件人 busy 探测（`SubagentHost::is_child_running`）、SF-4/N3 `[subagent] maxNestingDepth` 配置面接线（拒绝文案旋钮转真实）、N1 distill panic 隔离、N2 Drop 内 tokio::spawn 关停加固、Q3 registered 落档移至 create_agent 之后；SF-5（恢复子代理无嵌套派发面）记档为 O12；随批发现并修复 B1 sweeper 测试的脚本队列次序假设（T4 异步化后父续作调用先于子首调——测试改按新确定序排列，非引擎缺陷）。
- 2026-09-13（B4 复审二轮收口）：两 reviewer 复审确认首轮全部修复正确；新发现处置——code F1 busy 探测 TOCTOU（点时探测改**原子 claim**：`SubagentHost::claim_child_notification_turn` 与 spawn/resume 同锁取-判-置 `running`，guard 持有至通知 turn 结束，含 host 级回归测试）、code F2 busy 停靠 no-op（steer buffer get-or-create，真实停靠）、code F3 deferred 窗口 envelope 不对称（envelope 决策挪到 materialize 路径——`prepend_op` 前插且幂等，exactly-one-metadata 成为结构不变量，含 deferral 测试断言）、code Q-1 busy 收件人阻塞整 tick（park 后继续锚定循环）、code Q-2/Q-3 通知 turn 身份盖章为收件人 + rsplit 解析消除；design F-1（子代理址通知的 journal/账本跨重启错位：重复投递/主上下文污染/停靠双收，**非本批引入**）记档 SDR-B4-04 d + 开放项 O13，B12 实跑先行验证；随批修复 SF-4 遗漏的 mirri-cli `SubagentConfig` 字面量初始化（全量门禁发现——此前 clippy 未覆盖该 crate，本轮起最终门禁恒为 workspace 全量）；随批修复全量门禁暴露的 `message.send` 默认寻址竞态（T8 起 `restore_subagents` 往同 agents map 加子代理后，「session 的第一个 agent」= HashMap 随机遍历序，~50% 进程落到子代理上报 `Tool "Agent" not found`——`list_agents` 改确定性排序 main 恒首 + 回归测试，原 flaky 测试 20/20 稳定）。
- 2026-09-14（goal 域 E5 落地联动）：等待语义的 goal 侧协同实现落定——US13.1（TaskWait 阻塞等待入口）/US13.2（续航提示任务清单）/US13.3（挂起、看门狗、预算口径）随 TS v2 批次落地（goal 域 commit 6c664e1d..bdce4843）；本域需求契约不变（E13 为终态需求文档，无实现状态）；落地事实见 goal-context §1.3，goal-ROADMAP O2 已关闭。
- 2026-09-14（会话审计 30c0d8bb 修复批次）：P0-1（子代理审批通道接线，ADR-009 + 修订记录 5/6）/P2-2（任务终态携带 stop_reason，ADR-010）落地——修复 `20a15edd` + serve 面镜像继承 `a8264c99` + 复审整改 `8506aba4` + 终态映射 `eb1a01d8`；验收期发现并修复 serve wait 模式审批死锁（既有缺陷，反向 RPC 投递移出连接循环，`bcfb3ed4`，ADR-009 修订记录 6——含 wait 模式同连接并发语义由串行变并行）；处置总账与验收证据见 [docs/specs/reports/rust-v3-session-30c0d8bb-audit.md](../reports/rust-v3-session-30c0d8bb-audit.md) §六；followup（并发上限/超时统一/断线恢复语义/审批 agent 身份）见该报告 §6.4。
- 2026-09-18（孤儿缺陷修复，用户报告 session-16f65edc）：stalled 回合孤儿化前台子代理——根因链：子代理 LLM 请求挂死（provider 无超时）→ 父回合 stall watchdog（300s 无进展）drop 回合 future → 前台 `task.start` 驱动器内联于工具执行 future、随之消亡，跟踪任务永驻 running（直到 2h 子代理超时），task driver 每 tick 重复 stale 告警。修复：① `agent_tool.rs` 前台路径加 `ForegroundOrphanGuard`（drop 时 fire 子 cancel token + `tasks.stop` 落定，first-terminal-wins 保护既有落定）——刻意不 fire 回合级 token（会误杀本应超越回合存活的 background 子代理）；② task driver stale 告警改边沿触发（每任务一次，不再每 tick 重放）；③ stall 超时可配置化（`with_turn_stall_timeout`，测试注入亚秒值；wait/fire/steer/cron/notification 五个回合入口全量透传）；红→绿集成用例 `should_cancel_the_foreground_subagent_when_the_parent_turn_stalls_out`（gated LLM 复现挂死形态）。遗留（B 方案域）：provider 层无请求超时——挂死的 LLM 调用本身只能靠 TCP 兜底，另行立项。**后续解除（2026-10-10）**：commit `776d2bb86`（fix(kosong): bound provider HTTP calls with connect and read timeouts）落地 `HTTP_CONNECT_TIMEOUT = 30s` + `HTTP_READ_TIMEOUT = 300s`（`mirri-kosong/src/provider.rs`），该遗留项关闭。
- 2026-10-17（stall watchdog 调查，session-b05098aa）：用户报「首 token 超 5 分钟被杀」存疑。核验结论：turn 17 被 rust 引擎 stall watchdog 取消（16:00:50 step.begin 后 300s 无任何 LiveEvent，16:05:50 协作式取消）；watchdog 量为引擎静默时长而非首 token 等待（任一 LLM delta/工具事件/重试即重置）。日志显示同日 13 次 watchdog 触发、跨 6 session（前两日 11/4 次），提示 provider 端点静默挂死为模式性现象而非孤例；且 `HTTP_READ_TIMEOUT`（300s）与 stall watchdog 同值赛跑，watchdog 恒先赢，`cancelled during llm stream` 掩盖了 read-timeout 语义。缺口登记为 O14（stall 超时用户侧配置化 + read/stall 相对值裁决），计划 `plan/rust-turn-stall-config.md`。
- 2026-10-17（stall 默认值 300s → 600s，用户裁决）：`ENGINE_TURN_STALL_TIMEOUT` 上调至 600s（`message_handlers.rs`），**顺带解除 O14 的 read/stall 赛跑**——`HTTP_READ_TIMEOUT`（300s）从此恒先于 watchdog 触发，静默挂死的 provider 流先以可重试的 stream error 进入重试循环（重试发 `StepRetrying` live 事件，天然重置 watchdog deadline），watchdog 退居引擎侧兜底。O14 剩余范围收窄为配置化入口（`turnStallTimeoutMs`）本身。全量注释同步：ask_user/agent_tool/mirri-acp 的 300s 字面量引用改为不写死数值，避免下次再调值漏改。
- 2026-10-18（O14 收口，stall watchdog 配置化落地）：`plan/rust-turn-stall-config.md` A/B/C 三项落地——A：`[loop_control] turnStallTimeoutMs`（`LoopControlConfig::turn_stall_timeout_ms`，camelCase 主名 + snake_case alias；absent = 引擎默认，`0` = 禁用 watchdog，对齐 `[subagent] timeout_ms` 惯例）；B：`from_parts` 接线（构造期一次性读取，不热更新）+ INFO 日志（生效值与来源：配置/引擎默认）+ D5 告警（显式值 < provider read timeout 时告警不拒绝）；C：kosong `HTTP_READ_TIMEOUT` 300s → 240s（计划 D4，read timeout 先触发→可重试错误，watchdog 退居兜底）。上游同日 `b01c980e1` 将 `ENGINE_TURN_STALL_TIMEOUT` 默认值 300s → 600s（read timeout 240s 先触发的同因裁决）——默认值演进：计划原设 300s，落地时以 600s 为准。用户文档：`docs/{zh,en}/configuration/config-files.md` `[loop_control]` 节增该键说明与示例。O14 关闭。
