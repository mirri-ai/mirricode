# 否定窗测试审计（Deterministic Negative Windows — 前置审计）

日期：2026-11-05 ｜ 基线：`test/sleep-governance` @ `e788ac9d` ｜ 目的：在改造前判定「这些测试是否必要、是否必须这么测」

**结论先行：** 49 个 b2 测试里只有 1 个严格冗余可删、1 对可合并、1 对可合成一条，其余 43 条各自锁定一个独立分支。**测试数量基本合理，问题在测试手法**（200–300 行的整机重启装置 + 以 LLM 调用次数代理行为 + 固定时间窗）。所以本轮改造的收益来自**降低测试成本与 flaky**，不是减少测试数量。

这条结论修正了上一轮的两个判断：

1. 我口头估计"约 15–19 个该删"——那是按文档标题猜的。逐条读实现后不成立：这些测试分别锁住 busy/idle/wakeup/resume/suppress/cron/goal 的不同分支，删掉就丢覆盖。
2. 本审计初稿又把 #22、#25、#41 列为合并对象，同样撤回（见 §2 末尾的修正记录）。

唯一真正便宜的部分是 **#15**：它与 #5 的装置逐字相同，断言是 #5 的严格子集。

---

## 0. 站点与测试的对应关系

| 文件 | 注解站点 | 归属测试数 |
|---|---|---|
| `tests/b2_notification_tests.rs` | 41 | 33 |
| `embedded_server/tests.rs` | 18 | 18（含 1 个 helper `assert_plugin_server_absent_for_a_while`） |
| `tests/cron_ws_4b_tests.rs` | 4 | 4 |
| `tests/t_session_consistency_tests.rs` | 3 | 2 |
| `mirri-core/src/subagent/swarm_batch.rs` | 6 | 5 |
| `tests/cron_ws_4a_tests.rs` / `tests/b1_print_mode_tests.rs` / `skill_watcher.rs` / `orphan_governance.rs` | 1 各 | 1 各 |
| `mirri-core/src/subagent/host.rs` / `mirri-test-harness/tests/mock_llm_tests.rs` / `mirri-acp/tests/conformance.rs` | 1 各 | 1 各 |
| **合计** | **79** | **65** |

---

## 1. 关键发现：最便宜的 seam 是可见性，不是计数器

上一版计划打算在生产代码里加 5 个 `AtomicU64` 计数器。**不需要。** driver 的循环体和 resume 对账本来就是独立的自由函数，测试可以直接调用它们：

| 目标 | 现有形态 | 改造 |
|---|---|---|
| 通知投递一个 tick | `task_driver.rs:221` `async fn deliver_due_notifications(19 args)` | `pub(super)`（纯可见性） |
| 会话 cron 一个 pass | `cron_driver.rs:741` `async fn schedule_session_cron_pass(...)` | `pub(super)` |
| 工作区 cron 一个 pass | `cron_driver.rs:123` `async fn schedule_workspace_cron_pass(...)` | `pub(super)` |
| resume 通知对账 | `session_handlers.rs:1713` `async fn restore_task_notifications(&self, session, id)` | `pub(super)` |
| MCP 启动完成 | `McpStartupTiming.outstanding/finished` 已在生产 | 加 `#[cfg(test)] fn is_settled()` + accessor（无新字段） |

测试侧只需要一个 helper 从 `EmbeddedServer` 的私有字段拼出 19 个参数（`embedded_server::tests` 是 `embedded_server` 的后代模块，可直接读私有字段——`cron_ws_4b_tests.rs:29` 已经在写 `server.llm_factory`，先例成立）。然后：

```rust
// "六个 tick 没有新通知" 变成六次真实的 adjudication
for _ in 0..6 {
    drive_notification_tick(&server, &session).await;
}
assert_eq!(occurrences, 1, "exactly one delivery");
```

**收益对比：**

| 方案 | 生产改动 | 窗口消除 | 确定性 |
|---|---|---|---|
| 计数器（上一版） | 5 个 `AtomicU64` + 6 个 accessor + ctor 初始化 | 需保留"等够 N 个 tick"的等待 | 是 |
| 可见性（本审计） | 4 个 `pub(super)` | 窗口整行删除 | 更强：每次调用都是**已完成的 adjudication** |

可见性方案还更精确：原来的 "6 ticks" 包含 sweep + terminate_expired + stale warn 等与断言无关的部分；直接驱动后测试字面就是"6 次投递裁决"。

**代价（必须诚实说明）：** 直接驱动后，这个测试不再证明「spawned driver loop 会按 timer 调用它」。需要保留 2 个测试跑真实 spawned loop 作为装配烟测（§2 的 `KEEP-LOOP` 标记：#3、#9）。

---

## 2. b2 逐条判定（49 条）

判定口径：
- **KEEP** — 不变量独立且真实，保留（窗口按 §1 方式消除）
- **KEEP-LOOP** — 同上，但必须跑真实 spawned loop（装配烟测）
- **MERGE** — 断言被另一条测试覆盖或只差一个参数分支，合并
- **REWRITE** — 不变量保留，改成直调 `restore_task_notifications`（省掉整机重启装置）
- **DELETE** — 严格子集，无独立覆盖

| # | 测试 | 行数 | 窗口 | 判定 | 依据 |
|---|---|---|---|---|---|
| 1 | `report_the_session_in_flight_while_a_notification_turn_runs` | 39 | 0 | KEEP | 只有此测覆盖"transport 读面 union driver 注入的 turn" |
| 2 | `publish_agent_status_frames_when_a_notification_turn_runs` | 50 | 0 | KEEP | driver 自建 flow 的 publisher 接线，用户 turn 测试覆盖不到 |
| 3 | `deliver_a_new_notification_turn_when_the_agent_is_idle` | 75 | 0 | KEEP-LOOP | 核心 happy path，且证明 driver 会被 timer 驱动 |
| 4 | `fire_notification_hook_when_a_due_notification_delivers` | 80 | 0 | KEEP | Notification hook + `sink: "context"` 形状 |
| 5 | `not_deliver_a_duplicate_notification_on_later_ticks` | 53 | 1 | KEEP | 幂等核心（同时是 #15 的超集）。装配证明由 #3 提供——它已断言 spawned loop 按 timer 投递；本测试要证的是「后续裁决不再投递」，直驱恰好测到这一点 |
| 6 | `fold_the_notification_into_the_running_turn_when_busy` | 143 | 1 | KEEP | mid-turn admission + `turn.steer` 先于 `steer_flushed` 的顺序契约 |
| 7 | `deliver_the_parked_notification_as_its_own_turn_when_the_turn_stops_naturally` | 101 | 1 | KEEP | final-summary 隐藏缺陷的回归锁 |
| 8 | `convert_parked_notifications_into_a_turn_when_idle` | 97 | 0 | KEEP | stranded 救援路径 |
| 9 | `exit_the_driver_when_the_session_disappears` | 30 | 1 | KEEP-LOOP | 唯一覆盖 loop 退出（无 panic/无残留）；只能靠真 loop |
| 10 | `not_notify_when_the_task_was_stopped_by_taskstop` | 62 | 1 | KEEP | TaskStop 抑制 + `notification_suppressed` 落盘 |
| 11 | `suppress_notifications_for_tasks_killed_at_session_close` | 106 | 1 | KEEP | close 抑制（TS stopAllOnExit parity） |
| 12 | `suppress_notifications_when_print_mode_drains` | 93 | 1 | KEEP | print drain 抑制，独立分支 |
| 13 | `silently_reappend_undelivered_notifications_on_resume` | 204 | 1 | **REWRITE** | 核心不变量（delivered vs undelivered 的区分）保留；装置可缩到直调 `restore_task_notifications` |
| 14 | `redeliver_the_notification_after_the_turn_is_undone` | 125 | 1 | KEEP | 真 `session.undo` → ledger → 重投的端到端；纯函数测不到 |
| 15 | `keep_the_notification_delivered_when_nothing_is_undone` | 44 | 1 | **DELETE** | 严格子集：装置与 5 完全相同，断言（`sightings == 1`）弱于 5 的（`occurrences == 1 && chat_call_count == 2`） |
| 16 | `not_redeliver_a_notification_when_compaction_drops_it_from_context` | 198 | 2 | KEEP | 持久 marker 胜过扫描 ledger；T0 只测谓词，这里测真 compaction 重写 |
| 17 | `not_create_an_llm_on_ticks_after_the_delivery_settles` | 88 | 1 | KEEP | 声称"空转 tick 的成本可退"，断言的是 LLM **创建**数，与 5 不同 |
| 18 | `hold_the_cron_fire_until_idle_and_inject_it_enveloped` | 114 | 1 | KEEP | busy → hold → idle 注入 + envelope |
| 19 | `not_inject_a_held_fire_when_a_recurring_task_is_deleted_mid_turn` | 85 | 2 | **MERGE → 20** | `is_delete_requested` 是纯集合查询（`cron.rs:1370`），19/20 测的是同一条 purge 路径；两个分支与 `remove()` 的 true/false 已由 T0 `should_purge_a_held_fire_when_its_task_is_delete_requested`（4033）、`should_not_purge_...`（4083）、`should_remove_task_and_report_success_...`（3932）覆盖。20 是判别性更强的一支（文档自述 naive 实现在此失败） |
| 20 | `not_inject_a_held_one_shot_fire_when_its_task_is_deleted_after_firing` | 88 | 2 | KEEP | 文档自述"naive get()-filter 恰好在此失败"，是判别测试 |
| 21 | `leave_a_mixed_buffer_untouched_until_the_next_turn` | 132 | 2 | KEEP | 混合 buffer 的 FIFO 作用域规则 |
| 22 | `inject_the_enveloped_prompt_when_idle` | 62 | 0 | KEEP | 独有断言：**durable** `turn.prompt` 的 `origin.kind == "cron_job"` 与 envelope 前缀（可重放性）。18 只断言 model input，测不到落盘 provenance |
| 23 | `deliver_the_answer_when_a_background_question_completes` | 97 | 0 | KEEP | AskUserQuestion 后台回路闭合，独立工具 |
| 24 | `deliver_a_notification_to_the_owning_agent_when_a_nested_subagent_finishes` | 131 | 1 | KEEP | addressee = 注册表父链，不是 subject |
| 25 | `deliver_a_process_notification_to_the_subagent_that_dispatched_it` | 103 | 1 | KEEP | `task_dispatcher` 的两臂**归属来源不同**：Agent 臂走 `session.subagent_parent(subject)`（含 `parent == main → None` 短路），Process 臂直接读 `task.dispatcher`（`task_driver.rs:1054-1070`）。共享的只是 liveness fallback |
| 26 | `not_redeliver_a_subagent_notification_after_the_session_is_resumed` | 216 | 3 | **MERGE → 30 的一条子用例** | 与 30 是同一不变量（restore 对账）的已投递/未投递两半；13 已示范"一条测试放两个任务"的写法 |
| 27 | `re_park_a_pending_subagent_notification_steer_on_resume` | 328 | 2 | KEEP | child journal 的 steer 重挂载 + 恰好一次投递；依赖真 resume 重放，直调替代不了 |
| 28 | `not_re_park_a_pre_fix_subagent_notification_steer_into_main_on_resume` | 219 | 1 | KEEP（低优先） | 兼容性护栏（`main` journal 上旧形态 steer）。价值随旧数据老化递减，但回归后果是"发给错 addressee 的 main turn"，保留 |
| 29 | `silently_reappend_a_notification_whose_steer_was_flushed_but_never_delivered_on_resume` | 241 | 1 | **REWRITE** | 崩溃窗（flushed 未 delivered）的自愈；断言（无 turn / 恰好一条 append / marker）可直调 `restore_task_notifications` |
| 30 | `silently_reappend_undelivered_subagent_notifications_on_resume` | 225 | 1 | **REWRITE（承接 26）** | 保留为唯一的 subagent restore 测试：两个任务（一个已投 marker，一个未投 → 静默重放），照 13 的结构；断言 child journal 路由 + marker + 无 turn |
| 31 | `cancel_the_notification_turn_when_message_abort_targets_the_session` | 71 | 0 | KEEP | abort 真取消 + busy 清除 |
| 32 | `hold_the_notification_delivery_while_the_goal_is_paused` | 64 | 1 | KEEP | goal paused 冻结投递 |
| 33 | `hold_the_stranded_notification_flush_while_the_goal_is_paused` | 102 | 1 | KEEP | paused gate 在 stranded peek **之前**，与 32 不同分支 |
| 34 | `cancel_the_notification_turn_when_goal_pause_is_called` | 59 | 0 | KEEP | pause → cancel_in_flight → self-settle 组合 |
| 35 | `cancel_the_notification_turn_when_goal_cancel_is_called` | 60 | 0 | KEEP | cancel 的"清除"半部（`goal.get` → null） |
| 36 | `pause_the_goal_when_message_abort_cancels_the_notification_turn` | 117 | 1 | KEEP | 中断 turn 必须停住 goal loop |
| 37 | `run_cron_turn_with_session_current_model_at_trigger` | 88 | 0 | KEEP | cron turn 读 live model（spec 11 R11/A12） |
| 38 | `use_pinned_model_for_notification_turn_addressed_to_subagent` | 152 | 1 | KEEP | subagent addressee 保留 pin，与 37 是相反方向 |
| 39 | `not_fire_overdue_tasks_when_scheduling_is_disabled` | 85 | 0 | KEEP | 禁用开关不进 check loop |
| 40 | `report_the_disabled_state_when_listing_cron_tasks_while_scheduling_is_disabled` | 85 | 0 | KEEP | honest-list 层 `disabled: true` |
| 41 | `inject_both_sessions_when_two_sessions_share_one_scheduler_tick` | 37 | 0 | KEEP | 断言是**两**次注入（一个 tick 扇出到多条 live session）；42 只断言存活的那条仍 fire，替代不了扇出 |
| 42 | `keep_scheduling_the_other_session_when_one_session_closes` | 42 | 0 | KEEP | V10 |
| 43 | `hold_a_manual_trigger_while_the_session_is_busy_and_deliver_on_idle` | 113 | 1 | KEEP | run-now 的 hold + 不动 cursor |
| 44 | `redeliver_a_held_fire_when_the_session_closes_and_reopens` | 108 | 2 | KEEP | "closed means lost" 已退休的守卫 |
| 45 | `cascade_clear_held_fires_when_the_session_is_deleted` | 82 | 1 | KEEP | V4 级联判据 |
| 46 | `merge_a_new_due_fire_into_the_held_one_while_busy` | 129 | 1 | KEEP | C.4 coalesce-on-hold |
| 47 | `clear_the_held_fire_when_the_llm_provider_cannot_be_created` | 50 | 1 | KEEP | M3 有界丢失 vs 无限重试 |
| 48 | `report_held_true_when_a_manual_trigger_queues_behind_a_busy_turn` | 111 | 1 | KEEP | wire 答案形状（`held` 键的有无） |
| 49 | `keep_scheduling_other_sessions_when_one_sessions_pass_panics` | 54 | 0 | KEEP | M2 panic 隔离，用了 `#[cfg(test)]` panic seam |

**汇总（49 条 → 46 条）：**

| 判定 | 条数 | 测试 |
|---|---|---|
| DELETE | 1 | #15 |
| MERGE（并入后消失） | 2 | #19→#20、#26→#30 |
| KEEP | 40 | 含 #28（低优先，兼容性护栏） |
| KEEP-LOOP（必须跑真实 spawned loop） | 2 | #3（证明 timer 驱动投递）、#9（loop 退出） |
| REWRITE（不变量保留，装置改直调） | 3 | #13、#29、合并后的 #30 |

窗口合计 41 处（与 `grep -c` 一致），行数合计约 5,238。

> 修正记录：初稿曾把 #22、#25、#41 也列为合并对象。逐条读实现后撤回——#22 断言的是 durable `turn.prompt` provenance、#25 走的是 `task_dispatcher` 的另一臂、#41 断言的是双扇出，三者合并都会丢覆盖。

---

## 3. 其它文件的站点判定（38 处）

### 3.1 `embedded_server/tests.rs`（18 处）

| 站点 | 归属测试 | 判定 | 处理 |
|---|---|---|---|
| 245 | `should_wait_for_census_and_recovery_before_cron_first_tick` | KEEP | hold probe 本身保证不会结算 → 窗口删除，直接断言 `census.get().is_none()` |
| 5935 | `should_launch_a_goal_continuation_turn_...` | KEEP | goal 离开 Active ⇒ settle 点已返回 ⇒ 窗口删除 |
| 6136 | `should_not_launch_a_continuation_when_background_tasks_are_in_flight` | KEEP | wait-mode `message.send` 返回前已跑完 chain 决策 ⇒ 窗口删除 |
| 6225 | `should_pause_the_goal_after_repeated_idle_continuation_turns` | KEEP | 同上（Paused 已写入） |
| 6318 | `should_pause_the_goal_when_the_absolute_continuation_limit_is_reached` | KEEP | 同上 |
| 15529 | `should_skip_with_info_notice_when_manual_compaction_has_nothing_to_compact` | KEEP | skip 路径在 spawn apply 任务**之前**返回 ⇒ 窗口删除 |
| 15828 | `should_not_fire_post_compact_hook_when_manual_compaction_cancelled` | KEEP | 需 **apply 完成信号**（hook 在 settle 之后才跑，`wait_manual_compaction_settle` 不够） |
| 15997 | `should_buffer_prompt_when_manual_compaction_in_flight_until_settled` | KEEP | 改断言 `manual_in_flight()` 为真 ⇒ 窗口删除 |
| 16445 | `should_not_persist_an_apply_record_when_the_must_shrink_guard_refuses` | KEEP | 需 apply 完成信号；同时删掉那条 `TODO(time-based absence check)` |
| 16695 | `should_deliver_the_compaction_cancelled_event_...` | KEEP | 需 apply 完成信号（teardown settle） |
| 16755 | `should_reject_a_second_manual_compaction_while_one_is_in_flight` | KEEP | 需 apply 完成信号（纯 teardown） |
| 16922 | `should_refuse_manual_compaction_when_a_turn_is_active` | KEEP | 需 apply 完成信号（纯 teardown） |
| 17028 / 17045 | `should_keep_the_newer_attempts_registration_when_a_stale_attempt_settles` | KEEP | 两处都需 apply 完成信号 |
| 17093 | `should_not_deliver_a_completed_event_after_a_cancelled_attempt_settles` | KEEP | 需 apply 完成信号 |
| 23260 | `should_not_connect_a_plugin_server_disabled_by_the_first_call` | KEEP | 改等 `McpStartupTiming` 结算（accessor only） |
| 36193 | `should_keep_a_silent_turn_alive_when_turn_stall_timeout_ms_is_zero` | KEEP | 断言 `turn_stall_timeout() == Duration::MAX` + `session_is_busy` ⇒ 窗口删除，不需要"等 300ms 看没炸" |
| 44094 | `assert_plugin_server_absent_for_a_while`（helper，1 个调用点） | KEEP | 同上，等 MCP 结算 |

→ **0 删除。** 8 处需要唯一的那个计数器（compaction apply）；2 处等 MCP 结算——其中 `tests.rs:44094` 经审查改判为**保留**（`plugin.install` 不碰 `McpStartupTiming`，等它永真）；6 处窗口可直接删；2 处需先补信号（#245 的「探针被问过」标志、#5935 的 `chain_running`）。

### 3.2 其它文件（21 处）

| 文件 | 处 | 判定 | 处理 |
|---|---|---|---|
| `cron_ws_4b_tests.rs` | 4 | KEEP | `pub(super)` 直驱 cron pass；#806 改 `poll_agent_activity`，#1027 等 queued slot 出现 |
| `t_session_consistency_tests.rs` | 3 | KEEP | follow driver 的 pass 体不是独立函数 → 二选一：抽 `async fn follow_pass(...)`（推荐，属正常分解）或 1 个 `AtomicU64` |
| `cron_ws_4a_tests.rs` | 1 | KEEP | 改等 `await_housekeeping_settled()` |
| `b1_print_mode_tests.rs` | 1 | KEEP AS-IS | 300ms 是被测任务**模拟的工作时长**，是场景输入而非同步原语 |
| `skill_watcher.rs` | 1 | KEEP | `#[cfg(test)]` 退出标志（3 行）；`JoinHandle` 也可但需改结构体字段 |
| `swarm_batch.rs` | 6 | KEEP AS-IS | 全部在 `start_paused` 下，是虚拟时钟的场景步进；只改 reason 文案 |
| `host.rs` | 1 | KEEP | 测试内 fixture 加 chat 计数器，零生产改动 |
| `mock_llm_tests.rs` | 1 | KEEP | `delta_yield_count()` 已存在 |
| `orphan_governance.rs` | 1 | KEEP | 手写 `eventually` 换 `wait_for`（测试辅助代码） |
| `conformance.rs` | 1 | KEEP AS-IS | pgrep 查子进程后代，进程内无可观测面 |

---

## 4. 79 个窗口的去向

改造后 **8 处注解保留、71 处消失**（保留：`conformance.rs:883`、`b1_print_mode_tests.rs:111`、`swarm_batch.rs` ×6）。71 处的机制分布：

| 机制 | 处数 | 需要生产改动 |
|---|---|---|
| 删除窗口，改为直驱 driver 循环体（`deliver_due_notifications` / `schedule_*_cron_pass`） | 26 | 3 个 `pub(super)` |
| 删除窗口，改为直调 `restore_task_notifications` | 6 | 1 个 `pub(super)` |
| 删除窗口，改为已有的正向信号 | 18 | 无（1 个 `#[cfg(test)]` 方法：watcher 退出标志） |
| 删除窗口，改为 apply 完成信号 | 8 | **唯一的新计数器**：`AtomicU64` + accessor |
| 删除窗口，改为 MCP 启动结算 accessor | 2 | 1 个 `#[cfg(test)]` 方法 |
| 删除窗口（前置决策本就同步完成，无替代物） | 7 | 无 |
| 删除窗口，改为测试内 fixture 计数 | 2 | 无（fixture 本地） |
| 手写 `eventually` 换 `mirri_test_support::wait_for` | 1 | 无（测试辅助代码） |
| 随测试一起删除（#15） | 1 | 无 |
| **消失合计** | **71** | |
| 保留原窗口（逐条精确理由） | 8 | 无 |
| **总计** | **79** | |

各行的逐站点归属见 §2 / §3 的判定表（那是权威表）。

**生产代码净增量估计：** 4 个 `pub(super)`（零行）+ 1 个 `AtomicU64` 字段/初始化/accessor + 2 个 `#[cfg(test)]` 方法（`McpStartupTiming::is_settled` 与其 accessor）+ 1 个 `#[cfg(test)]` 退出标志字段/写入/accessor + 1 次 `follow_pass` 抽取 ≈ **50 行**，行为改动为 0（`dispose()` 除外，见 §7）。

对比上一版计划（5 个 `AtomicU64` + 6 个 accessor + ctor 初始化 ≈ 120 行，且要求测试保留"等够 N 个 tick"的等待）：**生产面减少约 2/3，且转换后的测试不再有任何等待。**

---

## 5. 范围选项

| 选项 | 内容 | 剩余窗口 | 生产改动 | 工时 |
|---|---|---|---|---|
| **A** | 只修 2 个 CI flaky（含 `dispose()` 真 bug） | 79 | 1 个字段 + 1 段 abort | 0.5–1 天 |
| **B（推荐）** | A + §1 可见性 seam + 71 处全量转换 | **8** | ≈45 行，零行为改动 | 3–4 天 |
| **C** | B + 3 处合并/重写（49 → 46 条 b2 测试，约 −900 行装置） | 8 | 同 B | 5–6 天 |
| **D** | 原计划（5 个 `AtomicU64` + 6 个 accessor） | 8 | ≈120 行 | 5–6 天 |

推荐 **B**：71/71 的窗口全部消失，flaky 归零，生产面 45 行且零行为改动（`dispose()` 除外，见待决策 2）。转换后的测试**不再有任何等待**——不是"换个方式等"，是不等。

**不建议**把 C 的合并放进 B：合并要动断言与 fixture，评审面变大，而收益只有 3 条测试。C 真正的价值是那约 900 行整机重启装置，值得单独一批做。

**不建议 D**：同样收益，生产面是 B 的 2.7 倍，且要求测试保留"等够 N 个 tick"的等待——比 B 的直驱模型更弱。

---

## 6. 明确不做

- 不动 `tests.rs` 其余 ~690 个测试（不在本次 79 个站点范围内，需要独立审计）
- 不引入 `start_paused`（ROADMAP R4 裁决）
- 不删除任何被 `#[cfg(test)]` seam 支撑的 panic 注入点（`cron_pass_panic_session` 等）
- 不为"减少测试数量"而删除覆盖

---

## 6.5 审查后的判定修订（2026-11-05）

三份独立审查（代码事实 / 可执行性 / 断言语义）推翻或修正了本审计的以下判断。**以本节为准**：

| 站点 | 原判定 | 修订后 | 原因 |
|---|---|---|---|
| `tests.rs:44094` | 等 MCP 结算（转换） | **保留窗口** | `plugin.install` 从不触及 `mcp_startup`（只有 `mod.rs:2939/3003/3078` 三处 `add_task()`），`is_settled()` 对它永真，会塌缩为瞬时采样并漏掉「install 异步连接」的回归 |
| `tests.rs:15997` | 断言 `manual_in_flight()` | **改为「send 仍在跑 + 计数稳定」** | 原方案断言的是被测机制本身：删掉 `wait_manual_compaction_settle`（正是它守护的回归）测试照样通过 |
| `b2:1440` | 等 `notification_delivered_at` | **改等 turn 收尾** | marker 在 `run_engine_turn` **之前**就写（`task_driver.rs:752` vs `884`），等待是空操作 |
| `b2:2350/2457/2603/2950/4402` | `wait_for_turn_idle` | **改用 addressee 维度** | `poll_agent_activity` 硬编码 `agent_id: "main"`，而这些测试的投递对象是 subagent |
| `tests.rs:5935` | 删窗口（理由：goal 非 Active） | **改等 `chain_running == false`** | `UpdateGoal` 在 turn 进行中就写 `Complete`（`update_goal.rs:100`），早于 settle |
| `tests.rs:245` | 删窗口 | **删窗 + 补「探针被问过」标志** | 删窗后两个断言在 t=0 都为真，尚未观测到任何东西 |
| `tests.rs:36193` | 删窗口 | **删窗 + 先等 turn 可见** | 否则 fire-mode send 后立刻查 busy 会引入新 flake |
| `t_session_consistency:163` | 3 次 `follow_pass` | **先等 memory marker 再 drain channel** | relay 由 spawn 的 forwarder 转发，同步 pass 可能在帧到达前返回 |
| `cron_ws_4a:914` | 等 `await_housekeeping_settled()` | **改等一次 cron pass** | census 里的 import 是 `tokio::spawn`（`orphan_governance.rs:171-174`），cell 结算不等它 |

**保留集因此从 8 变为 9**，转换数 71 → 70。

---

## 7. 决策记录

2026-11-05 用户裁决：

1. **范围 B** —— 全部窗口确定性化，不合并/不重写 b2 测试（那 3 条留作 C，后续单独一批）。审查后保留集为 9 处、转换 70 处（见 §6.5）。
2. **`dispose()` 行为改动 —— 已批准**（订阅通道在 dispose 后真正关闭；嵌入方 `recv()` 开始返回 `None`，进 changeset）。
3. **基线 —— `feat/dev-batch-2026-11-05`**（写作时 `f70b772d`）。`test/sleep-governance` 的 T4–T6 **放弃**，不合并、不作为基线。该分支的 `rust/` 与本文写作时的 `e788ac9d` 已不同：`tests.rs` +86 行导致 13 个站点行号下移（修正表见实施计划 Global Constraints），站点总数仍是 79。
4. follow driver —— 抽 `follow_pass` 函数（不是计数器）。
5. 既有门禁债务（199 条 `new (would fail)`）—— 不要求本批修，「不小心碰了也没事」，但不为它专门开工。

实施计划：[`2026-11-05-negative-window-determinization.md`](./2026-11-05-negative-window-determinization.md)（7 个任务，Task 3 只加设施，Task 4/5/6 分别转 b2 / `tests.rs` / 其余文件，Task 7 重锚 baseline 并回写 ROADMAP）。
