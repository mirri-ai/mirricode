# 模型裁定机制（spec 11）落地实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> 上游 spec: [../11-model-resolution.md](../11-model-resolution.md)（独立评审 APPROVE-WITH-FOLLOWUPS，F-1/F-2 已修，SDR-001 / ADR-001~006）
> 侦察依据：两份实施面侦察报告（2026-09-14，锚点均已实读核实，见各任务"背景"）

**Goal:** 按 spec 11 落地 rust 内核模型裁定机制——消灭内建具体模型（缺陷 A）、修复子代理三级继承（缺陷 B/C）、统一切换入口语义（持久化/生效/恢复/校验），并以 e2e 实证缺陷全消。

**Architecture:** 状态在会话级唯一持有（主 agent 的 AgentScopeConfig 为代理持有者，模型经 `apply_model_switch` 单一路径切换），裁定在请求级逐次发生（主 agent 走 `ConfigResolvingLlm` 每步重读；子代理在派发时点三级裁定并钉扎）。切换记录复用既有 `WireOp::ConfigUpdate { model_alias }` + `ContextAppendMessage { origin: model_switched }` marker 对（状态 fold + 转录投影），不新增 wire op。

**Tech Stack:** rust workspace（mirri-core / mirri-embedded / mirri-proto / mirri-test-harness）、既有测试基建（模块内联 tests + `mirri-embedded/src/embedded_server/tests.rs` 进程内 e2e + `ScriptedLlm`/`RecordingLlmFactory`）、web（Vue 3 vitest）、TUI（node-sdk 翻译层 + ink 组件测试）。

## Global Constraints

- 验收以 spec 11 §1.4 的 A1-A17 为准；每个任务标注覆盖哪些 A 项。
- 每批完成跑门禁：`cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets -- -D warnings` 零新增告警（预存豁免：terminal_pool_provider_tests 2 个 await_holding_lock）。
- 测试命名 Given-When-Then（`should_...when_...` 风格，域内既有惯例）。
- mutation 验证：每个 P0/P1 任务至少 1 条反向 mutation（恢复缺陷形态确认测试变红），数量与结论记入本文件任务末尾。
- 不改 TS v2 引擎（packages/agent-core-v2 / kap-server 零改动）；web/TUI 只做消费端。
- 侦察发现的测试钉死旧行为位置（见各任务）允许更新断言，但**不得删除测试场景本身**——改为断言新语义。
- 全部内建具体模型名（`claude-sonnet-4-20250514` / `claude-haiku-4-20250514`）从生产路径清零后，测试 fixture/seed 显式声明模型名是合法的（不算内建）。
- 实施中若发现需偏离 spec 语义：停下记录，以 ADR 修订回写 spec，不得静默偏离（goal 边界）。

## 优先级映射（spec → 任务）

| 优先级 | 验收项 | 任务 |
|---|---|---|
| P0（缺陷 A/B/C 根因） | A3-A8、A14 | T1、T2、T3 |
| P1（裁定链完整性） | A1、A6、A9-A12、A15-A17 | T4、T5、T6、T7 |
| P2（可观测性） | A2、A13 | T8、T9 |
| 终验 | ⑤（goal 完成判据） | T10 |

---

### Task 1: 内建具体模型清零——fail-loud 替代静默兜底

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/config.rs`（:42 `default_model` 缺省值、:387 `AgentScopeConfig::default().model_id`）
- Modify: `rust/crates/mirri-core/src/profile.rs`（:2580-2621 `builtin_profiles()` 四个内建 profile 的 `model` 字段、:1249-1254 `scope_config_from_profile` 的非空校验）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（:267-296 会话创建解析链）
- Modify: `rust/crates/mirri-core/src/session/manager/restore.rs`（:601-608 resume 兜底）
- Test: 上述各文件内联 tests + `rust/crates/mirri-core/tests/scope_tests.rs`、`tests/session_resume_tests.rs`、`rust/crates/mirri-embedded/src/embedded_server/tests.rs`

**Interfaces:**
- Produces: `AgentScopeConfig::default().model_id == ""`；`builtin_profiles()` 的四个条目 `model` 为空；`scope_config_from_profile` 接受无模型 profile（`model_id` 透传为空，报错推迟到请求级）；会话创建链 = 显式 `modelId` → config `default_model` → **报错**（附"未配置默认模型"指引）；resume 无记录兜底 = config `default_model` → 报错。T2/T4/T5 依赖"空 model_id 合法存在"这一约定。

**背景（侦察依据）**：现状六处硬编码落点（config.rs:42 + :387 + 四个内建 profile）；创建链在 `session_handlers.rs:283-286` 落到 `AgentScopeConfig::default().model_id`；resume 在 `restore.rs:606` `unwrap_or_else(|| base.model_id.clone())` 落同一硬编码。注意 `config/tests.rs` 大量用 `claude-sonnet-4-20250514` 作**显式 seed**——合法，只改"缺省值"，不改 seed 解析。

- [ ] **Step 1.1（红）**：在 `scope/config.rs` 内联 tests 加：`should_return_empty_model_id_for_default_scope_config`（断言 `AgentScopeConfig::default().model_id.is_empty()` 与 `ServerConfig` 缺省 `default_model` 为空）；在 `profile.rs` 内联 tests 加：`should_accept_profile_without_model`（无 model 的 profile 经 `scope_config_from_profile` 得到空 `model_id`，不报错）与 `should_have_no_model_on_builtin_profiles`（四个内建 profile 的 model 均为空）。跑 `cargo test -p mirri-core scope::config profile::tests -- --ignored=false`（按模块过滤），预期三条新测试红。
- [ ] **Step 1.2（红）**：`session_handlers` 集成测试（embedded tests.rs）加：`should_reject_session_creation_with_actionable_error_when_no_model_resolvable`（无 `modelId` 参数 + config 无 `default_model` → `session.create` 报错，错误文案含"未配置默认模型"类指引）；resume 侧 `tests/session_resume_tests.rs` 加：`should_fail_resume_with_actionable_error_when_no_model_record_and_no_default`。
- [ ] **Step 1.3（绿）**：实施——`config.rs:42` `default_model: String::new()`；`config.rs:387` `model_id: String::new()`；`profile.rs` 四个内建 profile 的 `model: "claude-…".to_string()` 全部改为空串（或字段改 `Option` 后置 None——**取空串**，与 `model_id: String` 既有形态一致，避免连锁类型改动）；`profile.rs:1249-1254` 删除 `profile.model.is_empty()` 报错分支（空模型透传）；`session_handlers.rs:267-296` 解析链第三级从 `AgentScopeConfig::default().model_id` 改为返回 `RequestInvalid`（错误含指引，见 spec R4）；`restore.rs:606` 同理改为传入的 config 快照 `default_model`，再无则报错。
- [ ] **Step 1.4（修旧行为测试）**：全 workspace 编译后收集红测试——凡因"缺省模型从硬编码变空"而红的既有测试，显式补 seed（config TOML 里写 `default_model` 或 create 传 `modelId`），**不得**把断言改回硬编码值。`read_wire_ops`（tests.rs:2998-3029）与 turn op 顺序测试（:2940-2972）不受影响（create-time config.update 仍存在——create 显式给模型时照落）。
- [ ] **Step 1.5（绿验证 + A7 半边）**：`cargo test -p mirri-core -p mirri-embedded` 全绿；`rg -n 'claude-(sonnet|haiku)-4' rust/crates/mirri-core/src rust/crates/mirri-embedded/src --glob '!**/tests.rs'`（生产路径）零命中（tests.rs 与 `#[cfg(test)]` 内的命中属 seed，合法——A7 断言对象是生产路径，见 spec A7 扫描定义）。
- [ ] **Step 1.6（mutation）**：反向恢复 `config.rs:387` 硬编码 → 1.1 的 `should_return_empty_model_id...` 变红；恢复 `profile.rs:1249` 校验 → `should_accept_profile_without_model` 变红。记录 2/2 CAUGHT。
- [ ] **Step 1.7（门禁 + 提交点）**：`cargo test --workspace` + clippy 门禁过；commit（`feat(core): remove builtin model defaults, fail loud when unresolvable`）。

**验收映射**：A6（创建/恢复报错半边）、A7（源码扫描半边——运行时装配半边在 T2/T5）。

---

### Task 2: 子代理三级裁定真实可达——Tier 3 用活父配置

**Files:**
- Modify: `rust/crates/mirri-core/src/subagent/host.rs`（:180-234 `resolve_subagent_binding`、:285-294 host 构造的 `parent_config` 快照、:482 config.model_id 赋值、:633-662 spawn future 既有 live 读先例）
- Modify: `rust/crates/mirri-core/src/session/manager/restore.rs`（:837 `restore_subagents` 的 `unwrap_or_default()` 空兜底）
- Test: `host.rs` 内联 tests、`rust/crates/mirri-core/tests/subagent_integration_tests.rs`

**Interfaces:**
- Consumes: T1 的"空 model 合法"约定。
- Produces: `resolve_subagent_binding` 三级链 = 显式（catalog 校验）→ profile default（用户配置类型）→ **spawn 时点的 live 父模型**（不再用 host 构造快照）→ 三无时报 `unknown_model_alias_error` 带 curated 清单。T3 的描述测试依赖此裁定结果形态。

**背景**：缺陷 B/C 的代码根源——`parent_config` 是 session.create 时 clone（host.rs:289，`session_handlers.rs:327` 注入），主 agent 之后 setModel 不再传导；Tier 3 拿到陈旧模型。spawn future 内已有从 live parent 读 reverse_rpc/permission 的先例（host.rs:633-662），照搬该模式读 `parent_config.model_id`。restore 侧 `registration.model` 为 None 时 `unwrap_or_default()`= 空串（restore.rs:837）——改为走"父会话当前模型"，再无则按 T1 报错。

- [ ] **Step 2.1（红）**：`host.rs` 内联 tests 加三条（用 `RecordingLlmFactory` 逐请求记录 model 的既有 seam，tests.rs:277 同款）：
  - `should_inherit_live_parent_model_when_spawned_without_model`（父 agent 构造后 `set_model("model-b")` 再 spawn 不带 model、profile 无默认 → child 的 create_llm 收到 `model-b`）——**这是缺陷 B 的直接断言**；
  - `should_use_profile_default_model_over_parent_model`（profile 带 `default_model` → 用之，父模型不覆盖）——**缺陷 C 的直接断言**；
  - `should_error_with_available_models_when_all_three_tiers_empty`（显式无 + profile 无默认 + 父 model_id 为空 → spawn 报错，错误含可用模型清单）。
- [ ] **Step 2.2（绿）**：实施——`resolve_subagent_binding` 增加从 live parent scope 读 `model_id` 的入口（spawn future 内取 `parent_agent.config()` 快照读值，或把 `parent_config: Arc<RwLock<AgentScopeConfig>>` 从克隆改为共享句柄——**取共享句柄方案**，与 `ConfigResolvingLlm` 同模式，Tier 3 读锁内取值）；三无时报错（复用 `unknown_model_alias_error`，host.rs:241-263 既有）。`restore_subagents` 的 `unwrap_or_default()` 改为解析父会话当前模型（主 log fold 值），解析不出则报错（A6 派发半边）。
- [ ] **Step 2.3（集成验证）**：`subagent_integration_tests.rs` 加端到端三条对应 2.1 语义（真实 SessionScope + host）；既有 `subagent/mod.rs:1048` 的 `task.model()` 断言（显式覆盖原值展示）不变。
- [ ] **Step 2.4（mutation）**：把 Tier 3 改回 host 构造快照 → `should_inherit_live_parent_model...` 变红。1/1 CAUGHT。
- [ ] **Step 2.5（门禁 + 提交点）**：workspace 门禁过；commit（`feat(subagent): resolve model from live parent config, three-tier chain reachable`）。

**验收映射**：A3、A5（类型默认）、A6（派发半边）、A14（钉扎既有行为回归确认——resume 沿用 child config，host.rs:1266 既有）。

---

### Task 3: 派发工具描述去硬编码（B-3 方案甲：动态清单保留）

**Files:**
- Modify: `rust/crates/mirri-core/src/tools/agent_tool.rs`（:82-110 描述拼装）
- Modify: `rust/crates/mirri-core/src/tools/agent_swarm_tool.rs`（:239-243 同构）
- Test: 两文件内联 tests（agent_tool.rs:603 起、agent_swarm_tool.rs 内联）

**Interfaces:**
- Consumes: T1 后内建 profile 无模型（`p.model` 为空，描述行不再拼 `(model)`）。
- Produces: 描述 = profiles 行（仅 `name — description`）+ `Available models:` curated 块（ModelCatalog 同源，既有）。描述中不存在任何硬编码模型名。

**背景**：现状 `agent_tool.rs:84-88` 把 `p.model` 拼进 profile 行（T1 后自然变空，需去掉空括号拼接）；curated 块（:100-107）本来就来自 catalog（stub 测试在 :1225/:880 注入）——它是 spec R7 保留的"与配置同源动态生成"段，**不要删**。既有测试 `agent_tool.rs:1290` / `agent_swarm_tool.rs:917` 断言 `desc.contains("claude-sonnet-4")`——它们断言的是 stub curated 内容，属合法 seed，但断言语义要升级为"描述中的模型名只能来自 catalog 注入"。

- [ ] **Step 3.1（红）**：加测试 `should_not_render_model_suffix_when_profile_has_no_model`（无模型 profile 的描述行不含 `()` 或模型名）；`should_render_only_catalog_sourced_models_in_description`（注入两个 curated 条目 `alpha`/`beta` → 描述恰含这两个、不含其他具体名）。
- [ ] **Step 3.2（绿）**：描述拼装去掉 `p.model` 参与的 ` (model)` 段（`model` 为空的类型不再渲染模型后缀；用户配置类型若显式带模型也**不再拼进描述**——裁定规则改为一句固定文案"默认沿用派发者当前模型，可用模型见下"）；curated 块维持现状。
- [ ] **Step 3.3（旧行为测试升级）**：`agent_tool.rs:1290` / `agent_swarm_tool.rs:917` 改为断言 stub 注入的中性名（如 `test-model-a`），并各加一条"描述不含 claude-sonnet/claude-haiku 字面量"守卫（防回归 A8）。
- [ ] **Step 3.4（mutation + 提交点）**：描述拼装回拼 `p.model` → 3.1 第一条变红。门禁过；commit（`feat(tools): drop hardcoded model names from agent dispatch descriptions`）。

**验收映射**：A8。

---

### Task 4: message.send 模型参数并入单一切换路径（A9/A6 闭合）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（:2489-2516 `apply_model_switch` 提取为共享函数，含 :2522-2578 `record_model_switch`）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（:254-315 send 的 model 参数应用点、:306-312 陈旧注释、:437-448 fail-fast resolve）
- Test: `embedded_server/tests.rs`（既有 :26216/:26257/:26313 三个 setModel e2e 为参照系）

**Interfaces:**
- Consumes: T1（校验链）；`apply_model_switch` 既有四件事（幂等去重 → set_model → `record_session_config_op` → `record_model_switch` marker+事件）。
- Produces: 共享切换函数 `switch_session_model(session, model_alias) -> Result`（命名实施时定，语义 = 上述四件事）；send 的 model 参数走它。后续 T5/T6 复用。

**背景**：现状 send 参数只 `agent.set_model(model)` 内存 sticky、不落 wire（message_handlers.rs:306-315 注释自认 resume 丢失——TS parity 破坏点）。spec ADR-002：一次参数携带 = 一次标准切换。send 时点的 fail-fast resolve（:437-448）保留，但校验提前到切换函数（同口径 R10）。

- [ ] **Step 4.1（红）**：tests.rs 加：`should_persist_send_model_param_to_wire_log_and_emit_marker_and_event`（`message.send {model: "model-b"}` → wire ops 含 `ConfigUpdate { model_alias: "model-b" }` + `ContextAppendMessage { origin.kind: "model_switched" }`；订阅收到 `event.session.model_switched`）与 `should_resume_session_with_model_from_send_param`（send 带 model → turn 完成 → drop session → resume → `agent.status`/下一步 create_llm 收到 model-b）。**A9+A10 的 send 半边**。
- [ ] **Step 4.2（绿）**：提取 `apply_model_switch` 为模块级共享函数（去 agent_id 泛化：send 路径恒主 agent）；`message_handlers.rs:313-315` 改调它；删除"resume 回放会话自身默认"的陈旧注释。
- [ ] **Step 4.3（回归确认）**：既有三测试（:26216 marker+事件、:26257 no-op 不落日志、:26313 wire 持久化）全绿；`tests.rs:6505` 附近钉 setModel 旧行为的测试若红，按新语义更新（场景不删）。
- [ ] **Step 4.4（mutation）**：send 路径回退为裸 `set_model` → 4.1 两测试变红。2/2 CAUGHT。
- [ ] **Step 4.5（门禁 + 提交点）**：commit（`feat(send): route model param through shared switch path with persistence`）。

**验收映射**：A9（含"本次请求即新模型"——resolver 每步重读既有，B7/B8 侦察已证 mid-turn 生效机制存在，4.1 的 resume 断言同时钉住）、A6（发消息报错半边——三无从模型时 send 报错）。

---

### Task 5: 请求级解析与校验口径统一（A1/A11/A15，ADR-004/005）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/providers/config_factory.rs`（:259-296 `ConfigAwareLlmFactory::create_llm` 的 env 裸名回退、`resolve_model_alias_in`）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（:2473-2480 `model_exists`）
- Test: `config_factory.rs` 内联 tests、embedded tests.rs、`rust/crates/mirri-test-harness/tests/cross_backend_contract.rs`（复合 id 契约）

**Interfaces:**
- Produces: 解析口径全入口统一 = 已配置别名 key（精确）或 `provider/model` 规范复合 id（M1 两形态）→ 否则 `model.not_found` 错误（附可用模型清单）；**废除裸名 env 回退**。请求级漂移（A15）：`create_llm` 解析不到（含别名已被删）→ 同一错误，不回退 default_model。**T1 评审 M-1 一并处理**：创建链对空串 `modelId`（`Some("")`）做 trim+过滤，与 restore 的空串过滤对称。

**背景**：侦察 B7/B10 + 风险 10——`create_llm` 无 `/` 裸名走 `ProviderLlmFactory` env 链（旁路）；`model_exists` 只认精确 key + `default_model` 等值（复合 id 在 setModel 不被接受，但 create/create_llm 容忍复合 id，:26183 测试可证）。**注意**：env 链是 Kimi 等 provider 的构造通道——废除的是"裸模型名静默走 env"这一**解析路径**，配置内 provider 的 env 凭据读取不动（那是 provider 解析层，spec 非目标明确两层正交）。

- [ ] **Step 5.1（红）**：`config_factory` 内联 tests 加：`should_reject_unknown_bare_model_name_with_available_list`（无 `/`、不在 `[models]` → 错误含已配置模型清单，**不再**构造 env LLM）；`should_accept_canonical_composite_id_resolving_to_alias`（复合 id 解析到 alias 走配置）。embedded tests.rs 加：`should_reject_set_model_with_unknown_alias_consistently_across_entries`（setModel / send / spawn 三入口传同一未知名 → 同一错误码与文案结构——A11）与 `should_error_on_next_request_when_current_model_alias_deleted`（切换到 model-b → 从 config 删除 model-b → 下一次 send 的 LLM 解析报错列清单，**不回退 default**——A15）。
- [ ] **Step 5.2（绿）**：`create_llm` 删裸名 env 分支（未命中 alias/复合即报错）；`model_exists` 扩为"可解析"口径（复用 `resolve_model_alias_in`，接受两形态）。
- [ ] **Step 5.3（旧行为清理）**：收集依赖 env 裸名的既有测试（`config/tests.rs:1165/:2271/:2527` 的 env 组合类）——**逐条甄别**：凡断言"env 变量配置出的模型可解析"的场景改为显式 `[models]` seed 后保留；env 凭据注入类（provider 构造）不动。cross_backend_contract 的复合 id 契约行确认仍绿。
- [ ] **Step 5.4（mutation + 门禁 + 提交点）**：恢复 env 裸名分支 → 5.1 第一条变红。门禁过；commit（`feat(resolution): unified alias validation, no env bare-name fallback, drift fails loud`）。

**验收映射**：A1（既有 resolver 每步重读，加一条 mid-turn 步级断言 `should_apply_mid_turn_model_switch_to_next_step`，用 `RecordingLlmFactory` 记录两次 create_llm 的 model）、A11、A15。capability-gap 注入 turn 级 sticky（run_turn.rs:62-63）为已知例外，记入任务备注（spec 未要求重算）。

---

### Task 6: 恢复一致与多 agent 归属（A10/A17，ADR-006）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（:2421-2466 `dispatch_set_model` 的 agent_id 接受面）
- Modify: `rust/crates/mirri-core/src/session/manager/restore.rs`（:601-608 兜底——T1 已改，此处验证 fold 语义）
- Test: `tests/session_resume_tests.rs`、embedded tests.rs

**Interfaces:**
- Consumes: T4（send 切换已落 wire → fold 已闭合；`switch_session_model(server, session_id, agent, model)` 模块级共享函数——agent 解析在调用方）。
- Produces: `agent.setModel` 对**子代理目标**返回拒绝（错误说明"模型切换是会话级语义，子代理模型在派发时点钉扎"）；wire `ConfigUpdate` 维持无 agent 归属字段（旧 log 兼容：无归属 = 主 agent，现状语义不变）；**mirri-server 的 `session.restore` base 种入 config snapshot 的 `default_model`**（T1 评审 I-1：该面现用裸默认 config，T1 后无记录会话恢复会跳过 config default 直接报错，R9 链不完整）；**send 带非主 agent 地址（agentId）+ model 参数的组合拒绝**（T4 评审 I-1：该组合现在切主 agent 状态但被地址 agent 的 turn 仍用旧模型 + marker 落主 agent context——"分割线宣布 model-b、回复仍来自 model-a"违背 R1，与 setModel 子代理目标同口径拒绝并附会话级/钉扎指引）。

**背景**：侦察风险 1/D17——`apply_model_switch` 接受任意 agentId 但 ConfigUpdate 无归属，子代理 setModel 会在 resume 时把模型 fold 到主 agent（错位）。spec ADR-006 裁决：切换是会话级、A14 钉扎在途子代理——对子代理的 setModel 应拒绝而非记录。

- [ ] **Step 6.1（红）**：tests.rs 加 `should_reject_set_model_on_subagent_with_pinning_explanation`（spawn 子代理 → `agent.setModel {agentId: child}` → 错误文案含"会话级/钉扎"指引）；`tests/session_resume_tests.rs` 加 `should_restore_multi_agent_session_to_last_switched_model`（主 agent 切换两次 → resume → 主 agent 模型 = 第二次；子代理恢复其 registration.model 钉扎值）——A17。
- [ ] **Step 6.2（绿）**：`dispatch_set_model` 校验目标 agent 为主 agent（`agent_id: None` 或主 id），否则 `RequestInvalid`。
- [ ] **Step 6.3（mutation + 门禁 + 提交点）**：去掉拒绝分支 → 6.1 第一条变红。门禁过；commit（`feat(session): session-scoped model switch, reject subagent targets`）。

**验收映射**：A10（resume 全量，与 4.1 呼应）、A16 的 wire 半边（marker 不属撤销单元——undo 既有实现只回退对话对，加一条 `should_keep_model_and_marker_after_undo` 断言）、A17。

---

### Task 7: 引擎内部轮次跟随（A12）

**Files:**
- Test: `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs` / `task_driver.rs` 内联或 embedded tests.rs（cron 通知类既有测试位置）

**Interfaces:**
- Consumes: 活 agent config 读取（cron_driver.rs:284-299 / task_driver.rs:331-341 既有）。
- Produces: 纯测试钉紧行为（实现预计零改动或极小）。

**背景**：侦察 D15——cron 与 task 通知已从目标 agent **live** config 取模型；子代理收件人用其 config（钉扎值）。A12 要求钉住"触发时会话当前模型 + 轮次内切换即时生效 + 子代理收件人钉扎"。

- [ ] **Step 7.1（红→绿）**：tests.rs 加：`should_run_cron_turn_with_session_current_model_at_trigger`（cron 触发前 set_model("model-b") → cron 轮次的 create_llm 收到 model-b）；`should_use_pinned_model_for_notification_turn_addressed_to_subagent`（向子代理投递通知 → 用子代理 config 钉扎模型，不用主 agent 新模型）。若第二条发现实现不满足（侦察未覆盖此细分），修 `task_driver.rs` 收件人锚定处的 config 取值。
- [ ] **Step 7.2（门禁 + 提交点）**：commit（`test(internal-turns): pin cron/notification model semantics`）。

**验收映射**：A12。

---

### Task 8: 用量逐请求归因（A13）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（`apply_model_switch` 追加 `set_usage_model_key` 联动或等价机制）
- Modify: `rust/crates/mirri-core/src/turn_flow/mod.rs`（:1284-1287 turn 开始捕获的 key 语义）
- Test: `rust/crates/mirri-core/src/usage.rs` 内联、embedded tests.rs

**Interfaces:**
- Produces: 内存 by_model 的 key 在 mid-turn 切换后指向新模型（与 wire `usage.record` 每步 `llm.model_name()` 口径对齐——统一用**别名**口径：resolver 解析出别名时同步更新 key；`llm.model_name()` 原始名与别名的映射经 catalog）。

**背景**：侦察风险 4/D16——双口径分裂：内存 key = turn 开始捕获（mid-turn 不更新），wire = 每步原始名。`RecordingLlmFactory` seam 可断言。**注意**：`llm.model_name()` 是原始模型名（M2），by_model 现用别名（M1）——归一策略：在 `usage.record` op 的写入处带上当步解析的别名（resolver 已知），内存 key 在切换时同步 `set_usage_model_key`（apply_model_switch 联动）。

- [ ] **Step 8.1（红）**：tests.rs 加 `should_attribute_post_switch_usage_to_new_model`（mid-turn 切换 → 该回合后续 step 的 usage 同时落旧/新模型桶，各自只含各自 step 的量；wire `usage.record` 的 model 字段与别名口径一致）。usage.rs 内联加 key 更新单测。
- [ ] **Step 8.2（绿）**：`apply_model_switch` 联动 `set_usage_model_key`；`turn_step.rs:489-502` 写 usage.record 时携带当步别名（resolver 暴露"本次解析的别名"——`ConfigResolvingLlm` 增只读访问或经 resolve 返回元组，形态实施时定，保持 trait 兼容）。
- [ ] **Step 8.3（mutation + 门禁 + 提交点）**：去掉切换联动 → 8.1 变红。commit（`feat(usage): per-request model attribution across mid-turn switches`）。

**验收映射**：A13。

---

### Task 9: 分割线呈现——TUI 补全 + web 对齐核验（A2）

**Files:**
- Modify: `packages/node-sdk/src/rust-engine/translate.ts`（:120-186 翻译表加 `event.session.model_switched` 臂）
- Modify: `apps/mirri-code/src/tui/types.ts`（:141-151 TranscriptEntryKind）、`tui/components/messages/`（新增 ModelSwitchMarkerComponent，仿 `swarm-markers.ts:8-22`）、`tui/controllers/session-event-handler.ts`（:242-282 加 case）、`tui/controllers/session-replay.ts`（:261-330 加 `model_switched` origin arm）
- Verify: `apps/mirri-web`（分割线已实现——只做语义 diff 核验，预计零改动）
- Test: TUI `apps/mirri-code/test/tui/`、web `apps/mirri-web/src/composables/messagesToTurns.test.ts`（既有 describe 扩断言）

**Interfaces:**
- Consumes: rust 侧 `event.session.model_switched {from, to}`（既有，web 已消费）与 wire marker 消息（恢复源）。
- Produces: TUI 分割线 live + 恢复（replay arm）；web 维持现状；显示名映射：事件携别名，TUI/web 端用 availableModels/modelDisplayName 映射，缺省回退别名原文（**不在引擎事件加显示名**——M4 不是标识符，映射属展示端职责）。

**背景**：侦察 A2 全链已通（web：`messagesToTurns.ts:591-599/:884-899`、ChatPane :651-664、i18n en/zh conversation.ts:17、恢复靠持久化 marker——全部已落地，git c1a87aa5/7d76470a）。**TUI 是唯一缺口**：翻译层静默丢弃 model_switched（translate.ts "unknown dropped"）、replay 无 origin arm。node-sdk 契约：`model_switched` 不在 klient 契约（rust-only 扩展事件，由 node-sdk fixtures 管理，`fixtures/rust-engine-events.ts:182/:605`）——若翻译层新增 v1 事件形态，同步 fixtures。

- [ ] **Step 9.1（红）**：TUI 组件测试：ModelSwitchMarkerComponent 渲染"模型已切换：A → B"行（i18n）；session-replay 测试：含 `origin.kind: "model_switched"` marker 的消息回放产出分割线条目（A2 恢复半边 TUI）；event-handler 测试：`sessionModelSwitched`（或翻译后事件）触发 addChild + requestRender。
- [ ] **Step 9.2（绿）**：translate.ts 加臂（映射为 v1 事件——形态：复用既有"扩展事件透传"模式或新增类型，与 translate 表 :585-595 meta.updated 先例一致）；TUI 三点接线；i18n key 对齐 web 文案。
- [ ] **Step 9.3（web 核验）**：跑 `pnpm -C apps/mirri-web test`——`messagesToTurns.test.ts:219-258` 既有 describe 全绿即确认零改动；若 rust 事件 payload 有字段变化（无），同步 mappers/fixtures。
- [ ] **Step 9.4（门禁 + 提交点）**：`pnpm -C apps/mirri-code test -- tui` + web vitest 过；commit（`feat(tui): model switch divider live and on replay`）。

**验收映射**：A2（web 既有 + TUI 新增；恢复半边经 4.1/6.1 的 wire 断言 + 9.1 replay 断言闭合）。

---

### Task 10: e2e 终验 + 规格回写（goal 完成判据 ⑤⑥）

**Files:**
- Test: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（三条终验 e2e，样板 :26216 同区）+ 新增源码扫描守卫测试
- Modify: `docs/specs/rust-core-design/ROADMAP.md`（模型裁定行 → 落地状态）、`context.md` §8（快照标注"已修复，见 ROADMAP"）、`11-model-resolution.md`（§2.5 记录族措辞按实现映射微调：`ConfigUpdate` + marker 对即"模型切换记录"的实现载体——以 ADR 修订标注记入，不改语义）

**Interfaces:**
- Consumes: T1-T9 全部。
- Produces: goal 完成判据 ⑤⑥ 的证据。

- [ ] **Step 10.1（e2e-a：缺陷 B/C 终验）**：tests.rs 加 `e2e_subagent_inherits_parent_custom_model_without_explicit_model`：`test_server_with_two_models()` 起服务（双别名 + RecordingLlmFactory）→ `session.create {modelId: "model-a"}` → 主回合内 spawn `subagent_type: "agent"` 不带 model → 断言 child create_llm 收到 `model-a`、turn 成功（对照取证 session-0b8117a4 的失败形态——无 ANTHROPIC 兜底、无静默内建）。
- [ ] **Step 10.2（e2e-b：缺陷 A 终验）**：源码扫描守卫测试（workspace 测试或 `rust/scripts/` 脚本 + CI 记账，形态实施时按仓库惯例定）：生产路径 `rg 'claude-sonnet-4|claude-haiku-4'` 于 `rust/crates/*/src`（排除 `#[cfg(test)]` 与 tests.rs、fixtures）零命中——A7 运行时装配半边由 T1/T2 的测试承担，本步钉源码半边。**显式排除清单（T1 评审 I-2 裁定）**：`image_pipeline.rs` 的 `VISION_MARKERS`（视觉能力判定的名字启发式，含 "claude-sonnet-4"/"claude-opus-4"/"gemini" 等——不参与模型裁定，非 R4 违规，登记为 A7 扫描排除项）；文档注释示例字面量（`config/mod.rs:11/:148`、`profile.rs:99/:222`、`anthropic.rs:57`、`model_catalog.rs:26`——doc 示例非装配路径，随本步或 T2 顺手换中性名，M-4 清单）。
- [ ] **Step 10.3（e2e-c：切换语义终验）**：`e2e_mid_turn_switch_persists_and_resumes`：wait 模式回合进行中（ScriptedLlm 两步脚本）`agent.setModel "model-b"` → 第二步 create_llm 收到 model-b（A1）→ 回合结束 resume → 首个 create_llm 仍 model-b、wire log 含 ConfigUpdate + marker（A9/A10）。
- [ ] **Step 10.4（验收矩阵核对）**：对照 spec A1-A17 逐条列出承载测试（本计划各任务 + 10.1-10.3），无遗漏后写入本文件末尾"验收矩阵覆盖表"。
- [ ] **Step 10.5（规格回写）**：ROADMAP 模型裁定行更新落地状态（批次、commit、mutation 总账）；context §8 标注修复；spec §2.5 措辞 ADR 修订标注。跑 `gen-changesets` skill 生成 changeset（CLI 行为变化：内建默认废除与 send 参数语义属用户可见变更——bump 按 skill 规则裁）。
- [ ] **Step 10.6（最终门禁）**：`cargo test --workspace`（记录总数，对照基线 4721+）+ clippy + `pnpm -C apps/mirri-web test` + `pnpm -C apps/mirri-code test`（TUI 相关）+ zero-io（若域内门禁适用）。
- [ ] **Step 10.5a（T10 前置：fixture 种子债清偿——T6 评审回写）**：T1 fail-loud 连带的 44 条预置红全部补种子——mirri-server `e2e_mock(3)` / `http(11)` / `ws(7)` / `ws_rpc(14)` + `mirri-test-harness harness_tests(9)`，各 fixture 按 cross_backend_contract 的 4cf03e13 形态补 `default_model`（**注意陷阱**：同时需移除或正确设置 `max_context_size`，否则 compaction 相关断言因窗口已知而翻红——见 T6 concern 1 的因果链）；完成后 workspace 全量门禁才可能全绿。

---

## 并行化裁定

- **串行**：T1 → T2 → T3（共享 profile/config 语义）；T4 → T5 → T6（共享切换路径与校验）；T7 依赖 T4 后可并行于 T5/T6；T8 依赖 T4/T5。
- **可并行**：T9（TUI/web 消费端，只依赖既有事件形态）可与 T4-T8 并行派发；T10 必须最后。
- 资源冲突提示：T1-T8 都动 `session_handlers.rs` / `message_handlers.rs`——**凡并行批，共享文件的修改必须同批一个实施者**；推荐 T4-T6 由同一实施者连续执行。

## 执行顺序总结

T1 → T2 → T3（P0）→ T4 → T5 → T6 → T7（P1，T7 可与 T5/T6 并行）→ T8（P1 收尾）∥ T9（P2，可提前并行）→ T10（终验 + 回写）。
