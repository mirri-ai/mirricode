# 否定窗确定性化（范围 B）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `rust/crates/` 中 79 处 `// hygiene-allow:` 固定等待窗的 **70 处**改为**确定性驱动**（直接调用 driver 循环体 / 断言已存在的正向信号），使转换后的测试**不再等待**；保留 9 处真实时间语义窗口并给出逐条理由；同时修掉 2 个 CI 实锤 flaky 的根因。

**Architecture:** 核心手段是**可见性而非计数器**——`deliver_due_notifications`、`schedule_session_cron_pass`、`schedule_workspace_cron_pass`、`follow_pass` 本身就是（或可抽成）独立的自由函数，改为/抽为 `pub(super)` 让同 crate 测试直接调用；于是"六个 tick 内没有新通知"变成"六次真实的投递裁决"，窗口整行删除。只有手动压缩的 apply 完成需要一个真正的 `#[cfg(test)]` 计数器（spawn 体内部无法直调）。断言词汇（事件集合、机会次数）逐条保持不变。

**Tech Stack:** Rust workspace（`rust/Cargo.toml`）、tokio、`mirri-test-support::wait_for::{wait_for, wait_for_async}`、`node rust/scripts/check-test-hygiene.mjs`、`cargo test` / `cargo clippy`。

**前置决策记录：** 范围与逐条判定见同目录 `2026-11-05-negative-window-audit.md`（审计结论：49 条 b2 测试中仅 1 条冗余、2 条并入；问题在测试手法而非测试数量）。本计划只执行该审计的 **范围 B**。

## Global Constraints

- **Worktree 隔离（硬性）：** 全部工作在独立 worktree + 分支 `test/deterministic-negative-windows` 内进行。不得在主 checkout 编辑。
- **基线分支（硬性，用户已裁决 2026-11-05）：从 `feat/dev-batch-2026-11-05` 切分支。**
  - `origin/main`（`6015a8ed`）的 `rust/` 树里一处 `hygiene-allow` 都没有、也没有 `rust/scripts/check-test-hygiene.mjs`——**绝不能**从 `main` 切。
  - `test/sleep-governance` 已**放弃**（不合并、不作为基线）。
  - 实际基线是 `feat/dev-batch-2026-11-05` @ **`f70b772d`**。它与本计划写作时的 `e788ac9d` **不再逐字节相同**：`tests.rs` +86 行、`session_handlers.rs` +27/−9、`task.rs` +148。站点总数仍是 **79**，但 **`tests.rs` 的行号整体下移**（见下表）。
- **`tests.rs` 行号修正表（`f70b772d` 实测，以测试名为准最可靠）：**

| 计划旧行号 | 实际行号 | 所属测试（源码名） |
|---|---|---|
| 15529 | **15615** | `should_skip_with_info_notice_when_manual_compaction_has_nothing_to_compact` |
| 15828 | **15914** | `should_not_fire_post_compact_hook_when_manual_compaction_cancelled` |
| 15997 | **16083** | `should_buffer_prompt_when_manual_compaction_in_flight_until_settled` |
| 16445 | **16531** | `should_not_persist_an_apply_record_when_the_must_shrink_guard_refuses` |
| 16695 | **16781** | `should_deliver_the_compaction_cancelled_event_when_an_in_flight_compaction_is_cancelled` |
| 16755 | **16841** | `should_reject_a_second_manual_compaction_while_one_is_in_flight` |
| 16922 | **17008** | `should_refuse_manual_compaction_when_a_turn_is_active` |
| 17028 | **17114** | `should_keep_the_newer_attempts_registration_when_a_stale_attempt_settles` |
| 17045 | **17131** | 同上（清理处） |
| 17093 | **17179** | `should_not_deliver_a_completed_event_after_a_cancelled_attempt_settles` |
| 23260 | **23346** | `should_not_connect_a_plugin_server_disabled_by_the_first_call` |
| 36193 | **36279** | `should_keep_a_silent_turn_alive_when_turn_stall_timeout_ms_is_zero` |
| 44094 | **44180** | helper `assert_plugin_server_absent_for_a_while`（定义在 **44166**） |

  245 / 5935 / 6136 / 6225 / 6318 五行**未漂移**（都在插入点之前）。其余文件的 38 个站点未受影响（实测行号与计划一致）。

- **断言语义等价（硬性）：** 禁止/要求的事件集合、计数、机会次数（`six ticks`、`a dozen ticks`、`three follow ticks`）逐条保留。只换等待机制，不"顺手改强"或"顺手改弱"。
- **不新增 paused-clock**（`docs/specs/rust-core-design/ROADMAP.md` R4 裁决）：`swarm_batch.rs` 的 6 处已在 `start_paused` 下，只改 reason 文案。
- **生产改动最小化：** 3 个 `pub(super)`（零逻辑行：`deliver_due_notifications`、两个 cron pass 函数）+ 1 个 `#[cfg(test)] AtomicU64`（apply 完成，用 drop guard 计数）+ 1 个 `#[cfg(test)]` MCP 结算方法 + 1 个 `#[cfg(test)]` watcher 退出标志 + 1 次 `follow_pass` 抽取（含循环顺序不变）。`restore_task_notifications` **不**放宽（范围 B 没有消费者，留到范围 C）。**行为改动仅 `dispose()` 一处**（Task 2，需用户确认）。
- 遵守 `rust/AGENTS.md` 测试铁律：`// layer: T<n>`、Given-When-Then 命名、隔离契约八条。
- 不新增裸 sleep；保留的 `// hygiene-allow:` 只用于真实时间语义，reason 逐条具体（禁止再写泛化的 `negative window (proving absence)`）。
- **门禁当前就是红的（已实测），验收判据必须换。**（用户 2026-11-05：「不小心碰了也没事」——意味着既有债务不要求本批修，但也不禁止顺带处理；不要为它专门开工。） 该树实测 `violations: 360 / baseline-covered: 161 / new (would fail): 199 / baseline entries: 429`。那 199 条不在本批 79 个站点范围内（如 `conformance.rs:1724,2154,2180`、`session_group.rs:12254,13735`、`ask_user.rs:2008`、`hooks.rs:2979`）。因此：
  - **不能用 `gate ok: 0 new violations` 作为验收**——它在本批开始前就不可能达到。
  - 改用**相对判据**：Task 7 记录 `--report` 的 `new (would fail)` 数值，改动后必须**不高于**本任务 Task 1 Step 4 记录的基线值（等于即通过，因为本批不新增裸 sleep）。
  - 卫生扫描器不会统计 `hygiene-allow` 注解的行（`check-test-hygiene.mjs:310` 直接 `continue`），所以"注解减少了多少"只能靠 `grep -rn "hygiene-allow" rust/crates --include='*.rs' | wc -l`，每个任务的验收都用这条。
- **基线重锚：** 扫描器按 `file:line` 记账，编辑携带存量 baseline 的文件（`b2_notification_tests.rs` 91 条、`tests.rs` 86 条）必然漂移，漂移条目会被报成 new violation。禁止用注解吸收漂移；Task 7 用 `--write-baseline` 重锚一次，并证明**重锚后 `new (would fail)` 回到 0 或等于基线值**、条目数不增长、reason 类别不变。
- 提交：Conventional Commit 风格，每类一提交，不暴露 agent 身份，无 co-author。每次 `git commit` 前询问用户。
- 长 cargo 命令后台运行（`run_in_background`，超时 ≥20 分钟），输出重定向 `.tmp/negwin/`；卡点排查靠读日志，禁盲目加超时（测试超时纪律）。

## 基线事实（用于归因，不写进 commit message）

- 79 处注解分布于 12 个文件；归属 65 个测试。
- `mirri-embedded --lib` 本地 ≈585 s / CI ≈152 s；`b2_notification_tests` 本地 ≈65 s（Task 1 实测记录）。
- 已知**预存**失败（勿归咎本批、勿顺手修）：`mirri-test-harness` 的 `cross_backend_contract` 3 个 restore 测试（`state.json not found`）、`should_mount_the_watcher_within_a_fixed_time_budget`（负载 flaky）。
- **实测追加（Task 1 在 `4ba0444a` 上跑全量 lib 时发现）**：`should_carry_the_created_stamp_on_archive_view_rows_when_listing` 也是预存失败——created stamp 与 `state.json` 的秒边界竞态（`left: …073, right: …072`）。不在本批 79 个站点范围内，不修，但**不要**把它算成本批引入的回归。
- **基线实测值（`4ba0444a`）**：b2 `49 passed`（测试耗时 3.31 s）；`mirri-embedded --lib` `1280 passed / 2 failed`（53.26 s）；hygiene `new (would fail) = 206`、baseline entries `429`。`should_stop_event_stream_when_disposed` **单跑 10/10 通过、全量跑必失败**——flake 在基线上已复现，这是后续对比的证据。

## 语义陷阱（审查发现，必须按此修正表格里的写法）

以下站点如果照 §Task 4/5 表格的**简单替换**做，会把"证明 X 没有发生"换成空断言。逐条给出正确的替代物：

| 站点 | 表格里的简单写法为何失效 | 正确写法 |
|---|---|---|
| #1440 `not_redeliver_..._compaction_drops_it` | `run_notification_turn` 在 **LLM 调用之前**就调 `mark_notification_delivered`（`task_driver.rs:752` vs `884`），而测试已在前面轮询过 `notification_reached_model` → Markder 早已 `is_some()`，等待是**空操作**；200ms 实际承担的是"这个 turn 收尾" | 等 turn 收尾：`wait_for_turn_idle(&server, &session_id).await;`（后面的 `history[index+1]`、`retention_tail_start` 断言依赖它） |
| #15997 `buffer_prompt_when_manual_compaction_in_flight` | 断言 `manual_in_flight()` 是在断言**被测机制本身**：删掉 `wait_manual_compaction_settle` 这个回归它照样通过 | 见 Task 5 Step 3 的改写：先 `wait_manual_compaction_apply(before+1)` 不行（还没结算完）；改为等 `send_task` 仍在跑 + `chat_call_count` 稳定，见下方专段 |
| #2350 / #2457 / #4402 / #2603 / #2950 | `wait_for_turn_idle` 用的是 `poll_agent_activity`，它**硬编码 `agent_id: "main"`**（`tests.rs:24919`）。这些测试的投递对象是 **subagent**，main 早已 idle → 等待立即返回，150ms 被静默删除；#2950 更糟：resumed server B 的 main 可能**没有** `lastTurn`，轮询永不满足 → 5s 后失败 | 改为轮询 **addressee** 的 activity（`poll_agent_activity_for(server, session_id, agent_id, check)`，见 Task 3 新增），或失败时退化为 `!server.session_is_busy(...)` |
| #5935 `launch_goal_continuation_when_active_goal` | 理由写"goal 离开 Active ⇒ chain gate 已返回"是**错的**：`UpdateGoal` 在 turn **进行中**就写 `GoalStatus::Complete`（`update_goal.rs:100`），早于 settle 与 `maybe_launch_goal_chain` | 等 chain 真正停下：`goal_chain_states[session].chain_running == false`（chain 任务退出时清标志，`goal_driver.rs:553-564`）——见 Task 5 Step 1 的 helper |
| #245 `wait_for_census_and_recovery_before_cron_first_tick` | 删窗后两个断言在 t=0 都为真，什么都没观测到 | 给 `hold_probe` 加一个"被问过"标志：等 `probe_asked == true`（证明 scheduler 真的走到了 gate），再断言 `census.get().is_none()` |
| #36193 `silent_turn_alive_when_stall_timeout_zero` | 删窗后 `session_is_busy` 紧跟 fire-mode send，turn 可能尚未注册 → 新的 flake | 先等 turn 可见（`wait_for_async(!is_finished …)` 或等 `event.turn.started`），再断言 `turn_stall_timeout() == Duration::MAX` |
| #4983 / #5163 | session 已删 / provider 建不出来时 `schedule_session_cron_pass` 在 `resolve_session_via` 就早退，"不注入"由构造保证，`chat_call_count == 1` 变成恒真 | 接受这一点，但把断言改成**被钉住的守卫**：注释写明"pin 的是 resolve 早退（会话已删不再进入 due 检查）"，并保留原计数断言作为兜底 |
| #44094 `assert_plugin_server_absent_for_a_while` | `plugin.install` 不碰 `mcp_startup`，`is_settled()` 永真 → 塌缩为一次瞬时采样 | **保留本处窗口**并改 reason 为：`absence sample (plugin.install never touches McpStartupTiming; an async connect regression would need a real window to catch)` |
| #914 `not_reimport_workspace_local_archive` | census 里的 cron import 是 `tokio::spawn`（`orphan_governance.rs:171-174`），`await_housekeeping_settled()` 只等 cell，**不等这个 spawn** | 改等一次 cron pass 真正跑完（`drive_cron_pass` 两次）——import 在该 pass 之前必须已落盘；或保留窗口并写明理由 |
| #163 `follow_foreign_writes` | 断言在 event channel 上（`events.try_recv`），而 relay 由 spawn 的 forwarder 转发；3 次同步 follow_pass 可能在帧被转发前就返回 | 等 memory 的 marker 出现（已有 `marker_message_count`）后再 drain channel；不要只靠 3 次 pass |

**由此产生的站点数变化：** #44094（在 `tests.rs`，属 Task 5）从「转换」改为「保留」，所以 Task 5 的转换数由 18 变为 **17**，保留集由 8 变为 **9**。

完整的注解数链（每个任务结束时 `grep -rn "hygiene-allow" … | wc -l`）：

| 阶段 | 转换 | 剩余 |
|---|---|---|
| 起点 | — | 79 |
| Task 3（#758） | 1 | 78 |
| Task 4（b2 其余） | 40 | 38 |
| Task 5（`tests.rs`） | 17 | 21 |
| Task 6（其余 7 个文件） | 12 | **9** |

---

## 行号漂移提醒

**测试名一律带 `should_` 前缀。** 本计划与审计的表格为可读性省略了它（如 `exit_the_driver_when_the_session_disappears` 在源码里是 `should_exit_the_driver_when_the_session_disappears`）。按名对齐时务必补前缀，否则 `grep` 返回空、会被误判成"站点已移走"。

下文 `file:line` 以 `test/sleep-governance` @ `e788ac9d` 为准。Task 1 用 `grep -rn "hygiene-allow" rust/crates --include='*.rs' | sort` 重新导出权威清单；漂移按测试名对齐（`grep -n "async fn <name>" <file>`）。**权威判定表是审计文档的 §2/§3**，本文的行号只是定位提示。

---

## 文件结构

生产 seam（除 `dispose()` 外均为可见性或 `#[cfg(test)]`）：

- `rust/crates/mirri-embedded/src/embedded_server/task_driver.rs` — `deliver_due_notifications` 改 `pub(super)`。
- `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs` — 两个 pass 函数改 `pub(super)`；抽出 `follow_pass` 式的单次循环体。
- `rust/crates/mirri-embedded/src/embedded_server/mod.rs` — `manual_compaction_applies` 字段/初始化/accessor；`McpStartupTiming::is_settled` + accessor；`subscriber_forwarders` 字段 + `dispose()` abort。
- `rust/crates/mirri-embedded/src/embedded_server/compaction_handlers.rs` — apply 计数自增。
- `rust/crates/mirri-embedded/src/embedded_server/follow_scheduler.rs` — 抽 `follow_pass` 供直调。
- `rust/crates/mirri-embedded/src/skill_watcher.rs` — `#[cfg(test)]` 退出标志。

测试文件：

- `tests.rs`（18 处 + 全部 helper + dispose 测试）
- `tests/b2_notification_tests.rs`（41 处，含 #758）
- `tests/cron_ws_4b_tests.rs`（4）/ `tests/t_session_consistency_tests.rs`（3）/ `tests/cron_ws_4a_tests.rs`（1）
- `tests/b1_print_mode_tests.rs`（1，**保留**）
- `mirri-core/src/subagent/host.rs`（1）、`mirri-core/src/subagent/swarm_batch.rs`（6，**保留/relabel**）、`mirri-test-harness/tests/mock_llm_tests.rs`（1）、`mirri-acp/tests/conformance.rs`（1，**保留**）、`orphan_governance.rs`（1）

按文件计数合计（12 个文件）：`b2` 41 + `tests.rs` 18 + `swarm_batch` 6 + `cron_ws_4b` 4 + `t_session_consistency` 3 + `cron_ws_4a` 1 + `b1_print_mode` 1 + `skill_watcher` 1 + `host` 1 + `mock_llm_tests` 1 + `conformance` 1 + `orphan_governance` 1 = **79** ✓（`skill_watcher.rs` 的站点在"生产 seam"一节里列出，因为改动与它的 `#[cfg(test)]` 标志同处一个文件。）

文档：`docs/specs/rust-core-design/ROADMAP.md`（R4 状态回写）。

---

### Task 1: Worktree、分支、基线

**Files:**
- Create: worktree `.worktrees/deterministic-negative-windows`（分支 `test/deterministic-negative-windows`）
- Create: `.tmp/negwin/`（gitignored 日志目录）

**Interfaces:**
- Consumes: 无。
- Produces: 后续所有任务提交所在分支；`.tmp/negwin/baseline-*.log`。

- [ ] **Step 1: 确认基线分支与站点数**

```bash
cd /Users/chengri/workspace/opensource/mirri-ai/mirricode
git fetch origin
git log --oneline feat/dev-batch-2026-11-05 -5
git rev-parse --short feat/dev-batch-2026-11-05
grep -rn "hygiene-allow" rust/crates --include='*.rs' | wc -l
```

**用户裁决（2026-11-05）：** 基线是 `feat/dev-batch-2026-11-05`；`test/sleep-governance` 的 T4–T6 **放弃**，不合并、不作为基线。

预期：站点数 `79`。若 `rev-parse` 不是 `f70b772d`，说明分支又前进了——按测试名（**不是行号**）重新对齐 Task 4/5/6 的位置；Global Constraints 里有 `f70b772d` 的实测修正表可作参照。

- [ ] **Step 2: 建隔离 worktree**

```bash
cd /Users/chengri/workspace/opensource/mirri-ai/mirricode
git worktree add -b test/deterministic-negative-windows \
  .worktrees/deterministic-negative-windows feat/dev-batch-2026-11-05
cd .worktrees/deterministic-negative-windows
mkdir -p .tmp/negwin
git branch --show-current
grep -rn "hygiene-allow" rust/crates --include='*.rs' | wc -l
```

预期：分支名 `test/deterministic-negative-windows`；站点数 `79`。

> **不要用 `origin/main`。** 实测 `origin/main`（`6015a8ed`）的 `rust/` 树里一处 `hygiene-allow` 都没有，也没有 `check-test-hygiene.mjs`——从那里切分支，本计划的 79 个站点和全部工具都不存在。

- [ ] **Step 3: 重新导出权威站点清单**

```bash
grep -rn "hygiene-allow" rust/crates --include='*.rs' | sort \
  | tee .tmp/negwin/sites-before.txt | wc -l
```

预期：`79`。若不是 79，按**测试名**（源码里带 `should_` 前缀）逐条对齐——不要用行号，基线已从 `e788ac9d` 前进到 `f70b772d`。

- [ ] **Step 4: 记录门禁起点**

```bash
node rust/scripts/check-test-hygiene.mjs --report | tee .tmp/negwin/baseline-hygiene.txt
node -e "const b=require('./rust/scripts/test-hygiene-baseline.json').violations;const k=Object.keys(b);console.log('entries:',k.length);const r={};for(const v of Object.values(b))r[v.reason.split(':')[0]]=(r[v.reason.split(':')[0]]||0)+1;console.log(r);" | tee .tmp/negwin/baseline-counts.txt
```

**预期（已实测，这是起点而非目标）：** `violations: 360`、`baseline-covered: 161`、`new (would fail): 199`、b​aseline entries `429`。

把 `199` 这个数字记进 `.tmp/negwin/baseline-counts.txt`——它才是本批的**相对判据**（见 Global Constraints）。门禁在本批开始前就是红的，那 199 条是既有债务、不在 79 个站点范围内；**不要试图修它们**，也不要因为它们而停下来。真正要证明的是：改动后这个数字不增长。

- [ ] **Step 5: 记录改动前的耗时基线**

```bash
( time cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::b2_notification_tests > .tmp/negwin/baseline-b2.log 2>&1 ) 2>> .tmp/negwin/baseline-b2.log
( time cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib \
    > .tmp/negwin/baseline-embedded-lib.log 2>&1 ) 2>> .tmp/negwin/baseline-embedded-lib.log
```

后台任务（`disable_timeout: true`）。预期：两者全绿；记录日志**末尾**的 `real`（`( time … ) 2>> log` 是把计时追加到文件尾部，不是首部；b2 ≈65 s，embedded-lib ≈585 s）。

- [ ] **Step 6: 记录两个 flaky 的"改造前"证据**

```bash
for i in $(seq 1 10); do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::should_stop_event_stream_when_disposed --exact \
    >> .tmp/negwin/before-dispose.log 2>&1
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::e10_interrupted_recovery_tests::should_not_fold_interrupted_into_failed_on_live_row --exact \
    >> .tmp/negwin/before-e10.log 2>&1
done
grep -c "test result: ok" .tmp/negwin/before-dispose.log .tmp/negwin/before-e10.log
```

预期：`10` / `10`（本机可能不复现；CI 才是证据来源，日志仅作对照）。本任务**不修任何东西**。

---

### Task 2: 两个 CI flaky 修到根因

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（`subscribers` 附近 + `subscribe_events` + `dispose`）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs:4825-4845`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests/e10_interrupted_recovery_tests.rs:993-1065`

**Interfaces:**
- Consumes: 无。
- Produces: `dispose()` 现在关闭订阅通道；`subscriber_forwarders` 字段。

- [ ] **Step 1: 复述 `dispose()` 的契约变化（已获批准，需再次口头确认）**

**用户已批准（2026-11-05）。** 记录在案的事实：`dispose()` 现在清空 `subscribers` 向量，但 `subscribe_events` 装的两个 forwarder 任务各持一个 `Sender` clone，因此通道永不关闭——每调用一次 `subscribe_events` 泄漏两个任务；当前测试（`tests.rs:4838-4842`）断言 `is_err() || matches!(notification, Ok(None))`，它本身**接受**通道关闭，只是实际命中的总是 timeout 分支。失败形态是「队列里恰好有一帧时 `recv()` 返回 `Some`」→ 两个分支都不满足 → 失败。改动后嵌入方 `recv()` 会开始返回 `None`（这是有意的契约变化，进 changeset）。

> 内部方法：即使已获批准，**改动前仍要向用户复述一次**"嵌入方 `recv()` 会开始返回 `None`"，这是对外可见的行为变化。若用户当场改主意，降级为"有界排空"版本（排空至 `Disconnected` 或超时，超时后保留一条精确 reason 的豁免），并在 Task 7 的 ROADMAP 里记录。

**若用户拒绝：** 只做"有界排空"版本——排空队列直到 `Disconnected` 或超时，超时后保留一条 `// hygiene-allow: queue drain (a disposed engine may hold prefetched frames; closure is not observable)`，并在 Task 7 的 ROADMAP 回写里记录该决定。

- [ ] **Step 2: 写失败测试**

把 `should_stop_event_stream_when_disposed` 中 `server.dispose()` 之后的部分换成：

```rust
    // Then the event channel closes. Frames a relay had already queued
    // before the abort are drained first — the assertion is on CLOSURE,
    // not on "the next recv timed out" (a queued frame made the old
    // `timeout(5s)` form flake under load).
    mirri_test_support::wait_for::wait_for_async(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(5),
        || match events.try_recv() {
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Some(()),
            Ok(_) | Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
        },
    )
    .await
    .expect("event channel must close after dispose");
```

- [ ] **Step 3: 跑三次确认失败**

```bash
for i in 1 2 3; do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::should_stop_event_stream_when_disposed --exact 2>&1 | tail -5
done
```

预期：三次都 `FAILED`，信息 `event channel must close after dispose`（各约 5 s）。

- [ ] **Step 4: 追踪并 abort forwarder**

在 `mod.rs` 的 `subscribers` 字段旁加：

```rust
    /// The forwarder tasks [`EngineAdapter::subscribe_events`] installs per
    /// subscriber (the server-event and skill-catalog broadcasts). Tracked
    /// so `dispose` can abort them — each holds a `Sender` clone, and a
    /// live forwarder would keep the subscriber's channel open forever
    /// (the subscriber's `recv()` must resolve `None` once the engine is
    /// disposed).
    subscriber_forwarders: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
```

构造函数（`subscribers: Arc::new(std::sync::Mutex::new(Vec::new())),` 之后）：

```rust
            subscriber_forwarders: Arc::new(std::sync::Mutex::new(Vec::new())),
```

`subscribe_events` 里把两处 `tokio::spawn(...)` 的返回句柄绑定并登记（**保留两个 spawn 体原文**）：

```rust
        let forwarders = Arc::clone(&self.subscriber_forwarders);
        let server_forwarder = tokio::spawn(async move { /* 原 body 原样 */ });
        // …skill-catalog forwarder 同理…
        let skill_forwarder = tokio::spawn(async move { /* 原 body 原样 */ });
        forwarders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([server_forwarder, skill_forwarder]);
```

`dispose()` 中，在清空 `subscribers` **之前**：

```rust
        // The per-subscriber forwarders hold the only remaining `Sender`
        // clones once the vec below is cleared — abort them so each
        // subscriber's channel actually closes instead of living on with a
        // receiver that never sees `None`.
        for handle in std::mem::take(
            &mut *self
                .subscriber_forwarders
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        ) {
            handle.abort();
        }
```

- [ ] **Step 5: 跑十次确认通过**

```bash
for i in $(seq 1 10); do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::should_stop_event_stream_when_disposed --exact 2>&1 | tail -3
done
```

预期：`10 × test result: ok. 1 passed`，每次远快于 1 s（旧形态必然耗满 5 s）。

- [ ] **Step 6: 修 e10 的 fold 竞态**

`should_not_fold_interrupted_into_failed_on_live_row` 里，把

```rust
    // Then the live row's lastTurnReason stays `interrupted` — the fold
    // must not collapse the fifth value into the failed family
    let row = session_list_row(&server, session_id).await;
    assert_eq!(
        row["lastTurnReason"], "interrupted",
        "the live row carries the interrupted outcome, not failed"
    );
```

换成

```rust
    // Then the live row's lastTurnReason is `interrupted` — the fold must
    // not collapse the fifth value into the failed family.
    //
    // The fold is asynchronous: `absorb_session_foreign_updates` returns
    // after publishing, and the activity shadow consumes the event on its
    // own task. Poll the row (an async read) until the fold lands —
    // `lastTurnReason` becomes set exactly once (the replay never seeds
    // it), and a fold into `failed` resolves just as fast, so the assert
    // below still owns the verdict.
    //
    // NOTE: do NOT put this in a `wait_for_async` sync closure — the row
    // read is async, and a nested `block_on` inside a Tokio worker panics
    // ("Cannot start a runtime from within a runtime").
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if session_list_row(&server, session_id).await["lastTurnReason"].is_string() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the foreign absorb must fold the interrupted closure into the live row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let row = session_list_row(&server, session_id).await;
    assert_eq!(
        row["lastTurnReason"], "interrupted",
        "the live row carries the interrupted outcome, not failed"
    );
```

上面的 `tokio::time::sleep` 是 deadline 循环里的**轮询步进**（设计 §4.2 第 4 条许可形态）。运行 `node rust/scripts/check-test-hygiene.mjs` 确认未被判为违规；若被判，改为把轮询抽成带 `wait_for` 标记的本地 helper，**不要**加回 `hygiene-allow`。

- [ ] **Step 7: 跑十次并跑完整 lib**

```bash
for i in $(seq 1 10); do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::e10_interrupted_recovery_tests::should_not_fold_interrupted_into_failed_on_live_row --exact 2>&1 | tail -3
done
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib > .tmp/negwin/t2-lib.log 2>&1; tail -20 .tmp/negwin/t2-lib.log
```

后台任务（`run_in_background: true`，超时 ≥20 分钟；前台 300 s 会被杀）。

预期：10 次通过；lib 全绿（`dispose` 改动会影响其他 dispose 相关测试）。

**关于 e10 的验收口径：** 本机可能本来就复现不出来（Task 1 Step 6 可能已是 10/10）。这种情况下的证据链是**推理 + CI**，不是本地红→绿：
- 代码事实：`absorb_session_foreign_updates` 返回后 fold 仍在异步任务上（`activity_shadow`），原断言确实可能在 fold 落定前读到 `null`；
- 本地只证明"加了等待之后不再更差"（10/10 + 全量 lib 绿）；
- 真正的验证在 CI 的 `rust-test` job（PR #194 里它红过）。在 PR 描述里写明这一点。

- [ ] **Step 8: 提交**

```bash
git add -A rust/crates/mirri-embedded
git commit -m "fix: close subscriber event channels when the engine is disposed"
```

（`dispose()` 已获批准，用 `fix:` 前缀。若用户当场改主意，改用 `test: drain the disposed event stream instead of timing out on it`。）

---

### Task 3: 可见性 seam 与最小 `#[cfg(test)]` 设施

> 顺序说明：本任务只加设施（不转任何站点）；Task 4/5/6 才消费它们。`skill_watcher.rs` 由本任务加 `#[cfg(test)]` 退出标志，Task 6 再改它那条测试。

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/task_driver.rs:221`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:123,741`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1713`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/task_driver.rs`（tick/exit 探针自增）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/follow_scheduler.rs`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（apply 计数器、MCP 结算、compaction handle accessor）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/compaction_handlers.rs`
- Modify: `rust/crates/mirri-embedded/src/skill_watcher.rs`

**Interfaces:**
- Produces（后续任务全部依赖，签名固定）：
  - `pub(super) async fn deliver_due_notifications(index, registry, turn_flows, activity_trackers, steer_buffers, compaction_handles, usage_model_key_handles, llm_factory, server_config, flag_registry, turn_stall_timeout, turn_backstop_timeout, in_flight_turns, goal_chain_states, goal_chain_resources, provider_auth_failures, tool_result_home, session, session_id)`
  - `pub(super) async fn schedule_session_cron_pass(...)`（原签名不变）
  - `pub(super) async fn schedule_workspace_cron_pass(...)`（原签名不变）
  - `pub(super) async fn follow_pass(registry: &Arc<WorkspaceRegistry>, in_flight_turns: &InFlightTurnsMap, session_id: &str) -> bool`（**自由函数**，不是方法）
  - `pub(crate) fn manual_compaction_apply_count(&self) -> u64`
  - `pub(crate) fn mcp_startup_settled(&self) -> bool`
  - `SkillDirectoryWatcher::has_exited(&self) -> bool`
  - `task_driver_tick_count(&self, session_id) -> u64`、`task_driver_exit_count(&self) -> u64`
  - `wait_for_task_driver_exit(server, before)`（测试 helper）
  - `drive_notification_tick(s)` / `drive_cron_pass` / `drive_session_cron_pass` / `drive_follow_passes`（测试 helper，Task 4/5/6 消费）

- [ ] **Step 1: 放开四个函数的可见性**

只改签名行，不动函数体：

```rust
// task_driver.rs
pub(super) async fn deliver_due_notifications(

// cron_driver.rs
pub(super) async fn schedule_workspace_cron_pass(
pub(super) async fn schedule_session_cron_pass(

// session_handlers.rs — NOT widened in scope B (no consumer; the resume tests
// drive the driver instead). Leave it private; scope C's direct-call rewrite
// widens it.
```

- [ ] **Step 2: 验证测试能直接调用**

在 `tests.rs` 的 helper 区（`poll_agent_activity` 附近，约 24909 行）加临时探针并编译：

```rust
#[allow(dead_code)]
async fn _probe_seam_visibility(server: &EmbeddedServer, session_id: &str) {
    let session = server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("session is live");
    // `EmbeddedServer` is already in scope in `tests.rs` via `use super::*`;
    // do NOT path-qualify it through `session_handlers` (it arrives there by
    // a private `use`, so that path is not reachable).
    let _ = EmbeddedServer::deliver_due_notifications;
    let _ = EmbeddedServer::schedule_workspace_cron_pass;
    let _ = EmbeddedServer::schedule_session_cron_pass;
    let _ = session;
}
```

```bash
cargo check --manifest-path rust/Cargo.toml -p mirri-embedded --lib --tests 2>&1 | tail -15
```

预期：编译通过（`E0624` 之类的可见性错误消失）。**验证后删除这个探针函数。**

- [ ] **Step 3: 加通知 tick 的驱动 helper**

在 `tests.rs` 的 helper 区：

```rust
/// Drive ONE task-driver notification tick for `session_id` — the exact
/// body the spawned driver runs after its sweep, called directly so a test
/// can say "N completed delivery opportunities" instead of sleeping
/// `N × task_sweep_tick`.
///
/// What this deliberately does NOT prove: that the spawned loop calls it on
/// its interval. Two b2 tests keep running the real loop for that (the
/// audit's `KEEP-LOOP`: `deliver_a_new_notification_turn_when_the_agent_is_idle`
/// and `exit_the_driver_when_the_session_disappears`), so this seam stays
/// honest.
async fn drive_notification_tick(server: &EmbeddedServer, session_id: &str) {
    let session = server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("session is live");
    let resources = server.goal_chain_resources();
    super::task_driver::deliver_due_notifications(
        server.session_index(),
        &server.registry,
        &server.turn_flows,
        &server.activity_trackers,
        &server.steer_buffers,
        &server.compaction_handles,
        &server.usage_model_key_handles,
        &server.llm_factory,
        &server.config,
        server.flag_registry(),
        server.turn_stall_timeout,
        server.turn_backstop_timeout,
        &server.in_flight_turns,
        &server.goal_chain_states,
        &resources,
        &server.provider_auth_failures,
        server.home_dir().map(ToOwned::to_owned),
        &session,
        session_id,
    )
    .await;
}

/// Drive `ticks` notification ticks in sequence (the literal form of
/// "N ticks passed and nothing was delivered").
async fn drive_notification_ticks(server: &EmbeddedServer, session_id: &str, ticks: usize) {
    for _ in 0..ticks {
        drive_notification_tick(server, session_id).await;
    }
}
```

- [ ] **Step 4: 加 cron pass 的驱动 helper**

```rust
/// Drive ONE server-level cron pass for the default workspace (the
/// scheduler's `schedule_workspace_cron_pass`, with the orphan sweep
/// already latched so a repeated call is a pure due-check + drain).
async fn drive_cron_pass(server: &EmbeddedServer) {
    let workspace_id = server
        .workspace_registry()
        .default_workspace_id()
        .expect("the default workspace is set")
        .to_string();
    super::cron_driver::schedule_workspace_cron_pass(
        Arc::clone(&server.registry),
        Arc::new(server.workspace_run_spawner()),
        Arc::clone(&server.steer_buffers),
        workspace_id,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )
    .await;
}

/// Drive ONE session-scoped cron pass (the per-session half of the
/// scheduler tick: `check_due` → hold → purge → idle gate → drain).
async fn drive_session_cron_pass(server: &EmbeddedServer, session_id: &str) {
    super::cron_driver::schedule_session_cron_pass(
        Arc::clone(&server.registry),
        Arc::clone(&server.turn_flows),
        Arc::clone(&server.activity_trackers),
        Arc::clone(&server.steer_buffers),
        Arc::clone(&server.compaction_handles),
        Arc::clone(&server.usage_model_key_handles),
        server.llm_factory.clone(),
        Arc::clone(&server.in_flight_turns),
        server.session_index(),
        Arc::clone(server.flag_registry()),
        Arc::clone(&server.goal_chain_states),
        server.goal_chain_resources(),
        Arc::clone(&server.provider_auth_failures),
        Arc::clone(&server.config),
        server.turn_stall_timeout,
        server.turn_backstop_timeout,
        server.home_dir().map(ToOwned::to_owned),
        session_id.to_string(),
        Arc::clone(&server.cron_pass_panic_session),
    )
    .await;
}
```

`workspace_run_spawner()` 返回 `WorkspaceRunSpawner`（`cron_run_spawner.rs:102`，`pub(super)`）；`Arc::new(...)` 满足 `Arc<dyn WorkspaceFireExecutor>`。

- [ ] **Step 5: 加正向信号 helper**

```rust
/// The session's cron manager (the live session's own handle).
async fn session_cron(
    server: &EmbeddedServer,
    session_id: &str,
) -> Arc<mirri_core::cron::CronManager> {
    server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("session is live")
        .cron()
}

/// Wait until the scheduler has HELD `task_id`'s fire (the durable
/// write-before-inject record) — the positive signal the "several ticks
/// while the gate holds" windows proxied.
async fn wait_for_held_fire(server: &EmbeddedServer, session_id: &str, task_id: &str) {
    let cron = session_cron(server, session_id).await;
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || {
            cron.held_fires()
                .iter()
                .any(|fire| fire.task_id == task_id)
                .then_some(())
        },
    )
    .await
    .expect("the scheduler must hold the due fire");
}

/// Wait until the held queue is empty (the purge/inject/clear effect).
async fn wait_for_held_fires_cleared(server: &EmbeddedServer, session_id: &str) {
    let cron = session_cron(server, session_id).await;
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || cron.held_fires().is_empty().then_some(()),
    )
    .await
    .expect("the held queue must clear");
}

/// The session's live steer buffer (the same handle the driver's
/// adjudication reads through `parked_keys`). `steer_buffer_for` is
/// private to `embedded_server` — the `tests` subtree is a descendant, so
/// it is reachable; if a future refactor moves it, widen to `pub(super)`
/// rather than duplicating the lookup.
async fn session_steer_buffer(
    server: &EmbeddedServer,
    session_id: &str,
) -> Arc<std::sync::Mutex<mirri_core::SteerBuffer>> {
    let model_id = server
        .session_manager()
        .get_session(&SessionId::new(session_id))
        .await
        .expect("session is live")
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("main agent")
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .model_id
        .clone();
    server.steer_buffer_for(session_id, "main", &model_id)
}

/// Wait until `task_id`'s notification is parked in the steer buffer.
async fn wait_for_parked_notification(
    server: &EmbeddedServer,
    session_id: &str,
    task_id: &str,
) {
    let buffer = session_steer_buffer(server, session_id).await;
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || {
            // The guard's scope ends at the block: never across an await.
            let parked = {
                let buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
                mirri_core::task_notification::parked_keys(&buf)
            };
            parked.iter().any(|key| key.task_id == task_id).then_some(())
        },
    )
    .await
    .expect("the driver must park the notification");
}

/// Wait until the session's MAIN turn has settled. `poll_agent_activity`
/// hardcodes `agent_id: "main"`, so this is main-scoped by construction —
/// for a subagent-addressed turn use [`wait_for_agent_turn_idle`].
async fn wait_for_turn_idle(server: &EmbeddedServer, session_id: &str) {
    poll_agent_activity(server, session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;
}

/// Poll `agent.activity` for a SPECIFIC agent (main or a subagent child) —
/// the addressee-correct form of [`wait_for_turn_idle`].
/// `agent.activity` already takes an `agentId`
/// (`mod.rs:4767-4783` resolves it through `resolve_agent`), so this needs
/// no production change.
async fn poll_agent_activity_for(
    server: &EmbeddedServer,
    session_id: &str,
    agent_id: &str,
    mut check: impl FnMut(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = server
            .call_method(
                "agent.activity",
                json!({ "session_id": session_id, "agent_id": agent_id }),
            )
            .await
            .expect("agent.activity must answer");
        if check(&result) {
            return result;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "agent.activity for {agent_id} never satisfied the poll condition: {result}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until a specific agent's turn settles (the addressee-scoped
/// counterpart of [`wait_for_turn_idle`]).
async fn wait_for_agent_turn_idle(server: &EmbeddedServer, session_id: &str, agent_id: &str) {
    poll_agent_activity_for(server, session_id, agent_id, |state| {
        state["turn"].is_null()
    })
    .await;
}

/// Wait until the session's goal chain has stopped: the chain task clears
/// `chain_running` as its last act (`goal_driver.rs:553-564`). This — NOT
/// `goal.status != Active` — is the "no further continuation can land"
/// signal: `UpdateGoal` writes the terminal status DURING the turn
/// (`update_goal.rs:100`), well before the settle point runs the chain
/// decision.
async fn wait_for_goal_chain_stopped(server: &EmbeddedServer, session_id: &str) {
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || {
            server
                .goal_chain_states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(session_id)
                .is_none_or(|state| !state.chain_running)
                .then_some(())
        },
    )
    .await
    .expect("the goal chain must stop");
}
```

- [ ] **Step 6: 加手动压缩 apply 计数器（唯一的新计数器）**

`mod.rs`，与其他 `#[cfg(test)]` 字段同处：

```rust
    /// Test observation seam: manual-compaction apply tasks that have
    /// FINISHED (every arm — applied, cancelled, failed, panicked). The
    /// apply task's tail is where the cancel/settle/persist decisions land,
    /// so "the attempt wound down" is a counter wait, never a settle sleep.
    /// A counter (not a direct call) because the task is spawned from
    /// inside `dispatch_begin_compaction` and its `attempt.task` handle is
    /// not reachable from a test.
    #[cfg(test)]
    manual_compaction_applies: Arc<std::sync::atomic::AtomicU64>,
```

构造：

```rust
            #[cfg(test)]
            manual_compaction_applies: Arc::new(std::sync::atomic::AtomicU64::new(0)),
```

accessor（`session_is_busy` 旁）：

```rust
    /// Finished manual-compaction apply tasks.
    #[cfg(test)]
    pub(crate) fn manual_compaction_apply_count(&self) -> u64 {
        self.manual_compaction_applies
            .load(std::sync::atomic::Ordering::SeqCst)
    }

```

`compaction_handlers.rs` 的 `dispatch_begin_compaction`：`tokio::spawn` 之前

```rust
        #[cfg(test)]
        let apply_counter = Arc::clone(&self.manual_compaction_applies);
```

**spawn 体的第一条语句**是一个 drop guard——**不能**在 `match` 之后自增：

```rust
        tokio::spawn(async move {
            // The apply counter is incremented by DROP, not by a trailing
            // statement: the cancelled arm returns early
            // (`compaction_handlers.rs:240`, `if attempt.cancel.is_cancelled()
            // { …; return; }`), so a tail increment would miss exactly the
            // arm four of the waiting tests exercise.
            #[cfg(test)]
            let _apply_counted = CountOnDrop(Arc::clone(&apply_counter));
```

配合在 `compaction_handlers.rs` 的模块级（`#[cfg(test)]`）加：

```rust
/// Counts one finished manual-compaction apply task on drop — the arms
/// include the early `return` on cancellation, which a trailing increment
/// would skip.
#[cfg(test)]
struct CountOnDrop(Arc<std::sync::atomic::AtomicU64>);

#[cfg(test)]
impl Drop for CountOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
```

这样四条取消路径（#15828、#16695、#17028、#17093）也会计数；panic 展开时同样计数（drop guard 在 unwinding 时运行）。

测试 helper：

```rust
/// Wait until `expected` manual-compaction apply tasks have finished.
async fn wait_for_manual_compaction_apply(server: &EmbeddedServer, expected: u64) {
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || (server.manual_compaction_apply_count() >= expected).then_some(()),
    )
    .await
    .expect("the manual compaction's apply task must finish");
}
```

- [ ] **Step 7: 加 MCP 结算 accessor**

`McpStartupTiming`（`mod.rs:1747`）加：

```rust
    /// Whether this engine's startup connect set has FINISHED. Both halves
    /// are required: `outstanding == 0` is also the pristine initial state
    /// (a server whose set never spawned would read "settled" before
    /// anything ran), while `finished` is stamped only when the last
    /// outstanding task landed.
    #[cfg(test)]
    fn is_settled(&self) -> bool {
        self.started.is_some() && self.outstanding == 0
    }
```

**注意 `plugin.install` 不碰 `mcp_startup`**（实测：`plugin*.rs` 里 0 处引用；只有 `mod.rs:2939/3003/3078` 三处 `add_task()`）。因此 `assert_plugin_server_absent_for_a_while`（#44094）等它**永真**——见 Task 5 Step 4 的特殊处理。

accessor：

```rust
    /// Whether this engine's startup MCP connect set (connect + the plugin
    /// disable reconcile) has finished.
    #[cfg(test)]
    pub(crate) fn mcp_startup_settled(&self) -> bool {
        self.mcp_startup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_settled()
    }
```

helper：

```rust
/// Wait until the engine's startup MCP connect set has settled.
async fn wait_for_mcp_startup_settled(server: &EmbeddedServer) {
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || server.mcp_startup_settled().then_some(()),
    )
    .await
    .expect("the startup MCP connect set must settle");
}
```

- [ ] **Step 8: watcher 退出标志**

`skill_watcher.rs`：`SkillDirectoryWatcher` 加字段与 accessor：

```rust
pub struct SkillDirectoryWatcher {
    /// The shutdown signal — dropped (closing the channel) when this
    /// handle drops; the watcher task's select loop observes the close
    /// and exits.
    _shutdown: tokio::sync::oneshot::Sender<()>,
    /// Test observation: set by the task on its way out, so a test can wait
    /// for the exit instead of sleeping past it. Absent in production (a
    /// public accessor would be user-visible API for no product reason).
    #[cfg(test)]
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl SkillDirectoryWatcher {
    /// Whether the watcher task has observed the shutdown and exited.
    #[cfg(test)]
    pub(crate) fn has_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }
}
```

`spawn_skill_watcher` 里 `tokio::spawn` 之前：

```rust
    #[cfg(test)]
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(test)]
    let exit_flag = Arc::clone(&exited);
```

spawn 体末尾（`tracing::debug!(… "skill directory watcher task exited")` 之后）：

```rust
        #[cfg(test)]
        exit_flag.store(true, std::sync::atomic::Ordering::SeqCst);
```

构造：

```rust
    Some(SkillDirectoryWatcher {
        _shutdown: shutdown_tx,
        #[cfg(test)]
        exited,
    })
```

测试 helper：

```rust
// Defined INSIDE `skill_watcher.rs`'s own `#[cfg(test)] mod tests` — NOT in
// `embedded_server::tests`. `skill_watcher` and `embedded_server` are
// siblings under the crate root, so the latter's private test module is not
// reachable from here (E0603).
/// Wait until the watcher task has exited (its own flag, not a grace period).
async fn wait_for_watcher_exit(exited: &Arc<std::sync::atomic::AtomicBool>) {
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || exited.load(std::sync::atomic::Ordering::SeqCst).then_some(()),
    )
    .await
    .expect("the watcher task must exit once its handle drops");
}
```

（`skill_watcher.rs:708` 已有本地 `wait_until(Duration, || bool)`，若它可复用就直接用它，不必新增。）

- [ ] **Step 9: follow pass 抽取**

`follow_scheduler.rs` 的 tick body 抽出为自由函数（参数与 spawn 里的捕获一致）：

```rust
/// One follow-driver pass: the spawn's loop body, extracted so a test can
/// drive completed passes directly ("N passes absorbed nothing") instead of
/// sleeping `N × follow_tick`. Returns `false` when the driver should exit
/// (session gone / journal truncated).
pub(super) async fn follow_pass(
    registry: &Arc<WorkspaceRegistry>,
    in_flight_turns: &super::InFlightTurnsMap,
    session_id: &str,
) -> bool {
    // …原循环体原样搬入（in-flight 跳过也返回 true）…
}
```

**循环顺序必须保持"先 tick 后 pass"** —— 不要写成 `while follow_pass(...) { interval.tick().await; }`：`while` 的条件先求值，会让 pass #1 在第一次 tick 之前跑；而 tokio 的 `interval` 第一个 tick 立即完成，于是启动时会**连着跑两次 pass**（行为改动）。

正确形态：

```rust
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                if !follow_pass(&registry, &in_flight_turns, &sid).await {
                    break;
                }
            }
```

（原 spawn 体内的私有别名 `in_flight_prefix` 由 `follow_pass` 内部用 `format!("{session_id}:")` 重算，不再作为捕获变量。）

> 注意：`InFlightTurnsMap` 定义在 `embedded_server/mod.rs:1038`（`pub(super) type`），**不在** `mirri_core`——`follow_scheduler` 与 `tests` 都是 `embedded_server` 的后代模块，直接引用 `super::InFlightTurnsMap` 即可。

- [ ] **Step 10: 加 driver tick/exit 探针**

`mod.rs`：

```rust
    /// Test observation seam: completed task-driver passes per session, and
    /// drivers that exited because their session disappeared. Only two uses
    /// need a counter rather than a direct call: "N passes ran" for a test
    /// that must keep the REAL spawned loop, and the loop-exit assertion.
    #[cfg(test)]
    task_driver_ticks: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    #[cfg(test)]
    task_driver_exits: Arc<std::sync::atomic::AtomicU64>,
```

构造：

```rust
            #[cfg(test)]
            task_driver_ticks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            #[cfg(test)]
            task_driver_exits: Arc::new(std::sync::atomic::AtomicU64::new(0)),
```

accessor：

```rust
    /// Completed task-driver passes for one session. 0 when none ran.
    #[cfg(test)]
    pub(crate) fn task_driver_tick_count(&self, session_id: &str) -> u64 {
        self.task_driver_ticks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Task drivers that have exited on this server (session gone).
    #[cfg(test)]
    pub(crate) fn task_driver_exit_count(&self) -> u64 {
        self.task_driver_exits
            .load(std::sync::atomic::Ordering::SeqCst)
    }
```

`task_driver.rs` 的 spawn 体：`resolve_session_via` 失败分支里 `break` 之前 `tick_cells.1.fetch_add(1, SeqCst);`；`deliver_due_notifications(...).await;` 之后自增 per-session tick（见实现细节：绑定 `let tick_cells = (Arc::clone(&self.task_driver_ticks), Arc::clone(&self.task_driver_exits));` 于 spawn 之前，加 `#[cfg(test)]`）。

helper：

```rust
/// Wait until the spawned task driver has exited (its own counter).
async fn wait_for_task_driver_exit(server: &EmbeddedServer, before: u64) {
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || (server.task_driver_exit_count() > before).then_some(()),
    )
    .await
    .expect("the task driver must exit once its session disappears");
}
```

- [ ] **Step 11: 转 #758（b2 的 `exit_the_driver_when_the_session_disappears`）**

> **`task_driver_ticks` / `task_driver_tick_count` 的消费者：** Task 4 Step 1 的 #3 例外说明里会用到它（`KEEP-LOOP` 的 #3 保留真实 loop，但用 tick 计数把"跑过几轮"变成可断言的量）。若实施时决定不给 #3 加这条断言，就**删掉这两个字段与 accessor**，只留 `task_driver_exits`（#758 用它）——否则 Task 7 的 clippy 会因 `dead_code` 失败。

b2 的 `exit_the_driver_when_the_session_disappears`：

```rust
    // When the session closes, the driver observes the disappearance on its
    // next pass and exits — the exit counter is the signal (the loop stops
    // counting passes once it breaks, so a tick count cannot express this).
    let exits_before = server.task_driver_exit_count();
    server
        .call_method("session.close", json!({ "session_id": &session_id }))
        .await
        .expect("close");
    wait_for_task_driver_exit(&server, exits_before).await;
```

- [ ] **Step 12: 全量编译 + clippy**

```bash
cargo clippy --manifest-path rust/Cargo.toml -p mirri-embedded --all-targets -- -D warnings \
  > .tmp/negwin/t3-clippy.log 2>&1; tail -20 .tmp/negwin/t3-clippy.log
```

**预期：本步会报 `dead_code`，这是正常的。** 本任务只加设施，Task 4/5/6 才消费它们——在这个中间态，那些 helper 与 `pub(crate)` accessor 确实无人调用。

处理方式：给测试 helper 区块加一个**带到期说明**的属性，不要删设施：

```rust
// These helpers/platform probes are consumed by Tasks 4-6 of the
// determinization plan; they are dead until then.
#[allow(dead_code)]
```

（加在 helper 区块所在的 `mod`/区块即可，不要逐个函数加。）**Task 7 Step 4 的 workspace clippy 是真正的零告警门禁**——那里如果还有 `dead_code`，说明某个 helper 没被任何测试用上，删掉它。

`#[cfg(test)]` 字段的处理同理：Task 3 保留，Task 7 若仍无人读则删除。

- [ ] **Step 13: 提交**

```bash
git add -A rust/crates/mirri-embedded
git commit -m "test: expose the driver pass bodies so tests can drive them directly"
```

---

### Task 4: b2 的 41 处转换

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs`

**Interfaces:**
- Consumes: Task 3 的 `drive_notification_ticks` / `drive_session_cron_pass` / `wait_for_held_fire` / `wait_for_held_fires_cleared` / `wait_for_parked_notification` / `wait_for_turn_idle` / `wait_for_agent_turn_idle` / `wait_for_task_driver_exit`。（**不用** `drive_cron_pass`——它驱动 workspace 级 cron，本任务的两个 cron 站点是 session 级。）

- [ ] **Step 1: 先转 tick 类 20 处**

> **例外（不要动）：** #3 `deliver_a_new_notification_turn_when_the_agent_is_idle` 与 #9 `exit_the_driver_when_the_session_disappears` 是 `KEEP-LOOP`——它们必须跑真实 spawned loop。#3 本来就没有窗口（只做新增断言更不许改）；#9 已在 Task 3 Step 11 换成 `wait_for_task_driver_exit`。下表不含这两个测试。

按下表把 `tokio::time::sleep(...) // hygiene-allow: ...` 整行替换为 `drive_notification_ticks(&<server>, &session_id, <N>).await;`，并保留原有注释（把"N ticks (300ms)"改成"N 次投递裁决"）：

| 站点行 | 测试 | server 变量 | N |
|---|---|---|---|
| 380 | `not_deliver_a_duplicate_notification_on_later_ticks` | `server` | 6 |
| 812 | `not_notify_when_the_task_was_stopped_by_taskstop` | `server` | 6 |
| 898 | `suppress_notifications_for_tasks_killed_at_session_close` | `server_b` | 6 |
| 1010 | `suppress_notifications_when_print_mode_drains` | `server` | 6 |
| 1141 | `silently_reappend_undelivered_notifications_on_resume` | `server_b` | 6 |
| 1295 | `redeliver_the_notification_after_the_turn_is_undone` | `server` | 6 |
| 1390 | `keep_the_notification_delivered_when_nothing_is_undone` | `server` | 6 |
| 1533 | `not_redeliver_a_notification_when_compaction_drops_it_from_context` | `server` | 12 |
| 1662 | `not_create_an_llm_on_ticks_after_the_delivery_settles` | `server` | 6 |
| 2030 | `leave_a_mixed_buffer_untouched_until_the_next_turn` | `server` | 6 |
| 2098 | `leave_a_mixed_buffer_untouched_until_the_next_turn` | `server` | 2 |
| 2641 | `not_redeliver_a_subagent_notification_after_the_session_is_resumed` | `server_b` | 6 |
| 2699 | 同上 | `server_c` | 6 |
| 2893 | `re_park_a_pending_subagent_notification_steer_on_resume` | `server` | 2 |
| 3180 | `not_re_park_a_pre_fix_subagent_notification_steer_into_main_on_resume` | `server_b` | 6 |
| 3411 | `silently_reappend_a_notification_whose_steer_was_flushed_but_never_delivered_on_resume` | `server_b` | 6 |
| 3630 | `silently_reappend_undelivered_subagent_notifications_on_resume` | `server_b` | 6 |
| 3831 | `hold_the_notification_delivery_while_the_goal_is_paused` | `server` | 6 |
| 3925 | `hold_the_stranded_notification_flush_while_the_goal_is_paused` | `server` | 6 |
| 4163 | `pause_the_goal_when_message_abort_cancels_the_notification_turn` | `server` | 6 |

- [ ] **Step 2: 转正向信号 18 处**

| 站点行 | 替换 |
|---|---|
| 453 | `wait_for_parked_notification(&server, &session_id, &task_id).await;`（在 `release.send` 之前） |
| 586 | `wait_for_parked_notification(&server, &session_id, &task_id).await;`（把 `_task_id` 绑定改名为 `task_id`） |
| 1440 | **不要等 marker**（`run_notification_turn` 在 LLM 调用前就 mark，见语义陷阱表）→ `wait_for_turn_idle(&server, &session_id).await;` |
| 1728 | `wait_for_held_fire(&server, &session_id, "cron-b2").await;` |
| 1846 | `wait_for_held_fire(&server, &session_id, &task_id).await;` |
| 1866 | `wait_for_held_fires_cleared(&server, &session_id).await;` |
| 1932 | `wait_for_held_fire(&server, &session_id, &task_id).await;` |
| 1953 | `wait_for_held_fires_cleared(&server, &session_id).await;` |
| 2350 | `wait_for_agent_turn_idle(&server, &session_id, &child_agent_id).await;`（投递对象是子 agent；main 早已 idle） |
| 2457 | `wait_for_agent_turn_idle(&server, &session_id, &subagent_id).await;` |
| 2603 | `wait_for_agent_turn_idle(&server_a, &session_id, &child_agent_id).await;` — 原窗口保证子 agent 的投递 turn **已结束**再快照 `calls_after_delivery`，必须等对 addressee |
| 2950 | `wait_for_agent_turn_idle(&server, &session_id, &child_agent_id).await;` —— **不要**用 main-scoped 版本：resumed server B 的 main 可能没有 `lastTurn`，轮询会 5s 超时 |
| 4402 | `wait_for_agent_turn_idle(&server, &session_id, &subagent_id).await;` |
| 4748 | `wait_for_held_fire(&server, &session_id, "cron-gate").await;` |
| 4852 | `wait_for_held_fire(&server, &session_id, "cron-held").await;` |
| 4868 | 等 turn 落定（`!session_is_busy`，因为会话此时已 close）：`mirri_test_support::wait_for::wait_for_async(Duration::from_secs(5), Duration::from_millis(5), \|\| (!server.session_is_busy(&session_id)).then_some(())).await.expect("the released turn must settle");` |
| 5040 | `wait_for_held_fire(&server, &session_id, "cron-merge").await;` |
| 5265 | 同 4868 的 `!session_is_busy` 等待 |

- [ ] **Step 3: 转 cron pass 2 处**

> **这两个是 SESSION 级 cron**（`seed_overdue_one_shot` / `session.cron().restore_tasks(...)`），所以驱动的是 `schedule_session_cron_pass`，不是 workspace 那条。`drive_cron_pass` 只驱动 workspace 的 `cron.add`（Task 6 的 #626 用它）。

| 站点行 | 替换 |
|---|---|
| 4983 (`should_cascade_clear_held_fires_when_the_session_is_deleted`) | `drive_session_cron_pass(&server, &session_id).await;` 两次。会话已被删除，因此该 pass 在 `resolve_session_via` 处早退——正是"已删会话不再注入"的证明 |
| 5163 (`should_clear_the_held_fire_when_the_llm_provider_cannot_be_created`) | `drive_session_cron_pass(&server, &session_id).await;` 两次（hold 清除后不会重建） |

- [ ] **Step 4: 核对 #758 已在 Task 3 转好**

`exit_the_driver_when_the_session_disappears`（`KEEP-LOOP`）的窗口已在 Task 3 Step 11 换成 `wait_for_task_driver_exit`。本步只做核对：

```bash
start=$(grep -n "fn should_exit_the_driver_when_the_session_disappears" \
  rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs | cut -d: -f1)
sed -n "${start},$((start+30))p" \
  rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs \
  | grep -n "hygiene-allow\|wait_for_task_driver_exit"
```

预期：该测试体内只剩 `wait_for_task_driver_exit`，无 `hygiene-allow`。（按名定位，不用固定行号——Task 3 已改过这个文件。）

**每个任务结束时都用 grep 复核注解数**（扫描器不统计带注解的行，`--report` 永远看不出注解减少了多少）：

```bash
grep -rn "hygiene-allow" rust/crates --include='*.rs' | wc -l
```

- [ ] **Step 5: 跑 b2 全量**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
  embedded_server::tests::b2_notification_tests > .tmp/negwin/t4-b2.log 2>&1; tail -25 .tmp/negwin/t4-b2.log
```

后台任务。预期：全绿（b2 文件内注解归零：本任务 40 处 + Task 3 的 #758）。**若出现"断言在信号到达前被求值"的失败**，说明该站点依赖的是 tick 的**其他阶段**（`terminate_expired` / stale-warn，`task_driver.rs:152-209`）而非投递裁决——此时必须回到真实 spawned loop（用 `task_driver_tick_count` 等 tick 数），**不能**用单个信号糊过去，并在提交 message 里注明。这是硬检查，不是注释。

- [ ] **Step 6: 门禁 + 提交**

```bash
node rust/scripts/check-test-hygiene.mjs
git add -A rust/crates/mirri-embedded
git commit -m "test: drive the notification delivery instead of waiting for it"
```

预期注解数：`78 - 40 = 38`（b2 共 41 处；#758 由 Task 3 转换，本任务转其余 40 处，b2 文件到此归零）。

---

### Task 5: `tests.rs` 的 17 处（#44094 保留）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`

**Interfaces:**
- Consumes: Task 3 的 `wait_for_task_driver_exit` 与 apply/MCP/正向信号 helper。

- [ ] **Step 1: `tests.rs` 6 处删窗口（其中 2 处必须先补信号）**

| 站点 | 测试 | 做法 |
|---|---|---|
| 245 | `should_wait_for_census_and_recovery_before_cron_first_tick` | **补信号**后删窗（见下） |
| 5935 | `should_launch_a_goal_continuation_turn_when_a_turn_completes_with_an_active_goal` | **改信号**为 `wait_for_goal_chain_stopped`（见下），再删窗 |
| 6136 | `should_not_launch_a_continuation_when_background_tasks_are_in_flight` | 直接删窗：`mode: "wait"` 在返回前已跑完 `maybe_launch_goal_chain`（`message_handlers.rs:1896-1904`） |
| 6225 | `should_pause_the_goal_after_repeated_idle_continuation_turns` | 直接删窗：pause 由"本该启动下一次"的同一决策写入（`goal_driver.rs:286-317`），观测到 `Paused` 即排除再启动 |
| 6318 | `should_pause_the_goal_when_the_absolute_continuation_limit_is_reached` | 同上 |
| 15615 | `should_skip_with_info_notice_when_manual_compaction_has_nothing_to_compact` | 直接删窗：`CompactionUnable` 早在 `tokio::spawn` 之前返回（`compaction_handlers.rs:174-186`） |

**#245 的正确做法**——原窗口是唯一证明"调度器走到了 gate"的观测点；只断言 `census.get().is_none()` 在 t=0 也为真，是空断言。给既有的 `hold_probe` 加一个"被问过"标志：

```rust
    // The probe records that the gate was actually CONSULTED — that is what
    // the deleted window observed (the scheduler reaching its first-tick
    // gate), and it cannot be inferred from the cell still being empty.
    let census_asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked_flag = Arc::clone(&census_asked);
    let census_released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let released_for_probe = Arc::clone(&census_released);
    server.set_settlement_probe(Some(Arc::new(move |asked: &'static str| {
        if asked == "census" {
            asked_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        asked != "census" || released_for_probe.load(std::sync::atomic::Ordering::SeqCst)
    })));
    // … spawn the dispatch exactly as the existing test does …
    mirri_test_support::wait_for::wait_for_async(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(5),
        || census_asked.load(std::sync::atomic::Ordering::SeqCst).then_some(()),
    )
    .await
    .expect("the scheduler's first tick must reach the census gate");

    // Then the gate is held (the probe cannot be lifted from inside) and
    // nothing this instance owns has been registered yet.
    assert!(
        server.housekeeping_settled().census.get().is_none(),
        "the held census must not settle while the hold is up"
    );
    assert!(
        !first_dispatch.is_finished(),
        "the first dispatch is parked on the held census gate"
    );
```

**#5935 的正确做法**——plan 旧稿的理由（"goal 离开 Active ⇒ chain gate 已返回"）是错的：`UpdateGoal` 在 turn 进行中就写 `Complete`（`update_goal.rs:100`），而 `wait_for_turn_prompts(…, 2)` 在第二个 turn **开始时**即满足。必须等 chain 任务自己退出：

```rust
    // The chain stopped: the chain task clears `chain_running` as its last
    // act. Neither `goal.status` nor the turn.prompt count proves this —
    // UpdateGoal writes the terminal status mid-turn, and the second
    // turn.prompt is recorded at that turn's START.
    wait_for_goal_chain_stopped(&server, &session_id).await;
    let ops = read_wire_ops(&server, &session_id).await;
    let turn_prompts = ops
        .iter()
        .filter(|op| matches!(op, WireOp::TurnPrompt { .. }))
        .count();
    assert_eq!(
        turn_prompts, 2,
        "no third turn.prompt may follow the completed goal: {ops:?}"
    );
```

- [ ] **Step 2: `tests.rs` 8 处换 apply 计数器**

| 站点 | 测试 | 做法 |
|---|---|---|
| 15914 | PostCompact hook 在取消时不触发 | `let before = server.manual_compaction_apply_count();` → 释放 gate 后 `wait_for_manual_compaction_apply(&server, before + 1).await;` |
| 16531 | must-shrink 拒绝不留 apply 记录 | 同上；**并删掉那条 `TODO(time-based absence check)` 注释块** |
| 16781 | 取消事件后允许再次 begin | 同上（teardown settle） |
| 16841 | 第二个 begin 被拒 | 同上 |
| 17008 | turn 活跃时拒绝 | 同上 |
| 17114 | 陈旧 attempt 结算 | 释放 `first_gate` 后等 `before + 1` |
| 17131 | 同上（清理） | 释放 `second_gate` 后等 `before + 1` |
| 17179 | 取消后不发 completed 事件 | 同上 |

- [ ] **Step 3: `tests.rs` 1 处换信号 + 1 处结构性改写**

- **16083**（`should_buffer_prompt_when_manual_compaction_in_flight_until_settled`）

  **不要**只断言 `manual_in_flight()`：那断言的是被测机制本身——把 `wait_manual_compaction_settle`（`turn_flow/mod.rs:1249`，正是本测试守护的回归）删掉，`manual_in_flight()` 仍为真、`chat_call_count` 在断言的瞬间仍为 3，测试照样通过。

  正确做法：窗口的契约是"这个 send 已经进入 turn 并被 park 住"。改成"send 任务仍在跑 + 计数稳定"：

```rust
    // The buffered send must have ENTERED the turn and be parked there —
    // that is what the deleted window observed. `manual_in_flight()` alone
    // cannot show it: it stays true even if the settle wait is removed
    // (the exact regression this test guards).
    assert!(
        !send_task.is_finished(),
        "the mid-compaction message must still be parked, not answered"
    );
    assert_eq!(
        mock.chat_call_count(),
        3,
        "turn must not reach the LLM while the manual compaction is in flight"
    );
    // Re-check after a beat: a turn that slipped past the settle wait would
    // pop the fourth scripted reply (the buffered turn's).
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await; // hygiene-allow: poll step (the sanctioned half of a stability re-check)
    assert!(
        !send_task.is_finished(),
        "the buffered message must stay parked until the compaction settles"
    );
```

  若这个 `hygiene-allow` 被判为新增违规（见 Global Constraints 的相对判据），改用 `wait_for` 的轮询形态，**不要**保留窗口。

- **36279**（`should_keep_a_silent_turn_alive_when_turn_stall_timeout_ms_is_zero`）

  删窗口，但**先等 turn 可见**再断言（否则 fire-mode send 后立刻查 `session_is_busy` 会引入新 flake）：

```rust
    // The watchdog cannot fire at all: `turnStallTimeoutMs = 0` arms an
    // unreachable deadline (`Duration::MAX`), so "no failure frame" is a
    // property of the armed window, not of a grace period.
    assert_eq!(
        server.turn_stall_timeout(),
        std::time::Duration::MAX,
        "a zero turnStallTimeoutMs must disarm the watchdog, not shorten it"
    );
    // The turn must be visibly in flight BEFORE the assertion — the deleted
    // window used to provide that ordering.
    mirri_test_support::wait_for::wait_for_async(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(5),
        || server.session_is_busy(&session_id).then_some(()),
    )
    .await
    .expect("the silent turn must register in flight");
    assert!(
        server.session_is_busy(&session_id),
        "a disabled watchdog must not cancel the silent turn"
    );
    while let Ok(notification) = events.try_recv() {
        assert!(
            !(notification.method == "event.turn.ended"
                && notification.params["agentId"] == json!("main")
                && notification.params["reason"] == json!("failed")),
            "a disabled watchdog must not cancel the silent turn: {}",
            notification.params
        );
    }
```

- [ ] **Step 4: `tests.rs` 1 处换 MCP 结算 + 1 处保留**

- **23346**（`should_not_connect_a_plugin_server_disabled_by_the_first_call`）：把 5×100ms 循环换成

```rust
    // The startup connect task's LAST act is the disable reconcile, so a
    // finished task makes the absence final — no sampling window.
    let runtime_name = "plugin-demo-plugin:helper";
    wait_for_mcp_startup_settled(&server).await;
    let listed = server
        .call_method("mcp.global.list", json!({}))
        .await
        .expect("mcp.global.list");
    assert!(
        !listed
            .as_array()
            .expect("the list result is an array")
            .iter()
            .any(|entry| entry["name"].as_str() == Some(runtime_name)),
        "a plugin server disabled by the first call must never stay connected: {listed}"
    );
```

- **44180**（`assert_plugin_server_absent_for_a_while` helper）：**保留窗口**。`plugin.install` 不碰 `mcp_startup`（实测 0 处引用；只有 `mod.rs:2939/3003/3078` 三处 `add_task()`），所以 `mcp_startup_settled()` 对它永真，改成等结算会塌缩为一次瞬时采样，漏掉"install 异步触发连接"的回归。只把 reason 改精确：

```rust
        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // hygiene-allow: absence sample (plugin.install never touches McpStartupTiming; an async connect regression needs a real window to catch)
```

- [ ] **Step 5: 跑受影响测试 + 门禁**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib > .tmp/negwin/t5-lib.log 2>&1; tail -25 .tmp/negwin/t5-lib.log
node rust/scripts/check-test-hygiene.mjs
```

后台任务。预期：全绿；注解数 `38 - 17 = 21`（#44094 按语义陷阱表改为**保留**）。

- [ ] **Step 6: 提交**

```bash
git add -A rust/crates/mirri-embedded
git commit -m "test: assert settled state instead of sampling windows in the embedded suite"
```

---

### Task 6: 其余文件的 12 处（另有 8 处按审计保留）

**Files:**
- Modify: `tests/cron_ws_4b_tests.rs`（4 处转换）
- Modify: `tests/t_session_consistency_tests.rs`（3 处转换）
- Modify: `tests/cron_ws_4a_tests.rs`（1 处转换）
- Modify: `src/skill_watcher.rs`（1 处转换，文件同时承载 Task 3 的 `#[cfg(test)]` 退出标志）
- Modify: `mirri-core/src/subagent/host.rs`（1 处转换，仅测试内 fixture）
- Modify: `mirri-test-harness/tests/mock_llm_tests.rs`（1 处转换）
- Modify: `embedded_server/orphan_governance.rs`（1 处转换，测试辅助函数）
- 保留不改（只核对 reason）：`tests/b1_print_mode_tests.rs`（1）、`mirri-acp/tests/conformance.rs`（1）、`mirri-core/src/subagent/swarm_batch.rs`（6）

**Interfaces:**
- Consumes: Task 3 的 helper 与 `follow_pass`。

- [ ] **Step 1: cron_ws_4b 的 4 处**

| 站点 | 测试 | 做法 |
|---|---|---|
| 626 | `should_drop_an_unopened_execution_session_after_seven_days` | 窗口换 `drive_cron_pass(&server).await;` 两次（第一次可能读在写入前，第二次必然观测到） |
| 806 | `should_run_the_full_create_to_rejection_chain_through_the_real_tool` | 换 `poll_agent_activity(&server, &session_id, \|state\| state["turn"].is_null() && state["lastTurn"].is_object()).await;` |
| 1027 | `should_park_a_queue_posture_fire_in_the_single_backlog_slot_while_running` | 换 `wait_for` 等 queued slot 出现（见下）。**前提已核实**：`try_open_run` 在 runs 互斥锁内同步写入（`workspace_store.rs:1055-1115`），且两次 `cron.trigger` 都被 `await`，所以 RPC 返回时 queued 记录已落盘——等第一次出现即已定案 |
| 1371 | `should_not_create_rows_for_cron_execution_sessions_until_explicitly_opened` | 换 `wait_for_async` 等 `index.get_session(&session_id).is_some()` |

1027 的替换（queued slot 在 runs 互斥锁内合并写入，出现即已定案）：

```rust
    // The queued slot is written under the runs mutex and merges one-deep,
    // so waiting for its appearance is enough: a second trigger can only
    // replace the same slot, never append a second record.
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(5),
        || {
            cron.runs(&task_id)
                .iter()
                .any(|run| {
                    run.status == mirri_core::cron::RunStatus::Skipped
                        && run
                            .skip_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("单槽排队"))
                })
                .then_some(())
        },
    )
    .await
    .expect("the fire must park in the single backlog slot");
```

- [ ] **Step 2: t_session_consistency 的 3 处**

| 站点 | 做法 |
|---|---|
| 163 | `drive_follow_passes(&server_b, &session_id, 3).await;` —— **但先补顺序**：该测试的"无重复"断言在 **event channel** 上（`events.try_recv()`），而 relay 由 spawn 的 forwarder 转发，3 次同步 pass 可能在帧被转发前就返回。改为先等 memory marker（已有 `marker_message_count`）再 drain channel，见下方专段 |
| 680 | 同上（3 次），随后保留 `assert!(server_b.session_is_busy(&session_id), …)` 作为前置校验 |
| 695 | 同上（3 次） |

封装（放 `tests.rs`）：

```rust
/// Drive `passes` completed follow-driver passes for one session. A pass
/// that skips for the in-flight guard counts as completed (that skip IS
/// what the in-flight test asserts).
async fn drive_follow_passes(server: &EmbeddedServer, session_id: &str, passes: usize) {
    for _ in 0..passes {
        // `tests` is `embedded_server::tests`, so its submodules need TWO
        // `super`s to reach `embedded_server::follow_scheduler`.
        let keep_going = crate::embedded_server::follow_scheduler::follow_pass(
            &server.registry,
            &server.in_flight_turns,
            session_id,
        )
        .await;
        assert!(keep_going, "the follow driver exited unexpectedly");
    }
}
```

**#163 的顺序修正**（`should_follow_foreign_writes_when_another_server_records_ops_on_shared_home`）：

```rust
    // Three passes, each of which finds the anchor advanced and absorbs
    // nothing new. The idempotence claim lives in MEMORY (the marker count),
    // not on the event channel: the relay forwards through a spawned task,
    // so a channel-only assertion could sample before a duplicate frame
    // would have been forwarded.
    drive_follow_passes(&server_b, &session_id, 3).await;
    assert_eq!(
        marker_message_count(&server_b, &session_id, "foreign hello from A").await,
        1,
        "later passes must not duplicate the absorbed message"
    );
```

（channel 上的断言可以保留作为额外证据，但**不能**作为唯一证据。）

- [ ] **Step 3: cron_ws_4a 的 1 处（914）**

```rust
    // The reboot's import settles through the census gate — waiting for the
    // housekeeping is the signal; no window (and no second throttled list
    // call) is needed.
    server2.await_housekeeping_settled().await;
    let listed = server2
        .call_method("cron.listAll", json!({}))
        .await
        .expect("cron.listAll");
```

- [ ] **Step 4: skill_watcher 的 1 处（993）**

```rust
        // When the handle drops (the registry-remove path)
        let watcher_exited = Arc::clone(&watcher.exited);
        drop(watcher);
        // The task's own exit flag — not a grace period.
        wait_for_watcher_exit(&watcher_exited).await;
```

- [ ] **Step 5: host.rs 的 1 处（4847）**

给测试内 fixture 加计数器（**零生产改动**）：

三处都要改（缺一处就编译不过）：

```rust
    struct FlippingLlmFactory {
        mode: Arc<std::sync::atomic::AtomicBool>,
        /// Test observation: how many times a turn reached the LLM.
        chat_calls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl SubagentLlmFactory for FlippingLlmFactory {
        fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
            Ok(Arc::new(FlippingLlm {
                mode: Arc::clone(&self.mode),
                capability: ModelCapability::default(),
                chat_calls: Arc::clone(&self.chat_calls),
            }))
        }
    }

    struct FlippingLlm {
        mode: Arc<std::sync::atomic::AtomicBool>,
        capability: ModelCapability,
        chat_calls: Arc<std::sync::atomic::AtomicU64>,
    }

    impl Llm for FlippingLlm {
        // …
        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.chat_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // … existing body unchanged …
        }
    }
```

测试侧要先拿到那个 `Arc`（**现有代码没有这个绑定，必须新增**），并且宿主构造时把它传进去：

```rust
        let mode = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let chat_calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let host = SessionSubagentHost::new(
            // … unchanged args …
            Arc::new(FlippingLlmFactory {
                mode: Arc::clone(&mode),
                chat_calls: Arc::clone(&chat_calls),
            }),
            // … unchanged args …
        );
```

断言处：

```rust
        // The turn reached the LLM (a pending stream by construction) and
        // therefore cannot have finished — the "the turn started" signal
        // replaces the sampling window.
        mirri_test_support::wait_for::wait_for_async(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(5),
            || (chat_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1).then_some(()),
        )
        .await
        .expect("the child turn must reach the LLM");
        assert!(
            !task.is_finished(),
            "the child turn should still be parked on the LLM"
        );
```

- [ ] **Step 6: mock_llm_tests 的 1 处（129）**

```rust
    mirri_test_support::wait_for::wait_for_async(
        Duration::from_secs(5),
        Duration::from_millis(1),
        || (mock.delta_yield_count() >= 2).then_some(()),
    )
    .await
    .expect("deltas must start flowing before the cancellation");
```

- [ ] **Step 7: orphan_governance 的 1 处（378）**

```rust
    /// Poll until `predicate` holds or ~5 s elapse (the sanctioned
    /// deadline-bounded poll form, not a fixed window).
    fn eventually<T>(mut predicate: impl FnMut() -> Option<T>) -> Option<T> {
        mirri_test_support::wait_for::wait_for(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(100),
            predicate,
        )
        .ok()
    }
```

- [ ] **Step 8: 梳理 8 处保留的 reason**

| 站点 | 新 reason |
|---|---|
| `swarm_batch.rs:1459` | `scenario pacing (virtual clock — keeps member N observably in-flight when member 0's rate limit settles, so the requeue branch runs)` |
| `swarm_batch.rs:1716` | `scenario pacing (virtual clock — the failure settles AFTER the cancellation: the exact race shape under test)` |
| `swarm_batch.rs:1812` | `scenario pacing (virtual clock — member work time the concurrency shrink measures against)` |
| `swarm_batch.rs:1842` | 同 1812 |
| `swarm_batch.rs:2413` | `scenario pacing (virtual clock — member 0's 1s of consumed budget before the rate limit)` |
| `swarm_batch.rs:2706` | `scenario pacing (virtual clock — silent-member work ticks; only advancing members feed the watchdog)` |
| `conformance.rs:883` | `real-io absence sample (pgrep over a spawned child's descendants — no in-process observable exists)` |
| `b1_print_mode_tests.rs:111` | 保持原文（`scenario pacing (the task's simulated 300ms work — the drain timing under the 1s ceiling is the point)`） |

- [ ] **Step 9: 跑受影响 crate + 门禁**

```bash
cargo test --manifest-path rust/Cargo.toml -p mirri-core --lib -- subagent:: 2>&1 | tail -8
cargo test --manifest-path rust/Cargo.toml -p mirri-test-harness 2>&1 | tail -8
# 预期：除 3 个【预存】失败（cross_backend_contract 的 restore 测试，见「基线事实」）外全绿
cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
  embedded_server::tests::t_session_consistency_tests \
  embedded_server::tests::cron_ws_4a_tests \
  embedded_server::tests::cron_ws_4b_tests \
  skill_watcher 2>&1 | tail -12
node rust/scripts/check-test-hygiene.mjs
```

预期：全绿；注解数 `21 - 12 = 9`。全仓剩余 9 处注解 = 本任务保留的 8 处（`swarm_batch` ×6、`conformance.rs` ×1、`b1_print_mode_tests.rs` ×1）+ Task 5 保留的 `tests.rs:44094` ×1。

- [ ] **Step 10: 提交**

```bash
git add -A rust/crates
git commit -m "test: convert the remaining observation windows to settled signals"
```

---

### Task 7: 验证、基线重锚、ROADMAP 回写

**Files:**
- Modify: `rust/scripts/test-hygiene-baseline.json`（重锚）
- Modify: `docs/specs/rust-core-design/ROADMAP.md`
- Create（仅当 Task 2 的 `dispose()` 改动落地）：`.changeset/<slug>.md`

- [ ] **Step 1: 确认站点清单**

```bash
grep -rn "hygiene-allow" rust/crates --include='*.rs' | sort | tee .tmp/negwin/sites-after.txt | wc -l
```

预期：`9` —— `mirri-core/src/subagent/swarm_batch.rs` ×6、`mirri-acp/tests/conformance.rs` ×1、`mirri-embedded/src/embedded_server/tests/b1_print_mode_tests.rs` ×1、`mirri-embedded/src/embedded_server/tests.rs:44094` ×1。

```bash
grep -rn "hygiene-allow" rust/crates --include='*.rs' | sed 's|rust/crates/||'
```

逐条核对这 8 个 reason 是否都是**具体**理由（见 Task 6 Step 8 的对照表）；任何一条还写着泛化的 `negative window (proving absence)` 就是漏转，回到对应任务补。

- [ ] **Step 2: 重锚 baseline 并证明未增长**

```bash
node -e "const b=require('./rust/scripts/test-hygiene-baseline.json').violations;const k=Object.keys(b);console.log(k.length,JSON.stringify(Object.values(b).reduce((a,v)=>(a[v.type+'|'+v.reason.split(':')[0]]=(a[v.type+'|'+v.reason.split(':')[0]]||0)+1,a),{})));" | tee .tmp/negwin/baseline-before.txt
node rust/scripts/check-test-hygiene.mjs --report | tee .tmp/negwin/report-before.txt
node rust/scripts/check-test-hygiene.mjs --write-baseline
node rust/scripts/check-test-hygiene.mjs
node -e "const b=require('./rust/scripts/test-hygiene-baseline.json').violations;const k=Object.keys(b);console.log(k.length,JSON.stringify(Object.values(b).reduce((a,v)=>(a[v.type+'|'+v.reason.split(':')[0]]=(a[v.type+'|'+v.reason.split(':')[0]]||0)+1,a),{})));" | tee .tmp/negwin/baseline-after.txt
```

验收（按序）：
1. 重锚后 `node rust/scripts/check-test-hygiene.mjs` 输出 `gate ok: 0 new violations`（重锚的作用正是把漂移条目重新锚定，所以这条在重锚后应当成立）；
2. AFTER 条目数 **≤** BEFORE（本批只删 sleep、不新增裸 sleep）；
3. reason 类别集合不新增（仍只有 `sleep|poll-step`、`sleep|legacy`、`sleep|negative-wait`、`set_var|legacy`；`negative-wait` 应下降）。

任一不满足就**不要提交**重锚结果，先修代码。

**注意：重锚会把这 199 条既有债务一并写进 baseline。** 这是可接受的（它们本来就在树里，且本批不引入它们），但必须在 PR 描述里写明「baseline 条目数从 429 变化到 N，其中新增的条目全部是重锚带来的行号位移与既有债务，本批未新增任何裸 sleep」——否则 reviewer 会以为你在用重锚掩盖新债。

- [ ] **Step 3: 提交重锚**

```bash
git add rust/scripts/test-hygiene-baseline.json
git commit -m "test: re-anchor the hygiene baseline after the negative-window conversions"
```

- [ ] **Step 4: clippy 全量**

```bash
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings \
  > .tmp/negwin/clippy.log 2>&1; tail -20 .tmp/negwin/clippy.log
```

后台，≥20 分钟。预期：`Finished`，无 `warning:`/`error:`。

- [ ] **Step 5: 全量测试**

```bash
cargo test --manifest-path rust/Cargo.toml --workspace --locked \
  > .tmp/negwin/full.log 2>&1; tail -30 .tmp/negwin/full.log
```

后台，≥20 分钟。预期：除两个**预存**失败（`cross_backend_contract` 的 3 个 restore 测试、`should_mount_the_watcher_within_a_fixed_time_budget`）外全绿。记录 `real` 并对比 Task 1。

- [ ] **Step 6: flaky 压测**

```bash
for i in $(seq 1 10); do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::should_stop_event_stream_when_disposed --exact >> .tmp/negwin/after-dispose.log 2>&1
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::e10_interrupted_recovery_tests::should_not_fold_interrupted_into_failed_on_live_row --exact >> .tmp/negwin/after-e10.log 2>&1
done
for i in $(seq 1 5); do
  cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::b2_notification_tests >> .tmp/negwin/after-b2.log 2>&1
done
grep -c "test result: ok" .tmp/negwin/after-b2.log
grep -c "FAILED\|panicked" .tmp/negwin/after-dispose.log .tmp/negwin/after-e10.log .tmp/negwin/after-b2.log
```

预期：b2 `5` 次 ok；三个日志的 FAILED/panicked 均为 `0`。

- [ ] **Step 7: 记录改造后耗时**

```bash
( time cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib -- \
    embedded_server::tests::b2_notification_tests > .tmp/negwin/after-b2-time.log 2>&1 ) 2>> .tmp/negwin/after-b2-time.log
( time cargo test --manifest-path rust/Cargo.toml -p mirri-embedded --lib \
    > .tmp/negwin/after-embedded-lib.log 2>&1 ) 2>> .tmp/negwin/after-embedded-lib.log
```

预期：b2 与 embedded-lib 的 `real` ≤ Task 1 基线。若 embedded-lib 退化 >10 s，先查因（某个 wait 未提前返回 / 轮询步进过粗）。

- [ ] **Step 8: ROADMAP 回写**

`docs/specs/rust-core-design/ROADMAP.md` §7 的 R4 条目下追加（中文，与文件一致）：

```markdown
- **R4 补：否定窗确定性化** ✅（2026-11-05，分支 `test/deterministic-negative-windows`，范围 B）：79 处 `hygiene-allow` 中 70 处删除窗口改为确定性驱动 —— `deliver_due_notifications` / `schedule_session_cron_pass` / `schedule_workspace_cron_pass` 放开为 `pub(super)`，`follow_pass` 从循环体抽出为自由函数，测试据此直接驱动，"约 N 个 tick"变成 N 次已完成的裁决；cron `held_fires()`、parked steer 键、`notification_delivered_at`、`McpStartupTiming` 结算、watcher 退出标志、armed stall window 把"等窗口"变成正向信号。两个 CI 实锤 flaky 修到根因（`dispose()` abort 订阅转发任务使通道真正关闭；interrupted live row 等 fold 落定再断言）。仅为手动压缩 apply 引入 1 个 `#[cfg(test)]` 计数器（drop guard 计数，覆盖取消早退分支）。保留 9 处（`swarm_batch` ×6 虚拟时钟步进、`conformance.rs` 的 pgrep 子进程抽样、`b1_print_mode_tests` 的 300ms 模拟工时、`tests.rs:44094` 的 plugin-install 缺席抽样），reason 逐条具体。判据与逐条判定见 `plans/2026-11-05-negative-window-audit.md`。R4 的"不批量 paused-clock 化"裁决不变。
```

- [ ] **Step 9: changeset 复核**

调 `gen-changesets` skill 走判定表：
- 可见性变化与 `#[cfg(test)]` seam → 用户不可感知 → 无条目。
- `dispose()` 行为改动 → 嵌入方 `recv()` 开始返回 `None` → 1 条 `patch`：

```markdown
---
'@mirri-ai/release': patch
---

Close subscriber event channels when the engine is disposed (`dispose()` now aborts the per-subscriber forwarders instead of leaving the channel open).
```

若用户拒绝了 `dispose()` 改动，则无需 changeset，在 PR 描述里说明。

- [ ] **Step 10: 最终校验 + 提交**

```bash
node rust/scripts/check-test-hygiene.mjs
git status --short
git add docs/specs/rust-core-design/ROADMAP.md docs/specs/rust-core-design/plans/
[ -f .changeset/*.md ] && git add .changeset
git commit -m "docs(specs): record the negative-window determinization in the R4 entry"
```

预期：门禁绿；`git status` 无 scratch 文件（`.tmp/` 已 gitignore）。

---

## Self-Review

本节记录**三份独立审查**（代码事实 / 可执行性 / 断言语义）的结论以及已修正项。计划正文已按下列修正更新；未修正项在这里明确登记为已知残留风险。

**1. 覆盖度（加法完整）。** 79 个站点按文件：`b2` 41（Task 3 转 #758，Task 4 转 40）、`tests.rs` 18（Task 5：17 转换 + #44094 保留）、`swarm_batch` 6（保留）、`cron_ws_4b` 4（Task 6）、`t_session_consistency` 3（Task 6）、`cron_ws_4a`/`b1_print_mode`/`skill_watcher`/`host`/`mock_llm_tests`/`conformance`/`orphan_governance` 各 1（Task 6）= 79 ✓。

注解数链：79 →（Task 3）78 →（Task 4）38 →（Task 5）21 →（Task 6）**9**。

**2. 审查已修正的缺陷（12 条，均为真错误）。**

| # | 缺陷 | 来源 | 修正 |
|---|---|---|---|
| 1 | 基线写成 `origin/main`——该树 `rust/` 里 0 处 `hygiene-allow`、也没有卫生扫描器 | 代码事实 | 改为从 `test/sleep-governance` 切；Task 1 Step 1 写明 `NOT-MERGED` 现状 |
| 2 | 验收写 `gate ok: 0 new violations`——该树实测 `new (would fail): 199`，起点就不可达 | 代码事实 | 改为相对判据（不高于基线 199）+ grep 数注解 |
| 3 | apply 计数器放在 `match` 之后——取消分支 `return` 早退（`compaction_handlers.rs:240`），4 个取消测试会 5s 超时 | 语义 + 事实 | 改为 `CountOnDrop` drop guard |
| 4 | `while follow_pass { interval.tick() }` 会让启动时连跑两次 pass（行为改动） | 代码事实 | 改为 `loop { interval.tick().await; if !follow_pass { break } }` |
| 5 | `McpStartupTiming::is_settled = outstanding == 0` 在初始态即为真 | 代码事实 | 改为 `started.is_some() && outstanding == 0` |
| 6 | `wait_for_turn_idle` 走 `poll_agent_activity`，硬编码 `agent_id: "main"`；5 个 subagent 站点会塌缩或超时 | 语义 | 新增 `poll_agent_activity_for` / `wait_for_agent_turn_idle`，5 个站点改用它 |
| 7 | #5935 的理由（"goal 离开 Active ⇒ chain 已返回"）是错的：`UpdateGoal` 在 turn 内就写 Complete | 语义 | 新增 `wait_for_goal_chain_stopped`，等 `chain_running == false` |
| 8 | #1440 等 `notification_delivered_at` 是空操作（marker 在 LLM 调用**之前**就写） | 语义 | 改等 turn 收尾 |
| 9 | #15997 断言 `manual_in_flight()` 是在断言被测机制本身 | 语义 | 改为"send 任务仍在跑 + 计数稳定" |
| 10 | #44094 等 MCP 结算永真（`plugin.install` 不碰 `mcp_startup`） | 语义 | 改为**保留**该窗口 + 精确 reason |
| 11 | `restore_task_notifications` 放宽后无人消费（dead_code）；`compaction_handle_for` 同理 | 可执行 | 两者都从计划删除 |
| 12 | `chat_calls` 只改了结构体、没改 factory 与测试绑定；`wait_for_watcher_exit` 放在不可达的模块；可见性探针的路径不存在 | 可执行 | 三处都给出完整代码 |

**3. 已知残留风险（登记而非掩盖）。**

- **直驱只跑投递裁决，不跑 tick 的其他阶段**（`terminate_expired` / stale-warn）。当前 40 个 b2 站点都不依赖它们（种子任务都已 terminal 或无 deadline），但这是"机会数"与生产 tick 的一处定义差异——Task 4 Step 5 已把它写成硬检查而非注释。
- **直驱与仍然存活的 spawned driver 并发**。`deliver_due_notifications` 的文档说它"serialized inside the driver loop"，直接调用会打破这个前提。对缺席断言无害，但若某个站点断言精确的投递次数，可能出现测试内双重裁决。PR 描述需写明。
- **#4983 / #5163** 的机会数从"约 8 / 约 5 个真实 tick"降为"2 次 pass"，且 session 已删 / provider 建不出来时 pass 在 `resolve_session_via` 早退——钉住的是守卫而非注入路径。已改为在注释里明说。
- **#245** 依赖 `hold_probe` 增加"被问过"标志；若探针语义变化，该测试的空断言风险会回来。
- **e10 的验证依赖 CI**（本机可能不复现），已在 Task 2 Step 7 写明证据链口径。
- **`docs/superpowers/` 不在 git 跟踪内**是仓库惯例；两份文档已放到 `docs/specs/rust-core-design/plans/`，属正常提交范围。

**4. 类型一致性。** helper 签名统一（见 §Task 3 Interfaces）；`follow_pass` 是**自由函数** `(registry, in_flight_turns, session_id) -> bool`，不是方法；`drive_cron_pass` 只用于 workspace 级 cron（Task 6 #626），`drive_session_cron_pass` 用于 session 级（Task 4 #4983/#5163）。
