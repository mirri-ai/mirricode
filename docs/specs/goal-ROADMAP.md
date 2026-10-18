# 目标域 — ROADMAP（状态与开放项）

> 本文件是本域开放项与执行状态的唯一维护点（按 `docs/specs/AGENTS.md` 治理约定）；需求/设计/现状分别住在 [goal-requirements.md](goal-requirements.md)、[goal-architecture.md](goal-architecture.md)、[goal-context.md](goal-context.md)。

## 1. 文档覆盖状态

| 能力面 | 需求 | 架构 | 现状盘点 | 备注 |
|---|---|---|---|---|
| E1 创建与校验（单目标/replace/审批） | ✅（v1 基线 2026-09-13） | ✅ | ✅ | |
| E2 生命周期四态（暂停来源族/阻塞/完成瞬态/恢复） | ✅ | ✅ | ✅ | 3-turn 复现为提示词级（context §1.1 陷阱） |
| E3 预算（三单位/记账/耗尽/宽限回合/剩余） | ✅ | ✅ | ✅ | |
| E4 续航循环（结束即续航/步数上限变体/防重入/空续步收尾/提醒注入） | ✅ | ✅ | ✅ | |
| E5 等待挂起（承接 SDR-011） | ✅（承接自 subagent 域 E13/US13.3） | ✅（ADR-001 机制切分） | ✅ | 已落地 TS v2（2026-09-14，commit 6c664e1d..bdce4843，四步 B→清单→挂起→看门狗全落地；落地事实见 context §1.3，机制裁决见 ADR-002） |
| E6 消费端（终端命令族/面板/完成卡片/队列/web/无界面） | ✅ | ✅ | ✅ | 队列为终端客户端资产（提案 P2） |
| NFR N1–N7 | ✅ | ✅ | ✅ | |

## 2. 待人工确认的开放项

| # | 开放项 | 来源 | 建议去向 |
|---|---|---|---|
| O1 | 阻塞"连续回合复现"的运行时强制（现状纯提示词自律，context §1.1；裁决点 Q1 倾向维持自律） | requirements P1 | Rust 侧可直接超出基线实现；TS 侧不回改 |
| O2 | ~~等待挂起（E5）落地排期——含续航提示任务感知、空转判定、看门狗时长定标（Q2）、预算计费口径、与 turn-scheduling 挂起接口对齐。建议顺序（2026-09-13 对齐裁决时的讨论）：阻塞等待入口（B，v1/Rust 参数先例，成本最低）→ 续航提示携带任务清单 → 空转判定挂起 → 看门狗——每步独立可用、独立可验证。过渡措施（2026-09-13）：E5 落地前，续航提示与活跃提醒已加入后台等待指引（勿轮询、空转收尾、勿为等待报 blocked、可并行前台派发），空转成本从轮询级降为回合级；其"无工具调用收尾"行为即未来挂起判定的触发信号，与 E5 无缝衔接~~ | SDR-001/ADR-001 | **已关闭（2026-09-14，TS v2 落地批次）**：四步顺序全部落地——阻塞等待入口 TaskWait（US13.1）、续航提示携带任务清单（US13.2，上限 20 + 截断标注）、空转判定挂起（US13.3，保守版判定 + onDidExecuteTool 观察点）、看门狗（GOAL_WAIT_WATCHDOG_MS，Q2 定标 2026-09）；过渡措施文案（2026-09-13 stopgap）已随正式机制收敛（GOAL_CONTINUATION_PROMPT 等待指引块 / goal-active-reminder.md / [background-tasks] 指引行，bdce4843）；落地事实见 goal-context §1.3，机制裁决见 goal-architecture ADR-002 |
| O3 | upcoming goals 队列引擎化（提案 P2）——现状 TUI 客户端本地文件，web/无界面无对等能力 | requirements P2 | 裁决立项 |
| O4 | Rust 侧事实：goalId 硬编码 "goal-1"、wire `completed`/快照 `complete` 双轨拼写、无 pendingContinuation 重入锁、续航层双接入位（F11 默认记账-only） | context §3 | Rust parity 批次 |
| O5 | 无界面模式仅支持 create 子命令（pause/resume 等回落普通 prompt）——是否补齐待裁决 | context §1.7 | 触发时再裁决 |
| O6 | 无预算时的预算引导失真：`budget_guidance` 每新回合必注入，未设任何预算时也宣称 "you are within budget"（goalInjection.ts:34-37,63,97-113）——requirements US4.3 已按诚实终态收紧（无预算不注入预算引导）；TS 侧不回改，Rust 落地对齐终态 | 本轮评审（G-2，2026-09-13） | Rust 落地批次对齐终态 |
| O7 | 挂起触发条件的精化版：现采保守版"本回合无任何工具调用 + 有后台任务在跑"（2026-09-13 裁决）；已知盲区为回合内轮询与只读状态查询型回合（不触发挂起，由阻塞等待入口+动态劝阻兜底，见 context §1.3 盲区表）。精化方向：只读状态查询（任务状态/目标查询）不算实际工作——覆盖更全但引入工具分类白名单。触发条件：保守版+劝阻+B 三者上线后，状态查询型空转残留仍显著时升级 | 本轮分析（2026-09-13，as-is/to-be 对比评审） | 观察后裁决；升级时修订 ADR-008 与 E5 判定。**进度（2026-09-14）**：保守版判定已按现状落地并有测试锁定（工具活动观察点 'goal-turn-tool-activity' 含被否决/合成调用，goalService.ts:429-437）；精化升级条件不变 |
| O8 | cron 注入回合不随 goal paused 冻结（goal-stop-freeze SDR-002 c 的范围裁决）：cron 是用户安排的定时机制，独立于目标机器，暂停只冻结目标驱动的活动（续航/通知投递）。若未来要求"paused 期间用户定时提醒也静音"，需再裁决（产品语义：暂停是否冻结用户自己的定时安排） | goal-stop-freeze 批次（2026-09-14） | 触发时再裁决；SDR-002 c / ADR-003 c 已固定现范围 |
| O9 | Rust `UpdateGoal(active)` 可从 `Complete` 回 `Active`（门禁只拦终态转移于非 active）——TS 中 complete 为瞬态随即 `clear`，无复活路径；Rust complete 常驻至 clear | goal-stop-freeze 批次（2026-09-14，context §3.1） | Rust parity 后续批次裁决：拒绝从 complete 复活（对齐 TS 无此路径）或明示允许 |
| O10 | `UpdateGoal` 门禁为 check-then-act 非原子：守卫检查与状态转移之间可插入状态变化（如恰好同时到达的暂停）——已接受为时序窗口（SDR-002 d），若需严格原子性（锁/版本号）再升级 | goal-stop-freeze 批次（2026-09-14，context §3.1） | 已接受边界；触发时再升级 |
| O11 | 被取消/停止的 turn（含子代理任务）遗留未决审批请求：goal.pause 冻结实测中，子代理被停止后其 pending 的 `Write`/`Bash` 审批卡残留 UI（activity 停留 awaiting-approval、会话列表 Approve 徽章不消失），需手动拒绝/批准才清——审批域生命周期与 turn 取消的联动（取消时应撤回/作废该 turn 发起的 pending 审批） | goal-stop-freeze web 验收（2026-09-14，session-7c871cca 实测） | 审批域（approval-architecture）批次裁决 |
| O12 | ~~TS 在 turn `failed`（`pauseActiveGoal` + `goalFailurePauseReason` provider 错误映射）与 `filtered` 时也 pause goal（turn/index.ts:450-455）；Rust Task 6 只覆盖取消路径——failed/filtered 路径的 pause parity 待裁决~~ | goal-stop-freeze 批次 review（2026-09-14） | **已解除（2026-10-18）**：goal 异常终局批次（SDR-003/ADR-004）在 rust 落地全路径结算——failed/stalled/aborted/hook-blocked 一律经 `settle_goal_after_turn` 结算（failed→paused 带失败文案，stalled→paused，aborted→paused，hook→blocked）；`filtered` 按 TS `handleTurnEnded` 语义归入正常收尾（不结算、可续航），与 TS 对齐。provider 错误细分类文案（rate limit/auth/api）为已知差距，统一 `Paused after turn failure: {error}` |
| O13 | subagent host 的 child turns 未注册 `in_flight_turns`：子代理 turn 运行中，session 级/turn 级 `message.abort` 均 `ok:true` 空转（复测实测：demo-a child turn 运行中两次 abort 无效果）——task/cron driver 注入回合已注册（本批修复），但 subagent host 在 mirri-core 层 spawn 的 child turn 无注册路径。用户场景：主 turn 结束/死亡后子代理仍在跑，"停止 Turn"对其无效 | goal-stop-freeze 复测验收（2026-09-14，session-791cea80 实测） | 后续批次裁决：child turn 注册进 in-flight 表或提供子代理取消通道（注意与 ADR-003 "pause 不杀后台任务进程"的范围边界——abort 与 pause 语义不同，abort 应可取消） |
| O14 | ~~主 agent fire turn 的 300s `ENGINE_TURN_TIMEOUT` 静默死亡后 goal 仍 Active 且无 turn 在跑：goal 循环完全依赖后续通知注入重启，窗口期内"goal 在跑"实为空转等待；turn.failed 后 UI spinner 清除正常，但 goal 状态与实际活动脱节的窗口期呈现待裁决~~ | goal-stop-freeze 复测验收（2026-09-14，session-791cea80 实测） | **已解除（2026-10-18）**：goal 异常终局批次（SDR-003/ADR-004）双管齐下——a) LlmPending 在途心跳（897688fd8）与 fire/steer coerce（8dad7b2df）消除误杀与落盘语义缺口；b) 异常死亡 turn 现在立即把 goal 结算为 paused（带类型化 reason），跨回合续航接管正常路径的续跑；配套 `[loop_control] turnStallTimeoutMs` 配置化 + 看门狗默认 600s + kosong read timeout 240s（provider 可重试错误优先） |

## 3. 变更历史

- 2026-09-13：建域四件套（SDR-001/ADR-001；用户裁决与 turn-scheduling 域同期建域、机制/产品分离）。等待挂起产品语义由本域 E5 正式承接（关闭 subagent 域 O11）。现状盘点基于 v2 `agent/goal/` 全量勘察 + v1/Rust 对照（快照 2026-09-13）。
- 2026-09-14：E5 等待挂起落地（TS v2，commit 6c664e1d..bdce4843）——TaskWait 阻塞等待入口 / 续航提示携带任务清单（上限 20）/ 空转判定挂起（保守版观察点）/ 看门狗（10 分钟），等待文案由 stopgap 收敛为正式机制版；O2 关闭、O7 进度更新、context §1.3 现状同步、ADR-002 落档（goal-architecture）。
- 2026-09-14（2）：**goal-stop-freeze 批次（Rust，commit f37a5a24..1200e57d）**——UpdateGoal 终态门禁（模型不可覆盖用户 paused、`paused` 移出模型输入）、task/cron 注入回合注册入 `in_flight_turns`（`message.abort` 可取消、计入 busy 派生）、`goal.pause`/`goal.cancel` 冻结机器（共享取消内核 + 通知投递门，cron 不 gate）、`goal.resume` 恢复投递；SDR-002（requirements）与 ADR-003（architecture）落档、context §3.1 现状同步、O8–O10 新增（后续批次裁决）。
- 2026-09-14（3）：**goal-stop-freeze web 实测验收**——Pause/Resume/Interrupt 三场景通过 Playwright + rust server 实测（Pause 冻结 3.5 分钟零新 turn、Resume 恢复挂起通知投递、Interrupt 真取消通知注入 turn）；验收发现缺口四"turn 取消后 goal 未环停"（取消后 63ms 任务驱动续跑至 turnsUsed=58，TS `pauseOnInterrupt` parity 缺失）——Task 6 修复（取消内核内 pause-on-interrupt，ADR-003 f、context §3.2），O11（陈旧审批残留）新增。
- 2026-09-14（4）：**Task 6 复测验收通过 + 新开放项**——新 goal fire turn 运行中 turn 级 `message.abort`：wire `goal.update {actor=user, status=paused, reason="Paused after interruption"}` 于 abort 后 ~3ms 先于结算落盘、UI GoalStrip `Active → Paused`、按钮切 Resume、90 秒零新 turn（细节见 context §3.2 复测段）；复测发现 O13（subagent host child turns 未注册 in-flight，abort 空转）、O14（300s 引擎超时静默杀死 fire turn 后 goal Active 空转等待通知重启），O11 审批残留再现、turn.failed 后 UI spinner 清除正常。

- 2026-10-18：**goal 跨回合续航 + 异常终局结算批次（Rust，goal-continuation worktree squash）**——异常终局（stalled/aborted/failed/hook-blocked）立即结算 goal（paused/blocked + 类型化 reason，5 结算点直调 `settle_goal_after_turn`）；跨回合自动续航（`maybe_launch_goal_chain` 判定链 + 链式 spawn task 注入 `goal_continuation` turn）；双护栏（idle 3 / 绝对 20，用户 turn 重置）；watchdog 配套（turnStallTimeoutMs 配置键、600s 默认、read timeout 240s 先触发）。SDR-003（requirements）/ADR-004（architecture）落档；O12、O14 关闭。
