# 会话一致性 · 批次 1.5 + 批次 2 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复用户实测发现的双实例缺口——desktop 跑子 agent 时 web 端 15 分钟空白（批次 1.5：子 journal 跟随）；补齐写侧冲突防护（批次 2：提交围栏 + 回合中止 + state 三路合并 + 索引对账串行化）。

**Architecture:** 批次 1.5 扩展吸收器到子 journal：`subagent.registered` 吸收时惰性建子 AgentScope + `child_wire_log` 锚，调度器每 tick 探测主 + 全部子 journal，子 agent 事件经事件总线投影到 web（`event.subagent.started` 投影已存在）。批次 2 在 message 落盘前做版本裁决（ADR-002），回合中工具批执行前检查（ADR-001），state.json 三路合并（ADR-003-R1），对账套 D8 锁（ADR-004）。

**Tech Stack:** rust/crates：mirri-core、mirri-embedded、mirri-session-index；既有 flag `session-consistency-follow`。

## Global Constraints

- 规格：`docs/specs/session-consistency-architecture.md` §2.4 三条不变量、ADR-001~005；批次 2 的验收 = US C2.1/C2.2/C2.3、C3.1/C3.2；批次 1.5 的验收 = US C1.2 对子 agent 内容成立。
- 围栏拦截必须发生在用户消息落盘之前（ADR-002：磁盘零写入、消息回填由客户端处理）。
- 三路合并不依赖 schema 演进（ADR-003-R1：共同祖先 = 自己上次写出的快照）。
- 跟随/围栏/合并全部在 `session-consistency-follow` flag 后，默认关闭。
- 测试 Given-When-Then；cargo 后台长超时（≥1200s）；提交 Conventional Commit 不暴露 agent 身份。
- ROADMAP 裁剪条目必须附"用户可见影响"一行（新规则，存量条目补齐）。
- **用户场景回归测试必须有**：双实例 A（desktop）派子 agent 干活、B（web）跟随，断言 B 在 5 秒内收到子 agent 内容事件。

## 已核实代码事实（实现者必读）

- **消息落盘点**：`message_handlers.rs` `send_message_with_origin`（:405）→ turn_recorder.record_op(TurnPrompt + ContextAppendMessage)（:769-783）。围栏检查插在 :755 附近（ingest/blob 处理之后、record_op 之前）——params 与 session 均已就绪。
- **回合工具执行点**：`run_engine_turn`（:130）→ `execute_tool_calls`（mirri-core/src/loop_engine/tool_call_batch.rs:50；turn_step.rs:737 调用）。中止机制：TurnCancel op + in_flight_turns 注销路径已存在（follow_scheduler.rs 已用）。
- **EngineError**：mirri-core/src/error.rs:139 起，`RequestInvalid(String)` 等变体；transport 层 adapter.rs 把 EngineError 映射为带 code 的错误。**新增变体** `SessionConflict { session_id: String }`——实现时读 adapter.rs 的映射处补 domain code `session.conflict`。
- **state 写入**：`SessionPersistence::save_state`（persistence.rs:336）——`AtomicDocument::write` 直接覆盖；调用方在 manager/mod.rs:444/571/1542 与 embedded 侧若干。**三路合并**插在 save_state 内部：原子写前 stat state.json mtime/len ≠ 自己上次写的 → 读盘上版本做字段级合并。
- **D8 锁**：`RegistryWriter`（mirri-session-index/src/registry.rs:228+），`acquire_lock() -> LockGuard`（:297），`with_timeouts(path, stale, wait)` 测试缝现成。锁是私有方法——需要把 `acquire_lock` 泛化为 `pub(crate)` 的通用文件锁助手或导出一个小模块供 embedded 复用。
- **子 agent 建 scope**：`SessionScope::create_agent(id, config) -> EngineResult<Arc<AgentScope>>`（session_scope.rs:1420）——已存在 agent 会报错（幂等由调用方保证）。config 来源：`build_agent_config_from_state`（restore.rs:994）的子 agent 版本在 restore_subagents（:1231-1347）里逐字段构造；吸收路径需要一份精简版（model/profileName 从 registration op 取）。
- **子 journal SessionWireLog**：`SessionScope::child_wire_log(agent_id)`（session_scope.rs:820 起，agent_wire_logs map）——与主 journal 同类型，drain_foreign_ops 可直接用；惰性打开用 `open_for_append`（不截断）。
- **子 agent 事件投影已存在**：`event.subagent.started`（event_projection.rs:703）+ `SessionEvent::SubagentStarted { subagent_id }`（events.rs:553）+ web 端 eventReducer/agentEventProjector 的 patchSubagent 已处理（eventReducer.ts:32-41 注释）。**web 渲染侧 G3 仍缺**（rpc.rs:1937 subagents 恒 []）——本批不动渲染，依赖项写入 ROADMAP。

## File Structure

**批次 1.5（Task 1-2）**：
- Modify: `rust/crates/mirri-core/src/session/consistency.rs` — apply_absorbed_op 处理 SubagentRegistered（惰性建子 scope + journal 锚）
- Modify: `rust/crates/mirri-core/src/session/manager/mod.rs` — absorb_session_foreign_updates 扩展子 journal drain
- Modify: `rust/crates/mirri-embedded/src/embedded_server/follow_scheduler.rs` — tick 探测主+子 journal
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests/t_session_consistency_tests.rs`

**批次 2（Task 3-6）**：
- Modify: `rust/crates/mirri-core/src/error.rs` — SessionConflict 变体
- Modify: `rust/crates/mirri-transport/src/adapter.rs` — domain code 映射
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs` — 提交围栏（:755 附近）+ 回合中止（run_engine_turn 工具批前）
- Modify: `rust/crates/mirri-core/src/session/persistence.rs` — save_state 三路合并
- Modify: `rust/crates/mirri-session-index/src/registry.rs` — 锁原语导出
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_index.rs` — 对账套锁
- Modify: `rust/crates/mirri-server/src/` — 若 adapter 映射在此则改（以实际为准）
- Modify: `apps/mirri-web/src/` — message.send 失败的 SessionConflict 回填输入框（读 useMirriWebClient 的 send 失败路径后落点确认）
- Modify: `docs/specs/session-consistency-ROADMAP.md` — 裁剪条目补"用户可见影响"；G3 立项依赖
- Create: `.changeset/*.md`

---

### Task 1: 子 agent 惰性吸收（批次 1.5 核心）

**Files:**
- Modify: `rust/crates/mirri-core/src/session/consistency.rs`（apply_absorbed_op）
- Modify: `rust/crates/mirri-core/src/session/manager/mod.rs`（absorb_session_foreign_updates）
- Test: consistency.rs 模块测试 + manager 测试

**Interfaces:**
- Consumes: 批次 1 的 `drain_foreign_ops`、`known_size`；`SessionScope::create_agent`、`child_wire_log`。
- Produces: `apply_absorbed_op` 的 `WireOp::SubagentRegistered { agent_id, model, profile_name, parent_agent_id, .. }` 分支（field 名以 wire.rs:705 实际为准——注意磁盘是 camelCase `agentId`）；`absorb_session_foreign_updates` 在主 journal drain 后遍历 `session.subagent_ids()`（若无此访问器则经 agents map）逐个 `child_wire_log(id)` drain 并应用到对应 AgentScope 的 context_memory + per-agent 事件。

- [ ] **Step 1: 失败测试**（consistency.rs）：

```rust
#[tokio::test]
async fn should_lazily_create_child_scope_and_absorb_its_journal_when_subagent_registered_arrives() {
    // Given: 一个 SessionWireLog（主 journal，带 metadata 头 + subagent.registered op）
    //        + 对应 agents/<id>/wire.jsonl（预写 2 条 message op）
    // When: absorb（走 apply_absorbed_op）
    // Then: session.agents 里有该子 agent；其 context_memory 消息数 == 2；
    //       子 journal 的 known_size == 文件长（锚已初始化）
}

#[tokio::test]
async fn should_absorb_foreign_child_journal_updates_on_each_tick() {
    // Given: 子 scope 已建、锚停在 N 字节；外部向子 journal 追加 1 条 message
    // When: 再次 drain（absorb_session_foreign_updates）
    // Then: 子 agent memory +1；事件总线收到对应 MessageReceived（agent 维度）
}
```

（SessionScope 的构造用 restore 路径或最小 scope；`SubagentRegistered` 的 config 构造：model/profile 从 op 字段直接映射，缺省回 default——不照抄 restore_subagents 的全部字段，最小可用即可，注释说明。）

- [ ] **Step 2: 红** — `cargo test -p mirri-core consistency`（后台）
- [ ] **Step 3: 实现**——apply_absorbed_op 加 SubagentRegistered 分支：`session.create_agent(AgentId::new(&agent_id), child_config)`（幂等：already exists 则跳过）+ `child_wire_log(agent_id)` 触发惰性打开（open_for_append）；absorb_session_foreign_updates 主 drain 后 for-each 子 agent drain（on_op 复用 apply_absorbed_op 的 per-agent 定向）。Truncated 子 journal 同主 journal 语义（停跟随该子）。
- [ ] **Step 4: 绿 + clippy**
- [ ] **Step 5: 提交** — `feat(core): lazily materialize subagent scopes and absorb child journals`

### Task 2: 调度器扩展 + 用户场景回归（批次 1.5 收口）

**Files:**
- Modify: `follow_scheduler.rs`（tick 探测主+全部子 journal——实际由 absorb_session_foreign_updates 内部遍历实现，调度器只需在偏差时调用它；确认子 journal 探测不增加每 tick 成本：无偏差时纯 stat）
- Test: `tests/t_session_consistency_tests.rs`

**Interfaces:**
- Consumes: Task 1。

- [ ] **Step 1: 用户场景测试（goal 验收锚）**：

```rust
#[tokio::test]
async fn should_show_subagent_activity_in_web_within_five_seconds_when_desktop_runs_child_task() {
    // Given: 双实例共享 home，flag 开，tick 50ms；B subscribe 事件流
    //        A 侧走真实 Task/Agent 工具路径派一个子 agent（用 ScriptedLlm 或
    //        直接构造子 agent journal 写入——优先真实路径，见 tests.rs 现有
    //        subagent 测试先例，搜 spawn_subagent / subagent helper）
    // When: A 的子 agent 持续写子 journal
    // Then: B 在 5 秒内收到 SubagentStarted + 子 agent 的 message 事件
    // And: B 侧 session.agents 含该子 agent
}
```

- [ ] **Step 2: 红 → Step 3: 实现（调度器无需大改则只补测试）→ Step 4: 绿 + clippy → Step 5: 提交**

### Task 3: SessionConflict 错误 + 提交围栏（批次 2 核心）

**Files:**
- Modify: `mirri-core/src/error.rs`、`mirri-transport/src/adapter.rs`（或映射实际所在）
- Modify: `message_handlers.rs`（:755 附近插围栏）
- Test: t_session_consistency_tests.rs + adapter 单测

**Interfaces:**
- Produces: `EngineError::SessionConflict { session_id: String }` → transport code `session.conflict`（形态对齐 adapter.rs 既有 domain code 写法）。

- [ ] **Step 1: 失败测试**：双实例，A 先写、B 提交 → B 收到 session.conflict 错误且磁盘零新增（wire line count 不变）；无偏差时提交正常。
- [ ] **Step 2: 红 → Step 3: 实现**——send_message_with_origin 在 record_op 之前：probe 主 journal + known_size 比对（复用 consistency::check_version）→ Divergent/Truncated 时返回 SessionConflict（落盘前，ADR-002）；探测失败按冲突处理（宁拦勿写，spec §5）。
- [ ] **Step 4: 绿 + clippy → Step 5: 提交**

### Task 4: 回合中止（回合中工具批前检查）

**Files:**
- Modify: `message_handlers.rs` run_engine_turn 工具批调度点（读 turn_step.rs:737 的调用链找 embedded 侧可插点；若引擎层更合适则改 `tool_call_batch.rs` 并带 flag 短路——**读代码后定，倾向 embedded 层 turn 循环内**）
- Test: t_session_consistency_tests.rs

- [ ] **Step 1: 失败测试**：B 回合进行中（gated 慢 turn），A 写入 → B 的回合在下个工具批前中止（turn.ended reason=cancelled 或专用 reason）+ 中止记录落盘 + 已完成部分保留。
- [ ] **Step 2: 红 → Step 3: 实现 → Step 4: 绿 + clippy → Step 5: 提交**

### Task 5: state.json 三路合并（ADR-003-R1）

**Files:**
- Modify: `mirri-core/src/session/persistence.rs`（save_state 内）
- Test: persistence.rs 既有测试模块

- [ ] **Step 1: 失败测试**：A 改标题、B 归档并发 → 后写者合并后两者都在；同字段后写者胜；单侧写无变化。
- [ ] **Step 2: 红 → Step 3: 实现**——SessionPersistence 增 `last_written: Mutex<HashMap<SessionId, (u64 len, u64 mtime_ms, PersistedSessionMetadata snapshot)>>`；save_state 前 stat ≠ 自己上次 → 读盘上 JSON 做三路字段合并（共同祖先 = snapshot）→ 原子写 → 更新 last_written。字段清单以 PersistedSessionMetadata 的 serde 字段为准（读后枚举，逐字段三路）。
- [ ] **Step 4: 绿 + clippy → Step 5: 提交**

### Task 6: 索引对账 D8 锁 + ROADMAP/changeset/收口

**Files:**
- Modify: `mirri-session-index/src/registry.rs`（锁原语导出为 `pub(crate)` 或独立 `lock_file` 模块）
- Modify: `mirri-embedded/src/embedded_server/session_index.rs`（reconcile_session_index_via 套锁；锁等待超时跳过本轮——对齐 ADR-004 的降级语义）
- Modify: `docs/specs/session-consistency-ROADMAP.md`（存量裁剪条目补"用户可见影响"行；G3 立项：web snapshot subagents 名册+转录投影，标注依赖批次 1.5 已完成）
- Modify: `apps/mirri-web/src/composables/useMirriWebClient.ts`（send 失败 code=session.conflict 时消息回填输入框 + toast——读现有 send 错误处理路径后插入；i18n 文案 en/zh）
- Create: `.changeset/session-consistency-write-side.md`（minor）

- [ ] **Step 1: 失败测试**（对账串行化：双实例同时触发 reconcile，断言无行丢失——复用批次 1 测试的构造）；web 回填（若有组件测试基建则加，否则以 e2e 手测清单记录在报告）
- [ ] **Step 2: 红 → Step 3: 实现（含 web 回填）→ Step 4: 全量验证**（cargo test 三 crate + clippy workspace + pnpm --filter @mirri-ai/mirri-web typecheck）**→ Step 5: 提交**

## Parallelization

Task 1→2 串行；Task 3→4 串行（同文件）；Task 5、6 相互独立可与 3/4 并行（但均动核心文件，**建议全程串行**：1→2→3→4→5→6）。Task 6 收口必须最后。

## Self-Review

- 覆盖：US C1.2（子 agent）→Task 1/2；C2.1→Task 3；C2.2→Task 4；C3.1→Task 5；C3.2→Task 6；C2.3（文案）→Task 6 web toast；G3→ROADMAP 立项（goal 边界内）。
- 用户场景回归 = Task 2 Step 1（goal 的验收锚）。
- 无占位符；类型与批次 1 的接口（drain/known_size/check_version/SessionConflict）一致。
