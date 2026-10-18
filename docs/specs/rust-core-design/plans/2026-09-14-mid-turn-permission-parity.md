# Mid-Turn Permission Parity（session-scope 审批记忆）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 turn 执行中的权限修改（session-scope 审批决定、mode 切换）通过所有答复通道都正确反映到执行中 turn 的下一个 tool call，与 TS v2 引擎语义对齐，并补齐回归测试。

**Architecture:** 不改 turn 执行管线的两阶段结构（先查证后执行，与 TS 一致）。核心修复是把 `scope: "session"` 语义从 `approval.decide` / `interaction.respond` 两条 gate 通道贯穿到 `PermissionManager` 的 session approvals 存储——复用 `approval.ask` 反向 RPC 通道已有的 `record_approval` 写回路径，零新增 wire op。

**Tech Stack:** Rust（mirri-core / mirri-embedded crates）、TypeScript（apps/mirri-web，仅透传一个字段）。

## Global Constraints

- 所有 rust 命令在 `rust/` 目录下执行：`cargo test -p mirri-core`、`cargo test -p mirri-embedded`、`cargo clippy --workspace --all-targets -- -D warnings`。
- zero-IO 守卫：repo 根目录 `node rust/scripts/check-zero-io.mjs` 必须 exit 0（本计划新增代码不做 IO，天然满足）。
- 测试命名 Given-When-Then（`should ... when ...` 风格，参考各文件现有测试）。
- wire log append-only：本计划**不新增** wire op（复用 `WireOp::PermissionRecordApprovalResult` 及其既有 reducer 路径），无 reducer 变更。
- 提交信息 Conventional Commit（`fix(permission): ...`），不加 co-author，不暴露 agent 身份。
- 完成后运行 gen-changesets skill 生成 changeset（默认 minor；major 必须先问用户）。

## 背景与修正（实施者必读）

**问题定义**：用户在 turn 执行中修改权限（点击 "session 允许"、切换 mode），预期反映到执行中 turn 的下一个 tool call。

**调查结论**：

1. **"batch 门控粒度差异"不成立，不要去"修"它。** TS v2 引擎同样是 per-step「顺序 prepare 全部（含权限门）→ 统一 executeBatch」（`packages/agent-core-v2/src/agent/toolExecutor/toolExecutorService.ts:207-224`），Rust 的 `run_tool_batch`（`rust/crates/mirri-core/src/tool_registry/execution.rs:478-504`）结构一致。两边语义相同：审批挂起期间改权限，同 step prepare 队列中后续 call 生效；执行期间改权限，下一 step 生效。这是两边共同的设计意图，不需要改动。

2. **真实缺口 A（本计划主体）**：session-scope 决定在 gate 通道被丢弃。
   - `approval.ask` 反向 RPC 回复带 `scope: "session"` 时正确记录（`rust/crates/mirri-core/src/turn_flow/hooks.rs:944-1054`：`record_result` 带 scope → `session_approved` → wire 记录 + `permission_manager.record_approval`）。
   - 但 `ApprovalDecision` 枚举没有 scope（`rust/crates/mirri-core/src/session/gates.rs:100-108`：`Approved | Denied(String) | Timeout`），导致 `approval.decide`（`rust/crates/mirri-embedded/src/embedded_server/interaction_handlers.rs:55-98`，params 只有 `approved: bool`）和 `interaction.respond`（`rust/crates/mirri-core/src/session/interaction.rs:148-180`，只解析 `{decision, feedback}`）两条通道的"session 允许"不会被记住，同 pattern 的下一个 tool call 会再次弹审批。
   - web 端已经发送 `scope: 'session'`，但 rust 端丢弃——`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:758-759` 注释原文："the current approval.decide params have no remember field — dropped"。

3. **真实缺口 B**：无任何「turn 执行中改权限 → 断言后续 tool call 行为」的测试；`PermissionManager::set_mode`（`rust/crates/mirri-core/src/permission/mod.rs:456`）零测试覆盖。

---

### Task 1: `ApprovalDecision::ApprovedForSession` 变体（mirri-core gates.rs）

**Files:**
- Modify: `rust/crates/mirri-core/src/session/gates.rs:100-108`（枚举）、`gates.rs:313`（resolved-event 布尔）
- Test: `rust/crates/mirri-core/src/session/gates.rs`（文件内 `#[cfg(test)]` 模块，样板见同文件 `should_record_resolved_ids_in_the_ring_when_approval_resolved` :786）

**Interfaces:**
- Produces: `ApprovalDecision::ApprovedForSession`（新变体）。语义：批准且记忆为 session 级规则。消费方：Task 2（interaction.rs 构造）、Task 3（interaction_handlers.rs 构造、hooks.rs 匹配）。
- 设计裁决：用**新变体**而不是给 `Approved` 加字段——auto-approve（gates.rs:221）、既有测试、hooks.rs:899 的 rpc 映射等所有 `Approved` 构造点零改动，只有消费 `Approved` 的 match 需要扩展。

- [ ] **Step 1: 写失败的测试**（加到 gates.rs 测试模块末尾，`should_answer_false_when_resolving_a_withdrawn_request` 之后）

```rust
/// Given a registered approval, when it is resolved with a session-scoped
/// approval, then the awaiting receiver gets `ApprovedForSession` (not plain
/// `Approved`), and the gate reports the id as resolved.
#[tokio::test]
async fn should_deliver_session_scoped_variant_when_resolved_with_approved_for_session() {
    // Given an approval gate with one pending request
    let gate = ApprovalGate::new(false);
    let (request_id, mut receiver) = gate.register(ApprovalSnapshot {
        tool_name: "Bash".to_string(),
        action: "execute Bash".to_string(),
        display: json!({}),
        agent_id: Uuid::nil(),
        turn_id: None,
        tool_call_id: Some("c1".to_string()),
        created_at: now_ms(),
        approval_rule: Some("Bash".to_string()),
    });

    // When the decision arrives with session scope
    assert!(gate.resolve(request_id, ApprovalDecision::ApprovedForSession));

    // Then the awaiting side receives the session-scoped variant
    let decision = receiver.recv().await.expect("the receiver resolves");
    assert!(matches!(decision, ApprovalDecision::ApprovedForSession));
    assert!(gate.is_recently_resolved(request_id));
}
```

- [ ] **Step 2: 运行确认编译失败**

Run: `cd rust && cargo test -p mirri-core session::gates`
Expected: 编译错误 `no variant named ApprovedForSession`

- [ ] **Step 3: 最小实现**

gates.rs:100-108 枚举改为：

```rust
pub enum ApprovalDecision {
    /// The tool call may proceed (one-off — nothing is memorized).
    Approved,
    /// The tool call may proceed AND the decision is memorized as a
    /// session-scoped approval rule (the gate-channel counterpart of the
    /// reverse-RPC reply `{ "decision": "approved", "scope": "session" }`).
    ApprovedForSession,
    /// The tool call must not proceed; `reason` explains why.
    Denied(String),
    /// No decision arrived in time (session closed via drain, or no
    /// responder posted one before the requester gave up).
    Timeout,
}
```

gates.rs:313 的 resolved-event 布尔扩展：

```rust
let approved = matches!(
    decision,
    ApprovalDecision::Approved | ApprovalDecision::ApprovedForSession
);
```

- [ ] **Step 4: 运行确认通过**

Run: `cd rust && cargo test -p mirri-core session::gates`
Expected: PASS（含新测试与既有测试）

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/session/gates.rs
git commit -m "feat(permission): add ApprovedForSession variant to ApprovalDecision"
```

---

### Task 2: `interaction.respond` 解析 scope（mirri-core interaction.rs）

**Files:**
- Modify: `rust/crates/mirri-core/src/session/interaction.rs:148-180`（approval 分支）
- Test: `rust/crates/mirri-core/src/session/interaction.rs`（文件内测试模块；若无则在文件尾新建 `#[cfg(test)] mod tests`，import 参照 gates.rs 测试模块）

**Interfaces:**
- Consumes: Task 1 的 `ApprovalDecision::ApprovedForSession`。
- Produces: `InteractionKernel::respond(id, response)` 接受 approval 响应携带 `scope: "session"`（klient 形状 `{decision, scope?, feedback?}`），映射为 `ApprovedForSession`。

- [ ] **Step 1: 写失败的测试**

```rust
use super::*;
use std::sync::Arc;
use uuid::Uuid;

/// Given a pending approval registered on the kernel's gate, when
/// `interaction.respond` carries `{decision: "approved", scope: "session"}`,
/// then the awaiting receiver resolves with the session-scoped variant.
#[tokio::test]
async fn should_memorize_session_scope_when_interaction_respond_carries_it() {
    // Given an interaction kernel whose approval gate holds one pending ask
    let approval_gate = Arc::new(ApprovalGate::new(false));
    let question_gate = Arc::new(QuestionGate::new());
    let kernel = InteractionKernel::new(
        Arc::clone(&approval_gate),
        Arc::clone(&question_gate),
    );
    let (request_id, mut receiver) = approval_gate.register(ApprovalSnapshot {
        tool_name: "Bash".to_string(),
        action: "execute Bash".to_string(),
        display: serde_json::json!({}),
        agent_id: Uuid::nil(),
        turn_id: None,
        tool_call_id: Some("c1".to_string()),
        created_at: now_ms(),
        approval_rule: Some("Bash".to_string()),
    });

    // When the response carries the session scope
    kernel
        .respond(
            &request_id.to_string(),
            serde_json::json!({ "decision": "approved", "scope": "session" }),
        )
        .expect("the approval resolves");

    // Then the awaiting side receives the session-scoped approval
    let decision = receiver.recv().await.expect("the receiver resolves");
    assert!(matches!(decision, ApprovalDecision::ApprovedForSession));

    // And a scope-less approval still resolves to the plain variant
    let (request_id2, mut receiver2) = approval_gate.register(ApprovalSnapshot {
        tool_name: "Bash".to_string(),
        action: "execute Bash".to_string(),
        display: serde_json::json!({}),
        agent_id: Uuid::nil(),
        turn_id: None,
        tool_call_id: Some("c2".to_string()),
        created_at: now_ms(),
        approval_rule: None,
    });
    kernel
        .respond(
            &request_id2.to_string(),
            serde_json::json!({ "decision": "approved" }),
        )
        .expect("the approval resolves");
    let decision2 = receiver2.recv().await.expect("the receiver resolves");
    assert!(matches!(decision2, ApprovalDecision::Approved));
}
```

（若 `ApprovalGate::register` 返回的 receiver 类型不是 `recv()` 接口，按 gates.rs:196-249 的既有 await 用法对齐——不要改 gate 本身。）

- [ ] **Step 2: 运行确认失败**

Run: `cd rust && cargo test -p mirri-core session::interaction`
Expected: FAIL —— 第二个断言处 plain-variant 场景通过，但第一个断言收到 `Approved`（scope 被忽略），`matches!(decision, ApprovedForSession)` 为 false。

- [ ] **Step 3: 最小实现**

interaction.rs:160-168 的 match 改为：

```rust
let scope = response.get("scope").and_then(Value::as_str);
let approval = match decision {
    "approved" => match scope {
        Some("session") => ApprovalDecision::ApprovedForSession,
        _ => ApprovalDecision::Approved,
    },
    "rejected" => ApprovalDecision::Denied(
        response
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or("denied by user")
            .to_string(),
    ),
    "cancelled" => ApprovalDecision::Denied("cancelled by user".to_string()),
    other => {
        return Err(InteractionRespondError::InvalidParams(format!(
            "unknown approval decision: {other}"
        )));
    }
};
```

- [ ] **Step 4: 运行确认通过**

Run: `cd rust && cargo test -p mirri-core session::interaction`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add rust/crates/mirri-core/src/session/interaction.rs
git commit -m "feat(permission): parse session scope in interaction.respond approvals"
```

---

### Task 3: 端到端——`approval.decide` 带 scope 记住 session 审批

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/interaction_handlers.rs:59-88`（`DecideParams` + decision 构造）
- Modify: `rust/crates/mirri-core/src/turn_flow/hooks.rs:992-1016`（`AskChannel::Gate` 匹配分支）
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（样板：`should_list_and_decide_pending_approvals_while_approval_wait_in_flight` :16304；helpers `poll_until_pending` :16271、`poll_agent_activity`、`test_server`、`ScriptedLlm` 均已存在）

**Interfaces:**
- Consumes: Task 1 的 `ApprovedForSession`。
- Produces: `approval.decide` RPC 接受可选 `scope: "session"` 参数；`AskChannel::Gate(ApprovedForSession)` 产出 `record_result = {"decision":"approved","scope":"session"}`，流入 hooks.rs:1028-1054 既有的 `session_approved` 路径（wire 记录 `sessionApprovalRule` + `permission_manager.record_approval(approval_rule)`）。

- [ ] **Step 1: 写失败的 e2e 测试**（加到 tests.rs :16416 之后，B4-a 测试区）

先在 `never_answering_handler`（:16289）旁加一个计数版 helper：

```rust
/// A reverse-RPC handler that never answers but counts `approval.ask`
/// calls — proves how many times the engine actually asked the user.
fn counting_never_answering_handler(ask_count: Arc<std::sync::atomic::AtomicUsize>) -> ReverseRpcHandler {
    Arc::new(move |method: String, _params: Value| {
        let ask_count = Arc::clone(&ask_count);
        Box::pin(async move {
            if method == "approval.ask" {
                ask_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            futures::future::pending::<()>().await;
            unreachable!("the pending future never resolves")
        })
    })
}
```

测试本体：

```rust
/// Given two same-pattern tool calls in one step with the first approval
/// decided through the gate channel, when the decision carries session
/// scope, then the second call runs without asking again.
#[tokio::test]
async fn should_not_ask_again_when_pending_approval_is_decided_with_session_scope() {
    // Given a never-answering (counting) reverse-RPC handler and a scripted
    // LLM that calls Bash twice in one step, then ends the turn
    let ask_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![
        mirri_core::LlmToolCall {
            id: "call_bash_1".to_string(),
            name: "Bash".to_string(),
            args: json!({ "command": "echo one" }),
        },
        mirri_core::LlmToolCall {
            id: "call_bash_2".to_string(),
            name: "Bash".to_string(),
            args: json!({ "command": "echo two" }),
        },
    ]);
    mock.push_text("done");
    let (server, _tmp) =
        test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    server
        .on_reverse_rpc(counting_never_answering_handler(Arc::clone(&ask_count)))
        .await
        .unwrap();
    let created = server.call_method("session.create", json!({})).await.unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the fire-mode turn reaches the first approval wait
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "run", "mode": "fire" }),
        )
        .await
        .unwrap();
    let rows = poll_until_pending(&server, &session_id, "approval.listPending").await;
    assert_eq!(rows.len(), 1, "params: {rows:?}");
    assert_eq!(rows[0]["toolName"], json!("Bash"));
    let request_id = rows[0]["id"].as_str().unwrap().to_string();

    // And the approval is decided with session scope through the gate channel
    let decided = server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": true,
                "scope": "session"
            }),
        )
        .await
        .unwrap();
    assert_eq!(decided, json!({ "ok": true }));

    // Then the turn unblocks and completes (both Bash calls ran)
    let end = poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;
    assert_eq!(end["lastTurn"]["reason"], "completed");

    // And the second same-pattern call never asked (session approval
    // memorized through the gate channel)
    assert_eq!(
        ask_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the session-scoped decision must suppress the second approval.ask"
    );
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd rust && cargo test -p mirri-embedded should_not_ask_again_when_pending_approval_is_decided_with_session_scope`
Expected: FAIL —— 末尾断言 `ask_count == 2`（第二个 Bash 再次 ask，gate 通道的 session scope 被丢弃）。注意：`DecideParams` 的 serde 尚不接受 `scope` 字段但未知字段默认忽略，所以测试能到达语义断言处，正好红在正确的原因上。

- [ ] **Step 3: 实现（两个 crate，三个改动点）**

① `interaction_handlers.rs` `DecideParams` 增加 scope，decision 构造（:59-88）：

```rust
struct DecideParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    #[serde(rename = "requestId", alias = "request_id")]
    request_id: String,
    approved: bool,
    #[serde(default)]
    reason: Option<String>,
    /// `Some("session")` memorizes the approval as a session-scoped rule
    /// (the klient `ApprovalResponse.scope` field; anything else or absent
    /// is a one-off decision).
    #[serde(default)]
    scope: Option<String>,
}
```

```rust
let decision = if params.approved {
    if params.scope.as_deref() == Some("session") {
        ApprovalDecision::ApprovedForSession
    } else {
        ApprovalDecision::Approved
    }
} else {
    ApprovalDecision::Denied(
        params
            .reason
            .unwrap_or_else(|| "denied by user".to_string()),
    )
};
```

（保持 `#[serde(rename_all = "camelCase")]` 等既有 derive 不动。）

② `turn_flow/hooks.rs:992` 的 Gate 分支前插入新 arm：

```rust
AskChannel::Gate(crate::session::gates::ApprovalDecision::ApprovedForSession) => {
    (
        PermissionResultOutcome::Approved,
        Some(json!({ "decision": "approved", "scope": "session" })),
    )
}
```

`record_result` 带 `scope:"session"` 后，hooks.rs:1028-1054 既有的 `session_approved` 判定自动成立 → wire 记录带 `sessionApprovalRule` + `permission_manager.record_approval(approval_rule)`，无需其他改动。

③ 同步更新 hooks.rs:809-1054 区段与 `dispatch_approval_decide`（interaction_handlers.rs:50-54）中描述"decide 无 scope"的过时注释，改为如实描述新行为。

- [ ] **Step 4: 运行确认通过**

Run: `cd rust && cargo test -p mirri-embedded should_not_ask_again_when_pending_approval_is_decided_with_session_scope`
Expected: PASS

- [ ] **Step 5: 回归**

Run: `cd rust && cargo test -p mirri-embedded && cargo test -p mirri-core`
Expected: 全绿（尤其 :16304 的既有 decide 测试不受影响——不带 scope 仍走 plain `Approved`）。

- [ ] **Step 6: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/interaction_handlers.rs \
        rust/crates/mirri-core/src/turn_flow/hooks.rs \
        rust/crates/mirri-embedded/src/embedded_server/tests.rs
git commit -m "fix(permission): memorize session-scoped approvals decided via approval.decide"
```

---

### Task 4: mid-turn mode 切换回归测试（characterization）

**Files:**
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（紧跟 Task 3 的测试之后）

**Interfaces:**
- Consumes: 既有 `agent.setPermission` RPC（`session_handlers.rs:2534`，mode 序列化为小写 `"manual"` / `"yolo"` / `"auto"`）、Task 3 的 `counting_never_answering_handler`。
- Produces: 无代码产出——这是对既有行为的特性测试（expected-green）。**若红，说明 mid-turn mode 热更新存在真 bug，停下来报告，不要为绿而改断言。**

- [ ] **Step 1: 写测试**

```rust
/// Given a turn blocked on a Manual-mode approval, when the permission
/// mode is switched to yolo mid-turn while the ask is pending, then the
/// next step's same-pattern tool call auto-approves (no second ask).
#[tokio::test]
async fn should_apply_permission_mode_change_to_next_tool_call_when_changed_mid_turn() {
    // Given a never-answering (counting) reverse-RPC handler and a scripted
    // LLM: step 1 calls Bash once (Manual → ask), step 2 calls Bash again
    let ask_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock = Arc::new(ScriptedLlm::new());
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_bash_1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo one" }),
    }]);
    mock.push_tool_use(vec![mirri_core::LlmToolCall {
        id: "call_bash_2".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo two" }),
    }]);
    mock.push_text("done");
    let (server, _tmp) =
        test_server(Some(Arc::new(ScriptedLlmFactory::new(Arc::clone(&mock)))));
    server
        .on_reverse_rpc(counting_never_answering_handler(Arc::clone(&ask_count)))
        .await
        .unwrap();
    let created = server.call_method("session.create", json!({})).await.unwrap();
    let session_id = created["id"].as_str().unwrap().to_string();

    // When the turn reaches the first approval wait
    server
        .call_method(
            "message.send",
            json!({ "session_id": &session_id, "text": "run", "mode": "fire" }),
        )
        .await
        .unwrap();
    let rows = poll_until_pending(&server, &session_id, "approval.listPending").await;
    let request_id = rows[0]["id"].as_str().unwrap().to_string();

    // And the mode is switched to yolo while the ask is still pending
    server
        .call_method(
            "agent.setPermission",
            json!({ "session_id": &session_id, "mode": "yolo" }),
        )
        .await
        .unwrap();

    // And the pending ask is answered once (one-off, no session scope)
    server
        .call_method(
            "approval.decide",
            json!({
                "session_id": &session_id,
                "requestId": &request_id,
                "approved": true
            }),
        )
        .await
        .unwrap();

    // Then the turn completes and the second Bash call never asked
    let end = poll_agent_activity(&server, &session_id, |state| {
        state["turn"].is_null() && state["lastTurn"].is_object()
    })
    .await;
    assert_eq!(end["lastTurn"]["reason"], "completed");
    assert_eq!(
        ask_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the mid-turn mode change must reach the next tool call's authorize"
    );
}
```

- [ ] **Step 2: 运行**

Run: `cd rust && cargo test -p mirri-embedded should_apply_permission_mode_change_to_next_tool_call_when_changed_mid_turn`
Expected: PASS（`set_mode` 直改 `RwLock`，下一次 `authorize_tool_execution` 现读新 mode）。若 FAIL：这是真 bug，报告并停下。

- [ ] **Step 3: Commit**

```bash
git add rust/crates/mirri-embedded/src/embedded_server/tests.rs
git commit -m "test(permission): cover mid-turn mode change reaching next tool call"
```

---

### Task 5: `set_mode` 单元测试（mirri-core permission/mod.rs）

**Files:**
- Test: `rust/crates/mirri-core/src/permission/mod.rs`（测试模块，样板见 `should_evaluate_new_decision_when_set_rules_swaps_rules_at_runtime` :715）

**Interfaces:**
- Consumes: `PermissionManager::new` / `set_mode` / `mode`。
- Produces: 锁定 set_mode 的 live-read 契约（turn 中途调用安全，仅影响后续求值——见 permission/mod.rs:453-455 设计注释）。

- [ ] **Step 1: 写测试**（加到 :715 测试之后）

```rust
/// Given a manager built in Manual mode, when set_mode swaps the mode at
/// runtime, then mode() reports the new value — the live-read contract the
/// authorize hook relies on (each tool call reads the mode at evaluation
/// time; no snapshot, no restart needed).
#[test]
fn should_return_new_mode_when_set_mode_changes_it_at_runtime() {
    let manager = PermissionManager::new(PermissionMode::Manual, vec![]);
    assert_eq!(manager.mode(), PermissionMode::Manual);

    manager.set_mode(PermissionMode::Yolo);

    assert_eq!(manager.mode(), PermissionMode::Yolo);
}
```

- [ ] **Step 2: 运行确认通过**（该方法此前零覆盖，此测试预期直接绿——它锁定契约而非驱动新实现）

Run: `cd rust && cargo test -p mirri-core should_return_new_mode_when_set_mode_changes_it_at_runtime`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add rust/crates/mirri-core/src/permission/mod.rs
git commit -m "test(permission): cover runtime set_mode live-read contract"
```

---

### Task 6: web 透传 scope 到 `approval.decide`

**Files:**
- Modify: `apps/mirri-web/src/api/daemon/ws-rpc-api.ts:751-760`

**Interfaces:**
- Consumes: Task 3 的 rust `approval.decide` 可选 `scope` 参数；`ApprovalResponse` 类型已含 `scope?: 'session'`（`apps/mirri-web/src/api/daemon/wire.ts:305`）。
- Produces: web 的"session 允许"在 parked-ask 之外的 fall-through 路径也能记住决定。

- [ ] **Step 1: 修改**（ws-rpc-api.ts:751-760）

```ts
    const reason = response.feedback ?? response.selectedLabel;
    try {
      await this.client.call<{ ok: boolean }>('approval.decide', {
        session_id: sessionId,
        request_id: approvalId,
        approved: response.decision === 'approved',
        ...(reason !== undefined ? { reason } : {}),
        ...(response.scope !== undefined ? { scope: response.scope } : {}),
      });
    } catch (error) {
```

并删除 758-759 的过时注释（"the current approval.decide params have no remember field — dropped"）。

- [ ] **Step 2: 类型检查 + 相关测试**

Run: `pnpm -C apps/mirri-web run typecheck && pnpm -C apps/mirri-web run test -- --run src/api`
Expected: typecheck 通过；无相关测试失败。（若 package.json 无 `typecheck` script，用其等价的 `vue-tsc --noEmit` script。）

- [ ] **Step 3: Commit**

```bash
git add apps/mirri-web/src/api/daemon/ws-rpc-api.ts
git commit -m "fix(web): pass approval scope through to approval.decide"
```

---

### Task 7: 全量门禁 + changeset

- [ ] **Step 1: rust 全量质量门禁**（按 rust/AGENTS.md）

Run: `cd rust && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: 全绿零告警。若有既有无关红测，报告用户，不要顺手修无关代码。

- [ ] **Step 2: zero-IO 守卫**

Run: `node rust/scripts/check-zero-io.mjs`
Expected: exit 0

- [ ] **Step 3: changeset** —— 调用 gen-changesets skill（`.agents/skills/gen-changesets/SKILL.md`）生成 changeset。行为修复 + 新增可选参数，默认 minor；不要自行判 major。

---

## 并行性说明

- Task 1 → Task 2 → Task 3 严格串行（类型依赖链：变体 → 消费方）。
- Task 4、Task 5 可在 Task 3 后并行（Task 4 复用 Task 3 的 `counting_never_answering_handler`；Task 5 无依赖）。
- Task 6 与 rust 任务无文件冲突，可与 Task 4/5 并行。
- Task 7 必须最后串行执行。
