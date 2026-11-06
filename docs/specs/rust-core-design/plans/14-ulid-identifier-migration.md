# 计划 · 实体标识符迁移到 ULID（UUID 全部替换）

> 状态：待执行 · 2026-11-02
> 设计：[../15-identifier-strategy.md](../15-identifier-strategy.md)（终态 ID-1..ID-6）· 裁决：[../00-overview.md §7 ADR-003](../00-overview.md)
> 本文只持有执行计划与阶段状态，不携带需求/设计内容。

## 0. 范围与硬约束

**范围**：`rust/crates` 下所有 `uuid` 生成点与解析点，含内联测试与集成测试；`uuid` 从工作区依赖表移除。

**硬约束（每批完成时自查，逐条附命令）**

| # | 约束 | 检查命令 |
|---|---|---|
| C1 | 无 `uuid` 类型使用残留 | `grep -rn "use uuid\|uuid::Uuid\|Uuid::new_v4\|Uuid::parse_str\|Uuid::nil" rust/crates` 无命中 |
| C2 | 工作区无 `uuid` 生产依赖 | `cargo tree --edges all -i uuid`（在 `rust/` 下）为空 |
| C3 | 双宿主解析同源 | `grep -rn "parse_raw\|parse_with_prefix" rust/crates/mirri-server rust/crates/mirri-embedded` 覆盖全部 id 解析点（数量见 M3-3） |
| C4 | 旧格式可读 | M3 的旧格式用例全绿 |
| C5 | wire 形状不变 | 标识符仍是字符串；**不得**改动协议里名为 `uuid` 的既有 JSON 字段（那是回合步骤的字段名，与本次无关） |

## 1. 批次

### M0 · `mirri-id` crate 建立（TDD）

| 步骤 | 内容 |
|---|---|
| M0-1 | 新建 `rust/crates/mirri-id`：依赖 `ulid = "3"`（3.x 的 feature 集只有 default/std/postgres/rkyv/testing，默认特性即满足需求、无 `uuid` feature）与 `serde`；`Id` 的 serde 手写，不借 `Ulid` 的 impl。登记到 `rust/Cargo.toml` 的 `members` 与 `[workspace.dependencies]`；同步更新 `pnpm-workspace.yaml` 无关（rust crate 不进 pnpm workspace） |
| M0-2 | 先写失败测试：`Id::new()` 产 26 字符 ULID；`parse_raw` 接受 ULID / 36 字符 UUID / 32 字符 UUID / 小写 ULID；同一 UUID 两种拼写相等；**全零的 26 字符与 36/32 字符拼写均等于 `Id::NIL`**；整数大小写 UUID 归一为小写；非法输入（长度不符、非法字符、带花括号、`urn:uuid:` 前缀）报错 |
| M0-3 | 实现 `Id`：`u128` + 形态类别（`Ulid`/`Uuid`/`Nil`）；`PartialEq`/`Eq`/`Hash`/`Ord` 只按载荷；`Display`；`Serialize` 输出规范形态；`Deserialize` 接受全部可读形态；`Id::NIL`、`is_nil()`、`shape()` |
| M0-4 | 测试：不变量 I1——两规范形态（26 大写 ULID、36 小写连字符 UUID）读入写出逐字相等；非规范形态（32 无连字符、小写 ULID、大写 UUID）规范化后**两遍往返稳定** |
| M0-5 | 实现 `parse_with_prefix`（只剥离 3.1 表的已知前缀，含 `session_` / `term_` / `f_` 三种下划线拼写；不盲拆 `-`）与 `prefixed(prefix)` |
| M0-6 | 前缀常量集中定义，作为其他 crate 的唯一来源；与设计 3.1 表逐项对齐（13 类，`step-` 不在其中） |

**编译绿点**：`cargo test -p mirri-id` 全绿；`cargo clippy -p mirri-id -- -D warnings` 干净。

### M1 · 逐 crate 迁移（源码 + 该 crate 的测试同批，每步可独立验证）

规则：每个 crate 的**源码改动与其测试改动同批完成**，该 crate 的 `cargo test -p <crate>` 全绿后才进入下一个。`mirri-core` 内部 M1-2 至 M1-6 是一个原子编译单元（门禁类型与事件字段互相依赖），绿点只在 M1-6 后。

| 步骤 | 位置 | 内容 |
|---|---|---|
| M1-1 | `mirri-collaboration` | `CollaborationSession.id`、`CollaborationManager` 的 `HashMap` 键、`CollaborationError::SessionNotFound`、`start()` 返回值改 `Id`。**前置核查**：`session.rs:99` 生成的成员 id 实际格式是 `<role>-<32 位无连字符>`，无 `agent-` 前缀；读侧 `hooks.rs:288` 与 `rpc.rs:2230` 只剥 `agent-`。迁移时必须同时决定该格式的读侧口径（见 D3） |
| M1-2 | `mirri-core` 门禁 | `ApprovalGate` / `QuestionGate` 的 request_id、agent_id、`pending` 映射键、`ResolvedRing` → `Id`；`Uuid::nil()` → `Id::NIL`；`is_nil()` 语义保留 |
| M1-3 | `mirri-core` 事件 | `SessionEvent` 的 **13 个** `Id` 字段（`events.rs:60,106,110,115,116,122,129,131,136,152,158,163,168`）；`uuid_from_id` 改为 `parse_with_prefix` 语义并改名，**同步核对全部调用方**的 nil 兜底行为（`manager/mod.rs:1131,1132,1714`、`embedded/session_handlers.rs:1669`） |
| M1-4 | `mirri-core` 交互与归属 | `InteractionKernel::respond` / `is_recently_resolved` 走 `parse_raw`；`member_agent_uuid` → 走 `parse_with_prefix`；`:1510` 的 `unwrap_or_else(nil)` 同步改 `Id::NIL` |
| M1-5 | `mirri-core` 任务 | `resolve_id` 裸 id 修复走 `parse_raw`；保留“非标识符形态不修复”的既有拒绝语义（`task.rs:2432` 的用例） |
| M1-6 | `mirri-core` 生成点 | `session-`（`session/manager/mod.rs:637,657,673,1084`）、`agent-`（`session/agent_lifecycle.rs:68,111`、`subagent/host.rs:637`）、`task-`（`task.rs:959`、`shell_command.rs:265`）、`turn-`（`subagent/host.rs:1117,1822`）、`run-`（`cron/workspace_store.rs:2246,2439`）、`cron-`（`cron.rs:2110`）、`goal-`（`tools/create_goal.rs:145`）、`plan-`（`tools/enter_plan_mode.rs:110`、`tools/exit_plan_mode.rs:276`）、`act-`（`tools/skill.rs:192`）、`term_`（`terminal_pool.rs:358`）、`step_id`（`loop_engine/turn_step.rs:292`，非实体前缀，保留原语义只换生成器）、临时文件名（`image_pipeline.rs:326`、`tool_result_truncation.rs:108`）；`tools/ask_user.rs` 的 4 处强类型（`:389` `deliver_question_with_reconnect` 参数、`:650`/`:911` `QuestionSnapshot.agent_id`、`:1029` `question_ask_payload` 参数）；协作工具解析点（`tools/collaboration.rs:29-34`）改 `parse_raw`，6 处 schema 的 `format: "uuid"` 按 5.3 去掉 |
| M1-7 | `mirri-persistence` / `mirri-plugin-host` / `mirri-remote` | `f_` 文件索引 id（`file_store.rs:117`）、插件临时目录（`manager.rs:548,1741`）、client id（`remote/lib.rs:311`） |
| M1-8 | `mirri-embedded` | 三处 `request_id` 解析（`interaction_handlers.rs:99,205,249`）、全部 `format!` 生成点（`message_handlers.rs:842,1614,1874,2713`、`session_handlers.rs:5222,5321,6124`、`goal_driver.rs:784`、`task_driver.rs:756`、`cron_driver.rs:1300`、`cron_run_spawner.rs:203`、`scheduled_task_create_handlers.rs:95` 与紧邻的 `:100`（`ApprovalSnapshot.agent_id` 的 nil））、`epoch`（`mod.rs:1750`）、`requestId` 兜底（`model_handlers.rs:365`）、`__test_` 临时名（`mcp_config_handlers.rs:319`） |
| M1-9 | `mirri-server` | 三处审批/问答 `request_id` 解析（`rpc.rs:957,1050,1129`）、agent 归属映射（`rpc.rs:2229-2235`）、归属判定（`ws_rpc.rs:1194`）、`error_id`（`rpc.rs:104`，`.simple()` 拼写消失）、`server_id`（`state.rs:168`）、ping nonce（`ws_rpc.rs:2094`）、request id 兜底（`http.rs:402`） |
| M1-10 | `mirri-proto` / `mirri-test-harness` | 删除 `mirri-proto` 的幽灵 `uuid` 依赖（**不新增** `mirri-id` 依赖：协议层不消费 `Id`）；harness 生成点替换（`http_client.rs`、`tests/harness_tests.rs`） |

**顺序注记**：`mirri-server` 的 dev-dependencies 含 `mirri-embedded`，因此 server 的测试必须等 embedded 迁移完成后才能编译通过——M1-8（embedded）先于 M1-9（server）。`mirri-test-harness` 在 M1-9 前会被传递编译，其现有 uuid 用法均为自包含字符串（不引用 core 被改类型），可编译；在 M1-9 验收时显式确认。

**每步验收**：`cargo test -p <crate>` 全绿 + `cargo clippy -p <crate> --all-targets -- -D warnings` 干净。

### M2 · 收口与跨 crate 行为用例

| 步骤 | 内容 |
|---|---|
| M2-1 | 移除 `rust/Cargo.toml` 的 `uuid` 与各 crate 的 `uuid = { workspace = true }`；`cargo machete` 无冗余依赖 |
| M2-2 | 新会话 id 是 ULID 形态、新任务 id 是 `task-<ULID>`（行为断言） |
| M2-3 | 旧格式反序列化：`SessionEvent` 携带 UUID 字符串可读回；规范形态读入→写出逐字相等；非规范形态两遍往返稳定 |
| M2-4 | 归属链路：`agent-<uuid>` 判定为成员而非 main；全零两种拼写均判为 main；`agent-<ulid>` 同样判定为成员 |
| M2-5 | 双宿主对称：嵌入服务端与独立服务端对同一批 request_id（ULID 与 UUID 各一）都能 resolve / answer |
| M2-6 | 任务裸 id 修复对 ULID 与 UUID 两种拼写都生效 |
| M2-7 | 协作工具接受两种形态的 session_id；schema 不再声明 `format: uuid` |

**验收**：`cargo test --workspace` 全绿（后台执行，≥20 分钟超时）+ `cargo clippy --workspace --all-targets -- -D warnings`。

### M3 · 文档与发布

| 步骤 | 内容 |
|---|---|
| M3-1 | 修正源码注释里“uuid”语义的表述（**排除**协议字段名 `uuid`，见 C5） |
| M3-2 | 核对 `05-crate-structure.md` / `context.md` 与代码一致（`context.md` §11 标注为已迁移后的快照，或留作历史快照并注明） |
| M3-3 | changeset（级别见 D1） |

## 2. 开放决策

| # | 议题 | 待定内容 | 建议 |
|---|---|---|---|
| D1 | changeset 级别 | `patch`（纯内部格式）/ `minor`（新会话 id 可排序，可感知）/ `major`（旧前端若严格校验 UUID 则读不了新会话） | `minor`。`major` 需用户显式同意，不自行写 |
| D2 | 工具 schema 的 `format` | 去掉 / 改成描述双形态的说明 | 去掉 `format`，在描述里写“原样使用，不要改写”（设计 5.3 已定为终态） |
| D3 | 协作成员 id 的读侧口径 | 协作 crate 生成的是 `<role>-<32 位无连字符>`（**无** `agent-` 前缀），而 core/server 侧归属解析只剥 `agent-` 前缀——两处口径本就不一致 | 迁移后统一生成 `agent-<ULID>`；读侧 `parse_with_prefix` 额外容忍历史 `<role>-<32 位>` 形态，并在 M1-1 用用例钉住 |
| D4 | `mirri-id` 与 pnpm/flake | 新增 rust crate 是否要动 `flake.nix` | 不动：`flake.nix` 的 `workspacePaths` 覆盖的是 pnpm 工作区，rust crate 由 `rust/Cargo.lock` 参与构建；需在 M0-1 复核 `flake.nix` 是否引用 rust crate 清单 |

## 3. 风险登记

| 风险 | 出现阶段 | 缓解 |
|---|---|---|
| 某一侧服务端解析点遗漏 → 审批/问答静默失效 | M1-8 / M1-9 | C1/C3 门禁 + M2-5 双宿主对称用例 |
| agent 归属静默降级为 main | M1-8 | M2-4 归属链路用例 |
| 非规范形态导致旧日志文本变化 | M0-4 / M2-3 | 不变量 I1 的两遍往返用例 |
| 哨兵历史拼写不等价 | M0-2 / M2-4 | 全零归一用例（26/32/36 三种拼写） |
| `mirri-core` 内部 M1-2..M1-6 中途编译红 | M1 | 按原子单元推进，绿点在 M1-6；中间不跑全量测试 |
| 误改协议字段名 `uuid` | M3-1 | C5 明确排除 |
| 跨 crate 类型替换波及测试导致长红 | M1 | 测试随源码同 crate 同批迁移，每 crate 一个绿点 |
