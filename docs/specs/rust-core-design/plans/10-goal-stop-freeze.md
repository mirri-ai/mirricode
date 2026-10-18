# Goal 模式"真正停止"（goal-stop-freeze）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Goal 模式下的"停止 Turn"与"Pause"真正停止引擎活动——注入 turn 可被 abort、Pause 冻结整个 goal 机器（取消在跑 turn + 挂起通知投递 + 禁止模型覆盖 paused）。

**Architecture:** 三个缺口的修复：(G1) task/cron driver 注入的 turn 以 Wait 语义注册进 `in_flight_turns` 表使 `message.abort` 可取消；(G2) `goal.pause`/`goal.cancel` 复用从 `dispatch_cancel` 提取的取消内核，且通知投递在 goal paused 时挂起；(G3) `UpdateGoal` 工具加状态机门禁（TS 引擎 parity）。SDD brief：`.superpowers/sdd/goal-stop-freeze-brief.md`。

**Tech Stack:** Rust（rust workspace：mirri-core、mirri-embedded、mirri-server crates），tokio，既有测试 harness（`test_server`/`b2_server`/`next_event_where`/`ScriptedLlm`/`BlockingLlm`）。

## Global Constraints

- 测试命名 Given-When-Then（`should_X_when_Y`），测试从需求/设计出发，不是给实现自证。
- `cargo test -p mirri-core -p mirri-embedded -p mirri-server` 全绿；`cargo clippy -p mirri-core -p mirri-embedded -p mirri-server --all-targets` 无新告警；`node rust/scripts/check-zero-io.mjs` 0 命中（本批不引入任何 IO）。
- 测试执行循序渐进：先跑新增的单测，再跑所在文件，最后跑三个 crate 全量。
- 本地 git commit（**不 push、不建 PR**）；commit message 遵循 conventional 风格、无任何 agent 署名/co-author。
- 仓库工作区有其他人的未提交改动（mirri-cli 等）——**绝不能动、不能 stage**。每个 Task 的 commit 用 pathspec 方式只提交本任务文件。
- wire log append-only：新增 wire op 语义只走既有 `WireOp::GoalUpdate`/`TurnEnded` 形状，不发明新 op。
- 事件投影词汇唯一源在 `mirri-core/src/session/event_projection.rs`：本批不新增事件类型，只改既有 payload 语义。

---

### Task 1: UpdateGoal 状态机门禁（G3）

**Files:**
- Modify: `rust/crates/mirri-core/src/tools/update_goal.rs`（schema/门禁/文案/注释 + 内联 tests）

**Interfaces:**
- Consumes: `GoalManager`（`mirri-core/src/goal.rs`，保持不动——门禁放工具层，TS parity）。
- Produces: `UpdateGoalTool` 行为变更——接受状态 `active|complete|blocked`（`paused` 从模型输入中移除）；`complete`/`blocked` 仅当当前状态为 `Active` 时允许；`active` 从 `Paused`/`Blocked` 允许（恢复路径）。

**背景（为什么这样改）**：TS 引擎的工具枚举没有 `paused`（模型不能暂停，只有用户能），且 `markComplete`/`markBlocked` 在非 active 时 no-op（`packages/agent-core/src/agent/goal/index.ts:512-539`）。rust 侧无任何守卫，导致实测中模型把用户 paused 覆盖成 completed。注入提示词（`turn_flow/injection/goal.rs` 的 paused note）已承诺"用户要求继续时调 UpdateGoal(active)"，所以 paused→active 必须保留。

- [ ] **Step 1: 写失败的测试**

在 `update_goal.rs` 的 `#[cfg(test)] mod tests` 中：

改写既有 `should_update_to_each_valid_status_when_given`（第 216 行附近）——去掉 `paused`：

```rust
    #[tokio::test]
    async fn should_update_to_each_valid_status_when_given() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Multi-state".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        for status in ["active", "blocked", "complete"] {
            let result = execute_tool(&tool, serde_json::json!({ "status": status })).await;
            assert!(
                !result.is_error,
                "status '{status}' failed: {}",
                output_text(&result)
            );
            let expected = match status {
                "active" => GoalStatus::Active,
                "blocked" => GoalStatus::Blocked,
                "complete" => GoalStatus::Complete,
                _ => unreachable!(),
            };
            assert_eq!(
                manager.get().unwrap().status,
                expected,
                "after setting {status}"
            );
        }
    }
```

既有 `should_return_an_error_when_no_goal_exists`（第 243 行）把输入 `"paused"` 改为 `"complete"`。

新增三个测试：

```rust
    #[tokio::test]
    async fn should_reject_complete_when_the_goal_is_paused_by_the_user() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "complete" })).await;

        assert!(result.is_error, "{}", output_text(&result));
        assert!(
            output_text(&result).contains("no active goal"),
            "{}",
            output_text(&result)
        );
        // The user's pause is never overwritten by the model.
        assert_eq!(manager.get().unwrap().status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn should_reject_blocked_when_the_goal_is_paused_by_the_user() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "blocked" })).await;

        assert!(result.is_error, "{}", output_text(&result));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn should_allow_resume_from_paused_when_the_model_is_asked_to_continue() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "active" })).await;

        assert!(!result.is_error, "{}", output_text(&result));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn should_reject_paused_as_a_model_supplied_status() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let execution = tool
            .resolve_execution(serde_json::json!({ "status": "paused" }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("paused"));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Active);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-core --lib tools::update_goal
```
预期：`should_reject_complete_when_the_goal_is_paused_by_the_user`、`should_reject_blocked_when_the_goal_is_paused_by_the_user` FAIL（当前无守卫，成功改写）；`should_update_to_each_valid_status_when_given` FAIL（`paused` 仍被接受）；`should_reject_paused_as_a_model_supplied_status` FAIL（phase-1 接受 `paused`）。

- [ ] **Step 3: 最小实现**

`update_goal.rs`：

1. `resolve_execution` 的状态解析（第 96-108 行）删掉 `"paused"` 分支：

```rust
        let status = match status_str {
            "active" => GoalStatus::Active,
            "complete" => GoalStatus::Complete,
            "blocked" => GoalStatus::Blocked,
            other => {
                return Ok(ToolExecution::Error(ToolErrorResult {
                    message: format!(
                        "UpdateGoal 'status' must be one of active, complete, blocked; got '{other}'"
                    ),
                }));
            }
        };
```

2. `parameters()` 的 schema enum（第 80 行）与 `description()`（第 67 行）同步去掉 `paused`，描述改为：`"Update the status of the current goal. Valid statuses: 'active', 'complete', 'blocked'. Use 'complete' when the objective has been achieved."`

3. execute 闭包内（第 132 行 `goal.get().is_none()` 检查之后、wire 记录之前）加门禁：

```rust
                    // TS parity (goal service guards): terminal
                    // transitions require an ACTIVE goal — a user pause
                    // (or any other non-active state) is never
                    // overwritten by the model. Resuming
                    // (paused/blocked → active) stays allowed: the
                    // paused-goal note tells the model to call UpdateGoal
                    // with 'active' when the user asks to continue.
                    if matches!(status, GoalStatus::Complete | GoalStatus::Blocked)
                        && goal
                            .get()
                            .is_some_and(|goal| goal.status != GoalStatus::Active)
                    {
                        return ToolResult::error(match status {
                            GoalStatus::Complete => "Goal not completed: no active goal.",
                            GoalStatus::Blocked => "Goal not blocked: no active goal.",
                            _ => unreachable!(),
                        });
                    }
```

4. 模块头注释（第 1-7 行）同步：Valid statuses are `active`, `complete`, `blocked`；注明模型不可暂停（只有用户能 pause）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-core --lib tools::update_goal
```
预期：全部 PASS。再跑 `cargo test -p mirri-core` 全量确认无破坏（UpdateGoal 相关的其他测试——搜索 `update_goal`/`UpdateGoal` 引用——若有引用 paused 输入的同步修正）。

- [ ] **Step 5: Commit**

```bash
git commit -m "fix(core): guard UpdateGoal terminal transitions against non-active goals" -- rust/crates/mirri-core/src/tools/update_goal.rs
```

---

### Task 2: task driver 通知 turn 注册为可取消（G1a）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（`InFlightTurnsMap` 别名 + `WaitTurnGuard` 上移至 mod.rs `pub(super)`）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（改用上移的 guard；`in_flight_turns` 字段类型换别名）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/task_driver.rs`（注册/传递）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（新增 `CancelAwareBlockingLlm` mock）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs`

**Interfaces:**
- Produces（Task 3/4 依赖）: `pub(super) type InFlightTurnsMap = Arc<std::sync::Mutex<HashMap<String, InFlightTurn>>>;`（mod.rs，`InFlightTurn` 定义旁）；`pub(super) struct WaitTurnGuard { pub(super) turns: InFlightTurnsMap, pub(super) key: String }`（mod.rs，实现 Drop 移除条目——从 message_handlers.rs:229-241 原样上移）。

**背景**：通知 turn 在 driver tick future 上内联运行且自闭合 bracket（Ok/Err 两臂都写 `TurnEnded` 并发布 `TurnCompleted`/`TurnFailed`），与 wait-mode 契约一致；注册为 `InFlightTurn::Wait` 后 `dispatch_cancel` 只 cancel token，turn 自行结算，无双结算风险。注册后注入 turn 计入 busy 派生（`session.list` busy、compaction 预检）——这是修复应有之义。

- [ ] **Step 1: 写失败的测试**

先在 `tests.rs` 的 `BlockingLlm`（约第 5169 行）旁新增 cancel-aware 阻塞 mock（模拟真实 provider SSE 泵在事件间检查 cancel 的形态，见 `providers/convert.rs` 的 `pump_sse_to_llm_events`）：

```rust
/// An [`Llm`] whose stream yields nothing until the turn's cancel token
/// fires, then ends — the shape of a hung real provider call that still
/// honours cooperative cancellation (the SSE pumps check the token
/// between events, `providers/convert.rs`). Unlike [`BlockingLlm`] this
/// lets a WAIT-registered turn settle promptly on abort.
#[derive(Debug)]
struct CancelAwareBlockingLlm;

impl Llm for CancelAwareBlockingLlm {
    fn system_prompt(&self) -> &str {
        "You are a blocked test assistant."
    }

    fn model_name(&self) -> &str {
        "cancel-aware-blocking-llm"
    }

    fn capability(&self) -> &ModelCapability {
        static CAPABILITY: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAPABILITY
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let cancel = params.cancel.clone();
        Ok(Box::pin(
            futures::stream::once(async move { cancel.cancelled().await })
                .filter_map(|(): ()| async move { None }),
        ))
    }
}

/// An [`LlmFactory`] serving one shared [`CancelAwareBlockingLlm`].
#[derive(Debug, Clone)]
struct CancelAwareBlockingLlmFactory;

impl LlmFactory for CancelAwareBlockingLlmFactory {
    fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        Ok(Arc::new(CancelAwareBlockingLlm) as Arc<dyn Llm>)
    }
}
```

（`futures::StreamExt` 的 `filter_map` 需要 `use futures::StreamExt;`——tests.rs 若未有则加。`LlmChatParams.cancel` 字段存在，见 `loop_engine/turn_step.rs` 的 `build_chat_params`。）

再在 `b2_notification_tests.rs` 中新增（复用 `b2_server` 需先泛化：把 `b2_server` 改为 `b2_server_with_factory(factory: Arc<dyn LlmFactory>, tick: Duration)` + 保持 `b2_server(mock, tick)` 委托）：

```rust
/// Given a completed detached task whose notification turn is stuck in a
/// cancel-aware blocked LLM call, when `message.abort` targets the
/// session, then the notification turn is actually cancelled — it
/// settles with the aborted stop reason instead of running to
/// completion, and the session stops reporting busy.
#[tokio::test]
async fn should_cancel_the_notification_turn_when_message_abort_targets_the_session() {
    let (server, _tmp) = b2_server_with_factory(
        Arc::new(CancelAwareBlockingLlmFactory),
        Duration::from_millis(50),
    );
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let _task_id = completed_background_task(&session, &session_id);

    // The notification turn starts and blocks inside the LLM call.
    let turn_started = next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {})
        .await;
    assert!(
        turn_started.params["turnId"]
            .as_str()
            .is_some_and(|id| id.starts_with("turn-")),
        "the notification turn carries the turn-<uuid> bracket id"
    );

    // When the user aborts the session's in-flight turn.
    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .expect("abort");
    assert_eq!(aborted["ok"], true);

    // Then the notification turn settles as cancelled — not running to
    // completion — and the turn.ended bracket names the aborted reason.
    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(
        ended.params["turnId"],
        turn_started.params["turnId"],
        "the cancelled turn is the notification turn"
    );

    // And the session stops reporting busy once the turn settles.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let listed = server.call_method("session.list", json!({})).await.unwrap();
        let view = listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == json!(session_id))
            .expect("the session is listed");
        if view["busy"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session must stop reporting busy after the abort"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
```

（`TurnStopReason::Aborted` 的 wire 拼写是 `"cancelled"`，见 `event_projection.rs:903`。）

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_notification_turn_when_message_abort_targets_the_session
```
预期：FAIL——abort 返回 `ok: true` 但 turn 永不结算（`next_event_where` 超时 panic：`event.turn.ended` 到达时 reason 是 `"completed"`，turn 跑满/或测试挂起直到 5s 超时）。注意 BlockingLlm 不可用于此处（非 cancel-aware 会等 300s ENGINE_TURN_TIMEOUT）。

- [ ] **Step 3: 最小实现**

1. `mod.rs`（`InFlightTurn` 定义旁，约 694-726 行区域后）：

```rust
/// The engine-wide in-flight turn registry's map type — shared by the
/// dispatch handlers and the task/cron drivers so every engine turn
/// (user-dispatched or driver-injected) is cancellable via
/// `message.abort`.
pub(super) type InFlightTurnsMap =
    Arc<std::sync::Mutex<HashMap<String, InFlightTurn>>>;

/// Removes a wait-registered turn's `in_flight_turns` entry when the
/// turn's inline future settles (success, error, or panic) — the entry
/// must not outlive the turn, or a later `message.abort` would cancel a
/// dead token.
pub(super) struct WaitTurnGuard {
    pub(super) turns: InFlightTurnsMap,
    pub(super) key: String,
}

impl Drop for WaitTurnGuard {
    fn drop(&mut self) {
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}
```

`EmbeddedServer::in_flight_turns` 字段类型（mod.rs:787-792）改为 `InFlightTurnsMap`（初始化处同步）。删除 `message_handlers.rs:225-241` 的私有 `WaitTurnGuard`，其使用点（约 651 行）改 `use super::WaitTurnGuard;`（加入文件顶部既有 `use super::{...}` 列表）。`WaitTurnGuard` 构造处字段初始化语法不变。

2. `task_driver.rs`：

   - `spawn_task_driver`（75 行起）capture 区加 `let in_flight_turns: InFlightTurnsMap = Arc::clone(&self.in_flight_turns);`，随 tick 传给 `deliver_due_notifications`。
   - `deliver_due_notifications`（154 行）与 `run_notification_turn`（508 行）签名各加 `in_flight_turns: &InFlightTurnsMap` 参数（调用点同步）。
   - `run_notification_turn` 中 `run_engine_turn` 调用前（turn_id 已在 550 行 mint、`TurnStarted` 已发布）：

```rust
    // Register the turn for cancellation (the `message.abort` path): the
    // notification turn runs inline on the driver's tick future and
    // closes its own bracket in both arms below — exactly the WAIT
    // contract, so only the cooperative cancel token is registered.
    let cancel_token = CancelToken::new();
    let in_flight_key = format!("{session_id}:{addressee_id}:{turn_id}");
    in_flight_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            in_flight_key.clone(),
            InFlightTurn::Wait {
                cancel: cancel_token.clone(),
            },
        );
    let _turn_registration = WaitTurnGuard {
        turns: Arc::clone(in_flight_turns),
        key: in_flight_key,
    };
```

   `run_engine_turn(..., CancelToken::new(), ...)`（650 行）改为 `cancel_token.clone()`；需要 `use super::{InFlightTurn, WaitTurnGuard};` 与 `use crate::cancel::CancelToken;`（按文件既有 import 风格调整）。

   - Ok 臂（655 行 `match turn_end`）开头加与 message_handlers wait 臂一致的改写：

```rust
        Ok(turn_end) => {
            // A cancelled notification turn reports `Aborted` even when
            // the provider stream ended cleanly after observing the
            // token — the awaiting client cancelled and must hear the
            // turn ended because of that (mirrors the message.send wait
            // arm).
            let turn_end =
                if cancel_token.is_cancelled() && turn_end.stop_reason != TurnStopReason::Aborted {
                    TurnEndResult {
                        stop_reason: TurnStopReason::Aborted,
                        ..turn_end
                    }
                } else {
                    turn_end
                };
```

（`TurnStopReason`/`TurnEndResult` 若未导入则加。）

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_notification_turn_when_message_abort_targets_the_session
cargo test -p mirri-embedded --lib b2_notification_tests
cargo test -p mirri-embedded --lib embedded_server::tests::should_cancel
```
最后跑 `-p mirri-embedded` 全量（busy 派生变化可能影响既有断言——若有测试因 driver turn 计入 busy 而红，读该测试判断：语义正确则修测试并注明，实现真有 bug 则修实现）。

- [ ] **Step 5: Commit**

```bash
git commit -m "fix(embedded): register notification turns as abortable in-flight turns" -- rust/crates/mirri-embedded/src/embedded_server/mod.rs rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs rust/crates/mirri-embedded/src/embedded_server/task_driver.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs
```

---

### Task 3: cron driver 注入 turn 注册为可取消（G1b）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs`
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（cron 注入 abort 测试）

**Interfaces:**
- Consumes: Task 2 的 `InFlightTurnsMap` / `WaitTurnGuard` / `CancelAwareBlockingLlm`。

- [ ] **Step 1: 写失败的测试**

在 `tests.rs` 的 `should_drive_due_cron_tasks_through_the_cron_driver_when_the_tick_fires`（23378 行）旁新增（其 setup 用 `session.cron().restore_tasks([...])` 直接种 cron，不需要 LLM 预热）：

```rust
/// Given an overdue cron task whose injected turn is stuck in a
/// cancel-aware blocked LLM call, when `message.abort` targets the
/// session, then the cron-injected turn is cancelled — it settles with
/// the aborted reason instead of running to completion.
#[tokio::test]
async fn should_cancel_the_cron_injected_turn_when_message_abort_targets_the_session() {
    let (server, _tmp) = test_server(Some(Arc::new(CancelAwareBlockingLlmFactory)));
    let server = server.with_cron_tick(Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let session = server
        .manager
        .get_session(&mirri_core::SessionId::new(&session_id))
        .await
        .unwrap();
    session.cron().restore_tasks([mirri_core::cron::CronTask {
        id: "cron-abort".to_string(),
        cron: "* * * * *".to_string(),
        prompt: "do the cron thing".to_string(),
        session_id: session_id.clone(),
        recurring: false,
        timezone: None,
        created_at: chrono::Utc::now() - chrono::Duration::minutes(10),
        last_fired: None,
        next_fire_at: None,
    }]);
    session
        .cron()
        .set_next_fire_at("cron-abort", Some(chrono::Utc::now() - chrono::Duration::minutes(1)));

    let mut events = server.subscribe_events().await.unwrap();
    let _ = next_event_where(&mut events, |n| n.method == "event.cron.fired", |_| {}).await;
    let turn_started = next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {})
        .await;

    let aborted = server
        .call_method("message.abort", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(aborted["ok"], true);

    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["reason"], "cancelled");
    assert_eq!(ended.params["turnId"], turn_started.params["turnId"]);
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_cron_injected_turn_when_message_abort_targets_the_session
```
预期：FAIL（turn 跑完 reason `completed` 或超时）。

- [ ] **Step 3: 最小实现**

`cron_driver.rs` 与 Task 2 同构：

1. `spawn_cron_driver`（155 行）capture `in_flight_turns`，tick 调用传入。
2. `inject_cron_prompt`（302 行）签名加 `in_flight_turns: &InFlightTurnsMap`。**两个调用方**都要传：driver tick 循环（243-255 行）与 `dispatch_trigger_cron_task` 的 spawned task（132-144 行；其 capture 区在 `&self` 上，直接 `Arc::clone(&self.in_flight_turns)`）。
3. `inject_cron_prompt` 内 turn_id mint（381 行）后、`run_engine_turn`（451 行）前：与 Task 2 相同的注册 + `WaitTurnGuard`（key 用 `target_agent_id`）；`CancelToken::new()`（472 行）→ `cancel_token.clone()`；Ok 臂加同款 Aborted 改写。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_cron_injected_turn_when_message_abort_targets_the_session
cargo test -p mirri-embedded --lib cron
```

- [ ] **Step 5: Commit**

```bash
git commit -m "fix(embedded): register cron-injected turns as abortable in-flight turns" -- rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs
```

---

### Task 4: goal.pause / goal.cancel 冻结语义（G2）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（从 `dispatch_cancel` 提取取消内核）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（`dispatch_pause_goal`/`dispatch_cancel_goal` 调用内核）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/task_driver.rs`（`deliver_due_notifications` 加 paused gate）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`

**Interfaces:**
- Produces: `pub(super) async fn cancel_in_flight_turns(&self, session_id: &str, turn_id: Option<&str>, cause: &str) -> Result<(), EngineError>`（message_handlers.rs；`cause` 拼进 `TurnFailed` 的 error 文案 `format!("turn aborted by {cause}")`）。`message.abort` 传 `Some("the user via message.abort")`（保持既有文案逐字节不变），pause/cancel 传 `"the user via goal.pause"` / `"the user via goal.cancel"`。

**背景**：`dispatch_pause_goal` 目前只写 wire op + 内存状态 + 发事件（session_handlers.rs:3408-3452）。取消内核提取自 `dispatch_cancel`（message_handlers.rs:988-1121）的 collect + per-entry cancel + fire 结算逻辑；Wait 条目只 cancel token 自行结算，fire 条目 hard-abort + aborting 侧发布 `TurnFailed`。通知 gate：`deliver_due_notifications` 入口检查 session goal——存在且 `Paused` 直接 return（due 通知保持 pending、stranded buffer 不 flush、不 park），resume 后下一 tick 恢复。

- [ ] **Step 1: 写失败的测试**

在 `tests.rs` 新增三个（均用 `CancelAwareBlockingLlmFactory`）：

```rust
/// Given a goal-mode session with an in-flight fire turn, when
/// `goal.pause` is called, then the goal flips to paused AND the
/// in-flight turn is cancelled (settles with the aborted reason) —
/// pausing must stop the machine, not just flip a state bit.
#[tokio::test]
async fn should_cancel_the_in_flight_turn_when_goal_pause_is_called() {
    let (server, _tmp) = test_server(Some(Arc::new(CancelAwareBlockingLlmFactory)));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();
    let mut events = server.subscribe_events().await.unwrap();

    let fired = server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "work on the goal", "mode": "fire" }),
        )
        .await
        .unwrap();
    let turn_id = fired["turn_id"].as_str().unwrap().to_string();
    let _ = next_event_where(&mut events, |n| n.method == "event.turn.started", |_| {}).await;

    // A goal exists (paused through the manager so the pause handler
    // sees one).
    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .unwrap();
    session.goal().create("Do the thing".to_string(), None).unwrap();

    let paused = server
        .call_method("goal.pause", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(paused["status"], "paused");

    let ended = next_event_where(&mut events, |n| n.method == "event.turn.ended", |_| {}).await;
    assert_eq!(ended.params["turnId"], json!(turn_id));
    // WAIT-fire contract: the cancelled fire turn settles as failed via
    // the aborting side's TurnFailed (reason "failed",
    // abort.user_cancelled).
    assert_eq!(ended.params["reason"], "failed");
    assert_eq!(ended.params["error"]["code"], "abort.user_cancelled");
}
```

（fire turn 被 pause 取消时走 fire-abort 结算：reason `"failed"` + `abort.user_cancelled`，与 message.abort 既有契约一致。）

```rust
/// Given a paused goal and a completed detached task whose notification
/// would be due, when ticks pass, then no notification turn runs — the
/// delivery is held while the goal is paused; after `goal.resume`, the
/// notification is delivered on the next tick.
#[tokio::test]
async fn should_hold_the_notification_delivery_while_the_goal_is_paused() {
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_text("notification handled");
    let (server, _tmp) = b2_server(Arc::clone(&mock), Duration::from_millis(50));
    let created = server
        .call_method("session.create", json!({}))
        .await
        .expect("create");
    let session_id = created["id"].as_str().unwrap().to_string();

    let session = server
        .session_manager()
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    session.goal().create("Do the thing".to_string(), None).unwrap();
    session.goal().update(mirri_core::GoalStatus::Paused);
    let _task_id = completed_background_task(&session, &session_id);

    // Several ticks pass while paused — no notification turn runs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !notification_reached_model(&mock),
        "no notification turn may run while the goal is paused"
    );

    // When the goal resumes, the held notification is delivered.
    let resumed = server
        .call_method("goal.resume", json!({ "session_id": &session_id }))
        .await
        .unwrap();
    assert_eq!(resumed["status"], "active");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if notification_reached_model(&mock) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held notification must be delivered after the goal resumes"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
```

（注意：此测试直接 manager `update(Paused)` 种状态后调 `goal.resume` RPC 验证 resume 侧；若想同时锁定 pause RPC 的挂起路径，可改用 `goal.pause` RPC 替代 `session.goal().update(...)`——两取其一即可，推荐 RPC 路径以覆盖 handler。）

```rust
/// Given a paused goal, when the model-side UpdateGoal is executed
/// through the tool, then the pause is not overwritten (Task 1 的引擎级
/// 复验：走 run_main_agent_tool 需要完整 turn，此处用 goal.get 断言
/// pause RPC 后状态)。
```
（此测试由 Task 1 的单测覆盖状态机本身；引擎级无需重复——在 plan 中标记为"由 Task 1 覆盖"，不新增。）

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_in_flight_turn_when_goal_pause_is_called
cargo test -p mirri-embedded --lib should_hold_the_notification_delivery_while_the_goal_is_paused
```
预期：前者 FAIL（`event.turn.ended` reason 是 `"completed"`——turn 不被取消，跑满直到 mock 的 cancel 语义把流结束 → 收到 completed）；后者 FAIL（paused 期间通知照常投递，`notification_reached_model` 立即 true）。

- [ ] **Step 3: 最小实现**

1. `message_handlers.rs`：把 `dispatch_cancel` 的 collect + per-entry 循环体（1003-1119 行区域）提取为：

```rust
    /// Cancel in-flight turns of the session (all, or one `turn_id`) —
    /// the shared kernel of `message.abort` and the goal lifecycle
    /// handlers (`goal.pause` / `goal.cancel` freeze the machine by
    /// cancelling whatever is running). Wait entries only observe the
    /// cooperative token and settle themselves; fire entries are
    /// hard-aborted and settled here (the dead task cannot publish its
    /// own bracket close).
    pub(super) async fn cancel_in_flight_turns(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        cause: &str,
    ) -> Result<(), EngineError> {
        // …dispatch_cancel 1003-1037 行的 collect + empty 处理原样搬入
        // （empty + turn_id → not_found；empty + None → idempotent Ok）…
        // …1043-1119 行的 per-entry 循环原样搬入，唯一改动：
        //   error: format!("turn aborted by {cause}")（替换硬编码
        //   "turn aborted by the user via message.abort"）…
        Ok(())
    }
```

`dispatch_cancel` 变为参数解析 + `self.cancel_in_flight_turns(&params.session_id, params.turn_id.as_deref(), "the user via message.abort").await`。既有 abort 测试的文案断言（contains "aborted"）不受影响。

2. `session_handlers.rs` `dispatch_pause_goal`（3433 行 `goal.update(...)` 之后、事件发布之后）：

```rust
        // SDR (goal-stop-freeze): pausing freezes the whole machine —
        // the in-flight turns are cancelled with the same semantics as
        // `message.abort` (the web's turn.ended events clear the working
        // UI), and the notification driver holds deliveries while the
        // goal is paused.
        if goal.get().is_some() {
            self.cancel_in_flight_turns(&params.session_id, None, "the user via goal.pause")
                .await?;
        }
```

`dispatch_cancel_goal`（3502 行起）在 `goal.clear()` 前加同款调用（cause `"the user via goal.cancel"`，不需要 goal 存在判断——cancel 本身意为清除）。`dispatch_resume_goal` 不取消任何东西（只解冻）。

3. `task_driver.rs` `deliver_due_notifications` 函数体开头（session 解析之后）：

```rust
    // SDR (goal-stop-freeze): a user-paused goal freezes the
    // notification loop — due (and stranded) notifications stay pending
    // until the goal resumes. Pausing must stop the machine, not just
    // flip a state bit the injected turns ignore.
    if session
        .goal()
        .get()
        .is_some_and(|goal| goal.status == mirri_core::GoalStatus::Paused)
    {
        return;
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-embedded --lib should_cancel_the_in_flight_turn_when_goal_pause_is_called
cargo test -p mirri-embedded --lib should_hold_the_notification_delivery_while_the_goal_is_paused
cargo test -p mirri-embedded --lib goal
cargo test -p mirri-embedded
```
（若既有 goal 测试因新取消语义变红——如测试里 pause 后预期 turn 继续跑——逐个读：新语义正确则修测试并注明 SDR 出处。）

- [ ] **Step 5: Commit**

```bash
git commit -m "fix(embedded): make goal pause and cancel freeze the machine" -- rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs rust/crates/mirri-embedded/src/embedded_server/task_driver.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs
```

---

### Task 5: SDR/ADR 规格、ROADMAP、changeset、全量门禁

**Files:**
- Modify: `docs/specs/goal-requirements.md`（SDR：Pause 语义裁定）
- Modify: `docs/specs/goal-architecture.md`（ADR：注入 turn 注册 / 取消内核 / 通知 gate / UpdateGoal 门禁）
- Modify: `docs/specs/goal-context.md`（现状：三缺口 → 已修）
- Modify: `docs/specs/goal-ROADMAP.md`（开放项 + 变更历史）
- Create: `.changeset/<kebab>.md`

- [ ] **Step 1: SDR 写入 goal-requirements.md**（E2 生命周期节内，按既有 SDR 格式）：Pause = 冻结整个 goal 机器（取消在跑 turn、挂起通知投递、模型不可覆盖 paused、resume 恢复投递）；cron 注入不 gate 的边界；goal.cancel 同样取消在跑 turn。
- [ ] **Step 2: ADR 写入 goal-architecture.md**：Wait 语义注册、取消内核提取、deliver gate 位置、UpdateGoal 工具层门禁（TS parity 引用 `agent/goal/index.ts` 守卫）、busy 派生含义变化。
- [ ] **Step 3: context/ROADMAP 更新**：context 现状节记录三缺口已修（引用 commit）；ROADMAP 变更历史加一行 2026-09-14 goal-stop-freeze 批次。
- [ ] **Step 4: 全量门禁**

```bash
cd rust && cargo test -p mirri-core -p mirri-embedded -p mirri-server
cargo clippy -p mirri-core -p mirri-embedded -p mirri-server --all-targets
node scripts/check-zero-io.mjs
```

- [ ] **Step 5: changeset**（按 `.agents/skills/gen-changesets/SKILL.md` 规则，英文、rust-engine 风格）：

```markdown
---
"@mirri-ai/mirri-code": patch
---

web: Make Stop and Pause actually stop the goal machinery in the rust engine; driver-injected turns are now cancellable, pausing cancels in-flight turns and holds task notifications until resumed, and the model can no longer overwrite a user pause as completed.
```

- [ ] **Step 6: Commit**

```bash
git commit -m "docs(specs): record goal-stop-freeze decisions and rust parity notes" -- docs/specs/goal-requirements.md docs/specs/goal-architecture.md docs/specs/goal-context.md docs/specs/goal-ROADMAP.md .changeset/
```
（changeset 文件名以实际为准替换 pathspec。）

---

### Task 6: turn 取消时 goal 环停（G4，TS pauseOnInterrupt parity）

> 实测发现（2026-09-14 web 验收，session-7c871cca）：用户 Interrupt 取消通知注入 turn 后（`turn.ended reason=cancelled` 落盘确认 turn 真停），**63ms 后任务驱动即投递下一条搁浅通知为新 turn，goal 保持 Active，循环自主续跑到 turnsUsed=58**——"停止 Turn 后还在跑"的另一半根因。TS 基线在 `packages/agent-core/src/agent/turn/index.ts:446-448` 有对等规则：`if (end.event.reason === 'cancelled') await this.agent.goal.pauseOnInterrupt({ reason: 'Paused after interruption' })`；本域状态机（goal-architecture §2.1）也早已声明 `active --> paused : 回合中断`。Rust 引擎缺这条转移。

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（`cancel_in_flight_turns` 内核加 pause-on-interrupt）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs`（通知 turn 中止 → goal paused）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（fire turn 中止 → goal paused）

**Interfaces:**
- Consumes: `GoalManager::update_with_reason(status, reason)`（mirri-core/src/goal.rs:247）；`WireOp::GoalUpdate { reason, actor }`（mirri-proto/src/wire.rs:267）；`goal_snapshot_json`（session_handlers 同源 import）。

**设计（为什么在内核做）**：fire 型 turn 被 hard-abort 后**根本不会跑自己的结算路径**（内核代为发布 `TurnFailed`），在四个结算点各自接入会漏 fire 路径；内核单点接入天然覆盖 wait/fire/通知/cron 全部取消路径。`goal.pause` 先置 Paused 再调内核（no-op）、`goal.cancel` 先 clear 再调内核（no-op），只有 `message.abort` 会带着 Active goal 进内核——无需 cause 分流。事件顺序与 goal.pause 一致：goal.updated 先于 turn 终态事件（ADR-003 既有 ordering）。

- [ ] **Step 1: 写失败的测试**

`b2_notification_tests.rs`（既有 `should_cancel_the_notification_turn_when_goal_pause_is_called` 旁）：

```rust
/// Given an active goal and a completed task whose notification turn is
/// stuck in a cancel-aware blocked LLM call, when `message.abort`
/// cancels that turn, then the goal parks itself as paused (resumable) —
/// an interrupted turn must stop the goal loop, not hand it to the next
/// stranded notification — and a second completed task's notification
/// is held while the goal stays paused.
#[tokio::test]
async fn should_pause_the_goal_when_message_abort_cancels_the_notification_turn() {
    // CancelAwareBlockingLlmFactory + b2_server（照抄相邻测试的搭建）
    // 1. goal.create（active）
    // 2. completed_background_task(...) ×2
    // 3. 等 event.turn.started（第一条通知 turn 在 LLM 阻塞）
    // 4. message.abort（session 级）
    // 5. 断言 event.turn.ended reason == "cancelled"
    // 6. 断言 event.goal.updated 到达且 status == "paused"（在 turn.ended 之后到达也接受——但 wire 顺序 goal.update 先于 turn.ended；事件断言用 next_event_where 轮询 goal.updated）
    // 7. 断言 goal.get（RPC 或 session.goal().get()）status == paused、terminal_reason == "Paused after interruption"
    // 8. sleep 数个 tick 后断言无第二条 event.turn.started（挂起中的第二条通知被 gate）
    // 9. goal.resume 后断言第二条通知恢复投递（event.turn.started 再次到达）
}
```

`tests.rs`（既有 `should_cancel_the_in_flight_turn_when_goal_pause_is_called` 旁）：

```rust
/// Given an active goal and an in-flight fire turn, when `message.abort`
/// cancels it, then the goal parks as paused with the interruption
/// reason — the user's stop must freeze the loop, not just the turn.
#[tokio::test]
async fn should_pause_the_goal_when_message_abort_cancels_the_in_flight_turn() {
    // 照抄相邻测试搭建（CancelAwareBlockingLlmFactory + fire turn + goal.create）
    // 1. message.abort（session 级，无 turn_id）
    // 2. 断言 turn.ended（fire 走 TurnFailed：reason "failed" + abort.user_cancelled）
    // 3. 断言 goal status == paused、terminal_reason == "Paused after interruption"
}
```

无 goal / 已 paused 的 no-op 由既有测试间接覆盖（Task 2/3 的 abort 测试无 goal，事件序列不变即回归证明）；若实现引入了多余事件会立刻红。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd rust && cargo test -p mirri-embedded --lib should_pause_the_goal_when_message_abort
```
预期：两个新测试 FAIL（goal 状态仍是 active，无 paused 转移）。

- [ ] **Step 3: 最小实现**

`message_handlers.rs` `cancel_in_flight_turns`（992 行）：`aborted_session` 解析（1035 行）之后、`for (key, inflight) in targeted` 循环之前：

```rust
        // TS parity (`pauseOnInterrupt`, agent/turn/index.ts:446): any
        // turn-level cancellation parks an active goal as `paused`
        // (resumable) — an interrupted turn must stop the goal loop, or
        // the next stranded notification/continuation restarts it within
        // one driver tick (observed live: 63ms). No-op when the goal is
        // missing or already non-active, so the `goal.pause` (already
        // paused) and `goal.cancel` (cleared) callers never re-transition.
        // Ordered BEFORE the cancel tokens fire so the wire reports the
        // pause ahead of the freeze's turn endings (the same ordering
        // `goal.pause` established).
        if let Some(session) = &aborted_session {
            let goal = session.goal();
            if goal.get().is_some_and(|goal| goal.status == GoalStatus::Active) {
                session
                    .record_op(WireOp::GoalUpdate {
                        goal_id: None,
                        status: Some(goal_status_wire(GoalStatus::Paused).to_string()),
                        reason: Some("Paused after interruption".to_string()),
                        turns_used: None,
                        tokens_used: None,
                        wall_clock_ms: None,
                        budget_limits: None,
                        actor: Some("user".to_string()),
                    })
                    .await;
                goal.update_with_reason(
                    GoalStatus::Paused,
                    Some("Paused after interruption".to_string()),
                );
                if let Ok(artifacts) = self.resolve_session_artifacts(session_id) {
                    let snapshot = goal
                        .get()
                        .map(|current| goal_snapshot_json(&current, &goal))
                        .unwrap_or(Value::Null);
                    artifacts.event_bus.publish(SessionEvent::GoalUpdated {
                        snapshot,
                        change: Some(serde_json::json!({
                            "kind": "lifecycle",
                            "status": "paused",
                            "reason": "Paused after interruption",
                            "actor": "user",
                        })),
                    });
                }
            }
        }
```

（import 按文件既有风格补：`GoalStatus`、`goal_status_wire`、`goal_snapshot_json`——session_handlers.rs 的 pause handler 同款。）

- [ ] **Step 4: 跑测试确认通过**

```bash
cd rust && cargo test -p mirri-embedded --lib should_pause_the_goal_when_message_abort
cargo test -p mirri-embedded --lib goal
cargo test -p mirri-embedded --lib b2_notification_tests
cargo test -p mirri-embedded
```

- [ ] **Step 5: Commit**

```bash
git commit -m "fix(embedded): park the active goal as paused when a turn is cancelled" -- rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs rust/crates/mirri-embedded/src/embedded_server/tests.rs rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs
```

---

## 验收（主控执行，不派发）

1. `cargo test` 三 crate 全绿 + clippy + zero-io（Task 5 已含）。
2. **rust+web 实测**：`cargo run --manifest-path rust/Cargo.toml -p mirri-cli -- serve` + `MIRRICODE_BACKEND=rust pnpm -C apps/mirri-web run dev`，用 Playwright 驱动 web：发起一个会 spawn 后台任务的 goal 模式 prompt → 中途 Stop（注入 turn 真停、思考状态清除）→ Pause（goal 状态 Paused、无新 turn、moon 清除）→ Resume（挂起通知恢复投递）。
3. 更新本文件勾选状态与 goal-ROADMAP。

## Self-Review 记录

- 覆盖：G1（Task 2/3）、G2（Task 4）、G3（Task 1）、G4 turn 取消环停（Task 6，实测验收发现后补）、web 契约（无需 web 改动：goal.updated 事件流不变、turn.ended 清 moon 复用前一批修复）、文档（Task 5）。
- 类型一致性：`InFlightTurnsMap`/`WaitTurnGuard` 在 Task 2 定义、Task 3/4 消费；`CancelAwareBlockingLlm` Task 2 定义、Task 3/4 复用；`turn_stop_reason_wire(Aborted) == "cancelled"`。
- 已知边界：非 cancel-aware 的挂死 provider 流仍受 300s `ENGINE_TURN_TIMEOUT` 兜底（既有文档化行为，不改变）。
