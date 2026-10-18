# Rust 内核域计划 — 09: LLM 会话标题生成（session-title-generation）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——本文件持**任务分解与验收标准**；开放项状态唯一源是 [../ROADMAP.md](../ROADMAP.md)。
> 上游 spec: [../09-session-title-generation.md](../09-session-title-generation.md)（SDR-014 / ADR-017，经 agent-29 两轮评审 APPROVE-WITH-FOLLOWUPS，followups 已于 2026-09-13 收口）

**Goal:** 未命名会话的用户轮次内注入命名指令 + 标题更新工具（仅主 agent），模型在既有响应流中调用工具写入概括性标题（零额外模型请求）；工具执行守卫原子防覆盖（用户手动先到先得）；首个用户轮次结束规则式兜底；命名后注入自熄。

> 代码级执行计划（bite-sized 任务、TDD 步骤、接口签名）：`docs/superpowers/plans/2026-09-13-llm-session-title.md`——本档持批次与验收语义，执行计划持实现细节。

**Architecture:** 全部落 rust 引擎——工具与注入逻辑在 mirri-core（注册与激活分离、Injector 文本面），宿主接线（时序迁移、工具启停、兜底站点）在 mirri-embedded 消息派发路径。契约链零新增 RPC、零新增事件（复用 `session.meta.updated` 与既有工具调用事件流）。TS 各端零改动。

**Tech Stack:** Rust workspace（mirri-core / mirri-embedded）；测试用既有 in-process harness 模式（mock LLM）；门禁同 07（cargo test --workspace + clippy -D warnings + zero-IO 守卫）。

## Global Constraints（继承 07，全文有效）

- 本地门禁：`cargo test --workspace` 全绿、`cargo clippy --workspace --all-targets -- -D warnings` 零告警、`node rust/scripts/check-zero-io.mjs` 0 命中。
- 测试命名 Given-When-Then；不改实现迁就旧测试（除非实现真有 bug）。
- changeset 走 gen-changesets skill；`major` bump 任何时刻需用户确认（硬门）。
- 契约链事实源核查：本计划不新增 RPC 方法面（registry 五环无涉）；新增内置工具走引擎内部工具面，不经 wire 方法注册表。
- 语义红线（来自 spec 评审，违反即返工）：
  1. **时序契约（A9）**：LLM 路径下消息派发时点不得派生标题——轮次开始前标题必须保持占位；
  2. **守卫原子性（A2/A3）**：工具写入与 `session.rename` 共用原子元数据更新闭包，先到先得、无部分写入；
  3. **兜底仅用户轮次**：cron/task 等引擎内部轮次不注入、不启用工具、不触发兜底；标题仅从用户消息文本派生；
  4. **脱敏单实现（A5）**：工具参数清洗与规则式兜底复用 `title.rs` 同一 sanitize，禁止两套规则；
  5. **注入自熄（A7）**：命名（手动或自动）后注入与工具启用消失；命名后首个用户轮次允许一条一次性工具移除提醒。

## 每批循环（SDD 协议，同 05/06/07）

brief（侦察 + 设计，`.superpowers/sdd/session-title-generation/task-<N>-brief.md`）→ TDD（红→绿）→ 评审（含 mutation）→ 本地门禁 → changeset → commit → ROADMAP/本档状态更新 → 批次报告。

## 停止规则（同 07）

用户裁决门 → 记队列跳过；mutation REJECT 两轮 → 暂停上报；环境阻断 → 记录替代验证；新架构问题 → 记 ROADMAP §5 候选不当场改。

## 任务总表

| 任务 | 主题 | 优先级 | 状态 |
|---|---|---|---|
| T1 | 标题更新工具（mirri-core） | P0 | ✅ |
| T2 | 轮次装配与命名指令注入（mirri-core） | P0 | ✅ |
| T3 | 宿主接线：时序迁移 + 工具启停（mirri-embedded） | P0 | ✅ |
| T4 | 兜底站点：首个用户轮次结束规则式（mirri-embedded） | P1 | ✅ |
| T5 | 验收矩阵与 mutation 收口 | P1 | ✅ |

> **范围声明**：ACP 端标题实时推送、fork 会话自动命名、历史会话标题重写、TS 侧任何改动——均不在本计划内（spec §1.3 非目标）。

---

## T1: 标题更新工具（mirri-core）

**优先级**：P0 ｜ **涉及面**：rust（mirri-core tools + BuiltinToolServices 扩展）

**背景**：标题更新工具是命名机制的写侧。按 ADR-017 决策 3/5/7：注册为内置工具（默认停用，激活由宿主控制，collaboration 工具先例 `tools/mod.rs` registered-but-disabled 模式）；执行守卫在原子元数据更新闭包内检查 `is_custom_title == false` 且标题为占位；参数清洗复用 `title.rs` 的 sanitize 与 ≤200 字符约束；免审批（元数据写入无破坏性，审批规则面按既有免审批工具先例接线）。工具名在 brief 阶段对齐 rust 既有工具命名风格落定。`BuiltinToolServices` 现无会话元数据句柄槽位，需扩展（评审 n3）。

**Goal**：主 agent 工具面可按状态激活一个标题更新工具，其执行满足守卫原子性、脱敏单实现与长度校验；成功写入后发布既有元数据更新事件；被守卫拒绝时静默返回。

**验收标准**：
- [ ] 工具执行：守卫通过 → 标题写入 + 既有元数据事件发布（A1 的事件侧）
- [ ] 用户已手动改名（`is_custom_title=true`）→ 工具调用静默跳过（A2）
- [ ] 已派生标题 → 工具调用静默跳过（A2/A7 的写侧）
- [ ] 参数含敏感内容 → 写入前脱敏（A5，与 `title.rs` 同一实现）
- [ ] 空标题 / >200 字符 → 视同未调用（返回提示，不写入）
- [ ] 工具调用记入 wire log（A8 审计侧）
- [ ] mutation 红线：删守卫检查即红；删 sanitize 即红

**风险**：`BuiltinToolServices` 扩展的依赖注入形态（元数据句柄的获取路径）——brief 侦察定。

---

## T2: 轮次装配与命名指令注入（mirri-core）

**优先级**：P0 ｜ **涉及面**：rust（mirri-core turn_flow / injection）

**背景**：按 ADR-017 决策 1/4：命名指令以系统提示片段注入用户轮次。注入决策在消息派发的异步上下文读取元数据后做出、轮次内静态生效（评审 M2 的同步缝规避——`Injector::inject` 是同步 trait，标题状态在异步锁后）。具体装配形态（turn 装配配置携带本轮注入标志，参照 `TurnFlowHooksConfig` 类先例）在 brief 侦察落定；注入器实现参照 `injection/goal.rs` 的去重模式但不采用其"最后用户消息后重注入"的触发式样（评审确认该模式会误触发）。

**Goal**：会话未命名时，用户轮次的模型请求中包含命名指令片段；命名后（含手动）注入消失；非用户轮次（cron/task）无注入。

**验收标准**：
- [ ] 未命名会话的用户轮次请求含命名指令（A1 前提）
- [ ] 命名后（手动/自动）的轮次请求无命名指令（A7）
- [ ] cron/task 轮次无注入（A11 关联红线 3）
- [ ] steer 追加消息不改变本轮注入决策（A11）
- [ ] mutation 红线：删注入决策即红

**风险**：装配信号穿越 turn_flow 的传递路径（派发时点 → 轮次装配）——brief 侦察定。

---

## T3: 宿主接线：时序迁移 + 工具启停（mirri-embedded）

**优先级**：P0 ｜ **涉及面**：rust（mirri-embedded message_handlers）

**背景**：按 ADR-017 决策 2/3 与时序契约（评审 B1 的核心修复）：移除 send 时点规则式派生（`message_handlers.rs:303-323` 一带）——LLM 路径下轮次开始前标题保持占位；no-LLM 路径（无 LLM 工厂、轮次不存在）保留派发时点直接规则式命名。工具启停：宿主在消息派发时点按"未命名"状态为主 agent 启用标题工具、命名后停用；子 agent 的同名注册保持停用（主 agent 门控）。工具启停会触发既有工具表变更提醒注入（ToolsDiffInjector），属 spec 已知副作用。

**Goal**：消息派发宿主成为注入决策与工具激活的唯一控制点；LLM 路径轮次开始前标题保持占位；no-LLM 路径行为不回归。

**验收标准**：
- [ ] LLM 路径：消息派发后、轮次结束前，标题保持占位（A9——全功能可测的前提）
- [ ] no-LLM 路径：消息派发时点直接规则式命名，会话物化、RPC 照常报错（行为保持）
- [ ] 未命名时主 agent 工具面含标题工具、子 agent 不含（A2 关联 + spec §2.4 门控）
- [ ] 命名后工具停用；命名后首个用户轮次一次性工具移除提醒、此后归零（A7）
- [ ] mutation 红线：恢复 send 时点派生即红（时序契约回归保护）

**风险**：send 时点派生移除波及的既有测试（现状派生有测试覆盖）——按"不改实现迁就旧测试"原则重定向测试到新时序。

---

## T4: 兜底站点：首个用户轮次结束规则式（mirri-embedded）

**优先级**：P1 ｜ **涉及面**：rust（mirri-embedded message_handlers：wait/fire/steer-idle 三站点）

**背景**：按 ADR-017 决策 6：首个用户发起的轮次结束（completed/failed/cancelled）仍未命名 → 规则式截取（仅用户消息文本，无文本保持未命名）。兜底站点为 message wait / fire / steer-idle 三处（`origin=user`）；cron/task 的 turn-end 站点不挂兜底。cancelled 双分支语义见 A10。

**Goal**：首个用户轮次无论如何结束，会话最终有标题（模型命名、或规则式兜底、或无文本时保持未命名待后续消息）。

**验收标准**：
- [ ] 模型未调用工具 → 轮次结束标题为用户消息文本截取（A4）
- [ ] 轮次 cancelled 且模型未调工具 → 兜底照常；已调工具 → 标题已写、兜底跳过（A10）
- [ ] steer-idle 作为会话首个用户轮次 → 与普通首条消息同语义（A11）
- [ ] 纯图片消息 → 轮次结束保持未命名，后续用户消息再触发（A6）
- [ ] mutation 红线：删兜底站点即红

**风险**：三站点兜底的幂等性（轮次结束可能多次通知）——守卫闭包天然幂等，brief 确认站点语义。

---

## T5: 验收矩阵与 mutation 收口

**优先级**：P1 ｜ **涉及面**：rust（e2e 测试 + mutation 评审）

**背景**：spec §1.4 的 A1-A11 验收矩阵需端到端覆盖（含 mock LLM 的工具调用注入路径、守卫竞态、时序前提）。守卫竞态（A3）与注入自熄（A7）是用户可感知正确性的核心，须有隔离用例。

**Goal**：A1-A11 全部有自动化 Given-When-Then 用例；mutation 评审通过；全量门禁绿。

**验收标准**：
- [ ] A1-A11 与 spec 验收表逐条对照通过（e2e：mock LLM 会调用标题工具的场景）
- [ ] A3 竞态隔离用例：用户改名与工具调用并发，先到先得
- [ ] mutation 评审：每任务至少 1 条红线独立重做全红
- [ ] `cargo test --workspace` 全绿 + clippy 零告警 + zero-IO 守卫 0 命中
- [ ] 自命名后第二个用户轮次起请求中无命名指令与标题工具（成本验证）
- [ ] changeset 生成（gen-changesets skill）

**风险**：mock LLM 的工具调用编排复杂度——沿用既有 in-process harness 的 mock 模式。

---

## 批次划分建议

T1+T2 为一批（core 侧：工具 + 注入，互为独立可并行）；T3 为一批（时序迁移是全功能开关点，单独评审）；T4+T5 为一批（兜底 + 验收收口）。每批走 SDD 循环；批次完成后更新本档状态列与 ROADMAP 对应行。