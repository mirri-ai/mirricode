# 技能目录热更新（skill catalog hot-reload）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 技能目录文件变更（新增/删除/修改）后，无需重启：watcher 防抖触发重扫 → diff 门控 → 界面事件 + 运行中会话下一轮清单系统提醒；手动重载（RPC + TUI 命令）兜底；watcher 失效静默降级。

**Architecture:** 四层改动（全部 rust 核心 + TUI 消费端，冻结 TS 核心包零改动）：① mirri-core 基建层——SkillCatalog 整表原子替换 API + diff、技能根收集函数、会话级 pending 提醒状态与 flush；② mirri-embedded 编排层——重扫编排入口（串行化）、notify watcher、workspace 级技能目录变更事件；③ RPC 面——`skill.reload` 方法（五环契约链）+ 预留事件透出；④ TUI——`/skills reload` 内置命令 + 提示呈现。web/desktop 消费端批次不在本计划（后续另行落地）。

**Tech Stack:** rust（tokio workspace full、notify 6 迁入 mirri-embedded）；TUI TypeScript。

**Spec:** 需求 [../../skills-requirements.md](../../skills-requirements.md) SDR-008 / US7.2；架构 [../../skills-architecture.md](../../skills-architecture.md) ADR-010；现状证据 [../../skills-context.md](../../skills-context.md) §11。

## Global Constraints

- 引擎行为只落 `rust/crates/`（TS `packages/*` 冻结）；node-sdk 只动两处既定豁免面：**test fixture**（契约链环节）与**客户端薄封装**（`reloadSkills()` 一个方法，逐字镜像 `reloadPlugins` 形态——纯 RPC 转发、零引擎逻辑，属 SDK 发布面而非引擎功能；`/skills reload` 的 TUI 实现依赖它）。其余 `packages/*` src 零改动。
- 重扫必须复用 `apply_skill_discovery`（`rust/crates/mirri-embedded/src/bootstrap.rs:415`，内部 clear + 完整重放），不新造扫描路径。
- 重扫入口按 workspace 串行化（单 mutex）：监视器触发、config 重放、`plugin.reload` 技能段、手动重载四路共用。
- 清单提醒走 `append_system_reminder` + wire 记录（`system_reminder_wire.rs` 原语），复用 interruption reminder 的 marker-防重放模式；不修改 agent 系统提示词本体。
- 「内容变更」判定粒度 = `SkillDef` 全字段级（`name/description/path/source/plugin/disable_model_invocation/is_sub_skill`，已 derive `PartialEq/Eq`）；纯正文编辑不触发通知。
- 提醒门控用 `ToolManager::is_tool_active("Skill")`（实际生效判定面；ADR-010 决策 6 的 `AgentToolPolicyService` 在 rust 无实例化——侦察已确认，以 ToolManager 为准）。
- 测试命名 Given-When-Then；优先加进既有测试文件。
- 验证命令：`cargo test --manifest-path rust/Cargo.toml -p mirri-core -p mirri-embedded`（后台任务，≥20 分钟超时）；全量 `cargo test --workspace` 只在 Phase 4 跑一次。

---

### Task 1: mirri-core — SkillCatalog 整表原子替换 + diff

**Files:**
- Modify: `rust/crates/mirri-core/src/skill.rs`
- Test: `rust/crates/mirri-core/src/skill.rs` 内既有 `#[cfg(test)]` mod（或 skill 模块既有测试文件）

**Interfaces:**
- Produces（Task 2 消费）:
  - `impl SkillCatalog { pub fn snapshot(&self) -> HashMap<String, SkillDef> }`（克隆 skills 读锁内容）
  - `impl SkillCatalog { pub fn replace_all(&self, skills: HashMap<String, SkillDef>, builtin_contents: HashMap<String, &'static str>) }`（单写锁原子替换整表 + 内嵌表；user_disabled 不动）
  - `pub struct SkillCatalogDiff { pub added: Vec<String>, pub removed: Vec<String>, pub updated: Vec<String> }`
  - `pub fn diff_skill_catalogs(before: &HashMap<String, SkillDef>, after: &HashMap<String, SkillDef>) -> SkillCatalogDiff`（按名对齐；字段不等 → updated）

**Steps:**

- [ ] **Step 1: 写失败测试** — Given 两个不同内容的 catalog 快照，When diff，Then added/removed/updated 各自正确（同名同字段不进 updated；`description` 变化进 updated）。Given `replace_all` 后，When `resolve`，Then 新表生效且 `user_disabled` 集保留。
- [ ] **Step 2: 实现** `snapshot` / `replace_all` / `diff_skill_catalogs`（`SkillCatalogDiff` derive `Debug/Clone/PartialEq`，字段排序保证确定性输出）。
- [ ] **Step 3: 跑** `cargo test -p mirri-core skill` 绿。

---

### Task 2: mirri-core — 技能根收集函数（watcher 监听集合的单一出口）

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/workspace_scope.rs`
- Test: workspace_scope 既有测试 mod

**Interfaces:**
- Produces（Task 4 消费）:
  - `impl WorkspaceScope { pub fn skill_root_paths(&self, mirri_home: Option<&Path>) -> Vec<PathBuf> }` — 项目 `.agents/skills`、项目品牌 `.mirri-code/skills`、用户 `~/.agents/skills`（HOME env）、user-brand `<mirri_home>/skills`（mirri_home 入参，与 `apply_skill_discovery` 的 `resolve_mirri_home` 解析规则逐字对齐——`WorkspaceScope` 自身不知道 mirri home）。归一去重（对齐 ADR-011 的默认根去重语义），含尚不存在的候选目录（watcher 需要监听候选）。

**Steps:**

- [ ] **Step 1: 写失败测试** — Given 临时 workspace cwd + 设 HOME env，When `skill_root_paths(mirri_home)`，Then 含四个根（项目 `.agents/skills`、项目品牌、用户 `~/.agents/skills`、user-brand `<mirri_home>/skills`——mirri_home 由调用方传入，`WorkspaceScope` 不知道它）、去重（对齐 ADR-011 语义）、不要求存在。
- [ ] **Step 2: 实现**（复用 `discover_default_skill_roots` 内部的路径构造逻辑，提取为返回集合的形式；原函数行为不变）。
- [ ] **Step 3: 跑** `cargo test -p mirri-core scope` 绿。

---

### Task 3: mirri-core — 会话级 pending 清单提醒（状态 + flush）

**Files:**
- Modify: `rust/crates/mirri-core/src/scope/session_scope.rs`（session scope 增 pending 句柄）
- Modify: `rust/crates/mirri-core/src/turn_flow/mod.rs`（TurnFlow 增可选 pending 句柄 setter；回合启动处 flush 逻辑，`run_one_turn_inner` 约 `turn_flow/mod.rs:1105`——`on_turn_started` 之前）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`（布线点：`get_or_create_turn_flow` 区域约行 1260-1330——交互主 agent 的 TurnFlow 创建处注入 pending 句柄）
- Test: 上述文件的既有测试 mod

**Interfaces:**
- Produces（Task 5 消费）:
  - Session scope：`pub fn mark_skill_catalog_refreshed(&self)`（置 pending；不渲染内容——渲染推迟到 flush 时刻取最新 catalog）
  - Session scope：`pub fn take_pending_skill_reminder(&self) -> bool`（取走并清除）
  - `turn_flow`：回合启动前，`take_pending_skill_reminder() == true` 且 `ToolManager::is_tool_active("Skill")` 时，渲染当前 catalog 清单（`render_skills_section` + 前置变更摘要行），经 `ContextMemory::append_system_reminder` 追加 + `record_system_reminders` 记 wire op（`system_reminder_wire.rs` 原语）。

**Steps:**

- [ ] **Step 1: 写失败测试** — Given 会话被 mark 且工具面含 Skill，When 下一轮 `run_one_turn`，Then 回合上下文出现带 `DISREGARD any earlier skill listings` 头的 system-reminder（一次，不重复）；工具面不含 Skill 的会话不注入；无 pending 不注入。Given catalog 被清空（diff 非空、清单为空），Then 提醒为清空声明文案（非空串、非旧清单残留）。Given subagent/cron/goal 驱动的 TurnFlow（不携带 pending 句柄），Then 回合不消费 pending、不注入。
- [ ] **Step 2: 实现** pending 句柄（session scope 持有；`SkillCatalog` 与 pending 打包为共享句柄供渲染）与 flush 挂点；**布线只进交互主 agent flow**——pending 句柄经 setter 注入用户消息派发链路创建的 TurnFlow（`message_handlers.rs` 的 `get_or_create_turn_flow` 区域，约行 1260-1330），子代理宿主 / cron / task / goal driver 的 `TurnFlow::new` 调用点不注入句柄。
- [ ] **Step 3: 跑** `cargo test -p mirri-core` 绿（重点 turn_flow 相关套件）。

---

### Task 4: mirri-embedded — 重扫编排入口（串行化 + diff + 通知编排）

**Files:**
- Create: `rust/crates/mirri-embedded/src/skill_reload.rs`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（`resync_skill_catalogs`、`refresh_plugin_consumers` 技能段改经编排入口；server 持有 `skill_reload` 状态）
- Modify: `rust/crates/mirri-embedded/src/lib.rs`（模块注册）
- Test: `rust/crates/mirri-embedded/src/skill_reload.rs` 内测试 mod

**Interfaces:**
- Produces（Task 5/6 消费）:
  - `pub struct SkillReloadOrchestrator { /* per-workspace mutex + broadcast tx */ }`
  - `impl SkillReloadOrchestrator { pub fn reload_workspace(&self, runtime: &Arc<WorkspaceRuntime>, ...) -> SkillReloadOutcome }` — 单 workspace 串行重扫：snapshot → scratch 构建 + `replace_all`（Task 4 Step 2 的原子落位）→ diff → 非空则：置各活跃 session 的 pending 标志 + 发 workspace 级事件。锁用 `std::sync::Mutex`（`apply_skill_discovery` 全程同步 IO 无 await 点，std 锁在 sync 与 async 两个调用上下文均安全且保持 sync 签名零级联；**不得跨 await 持有**——`tokio::sync::Mutex::blocking_lock` 在 async 上下文 panic，禁用）。
  - `pub struct SkillReloadOutcome { pub diff: SkillCatalogDiff, /* watcher 运行状态 */ }`
  - workspace 级事件：`tokio::sync::broadcast::Sender<Value>`（形态对齐 `mcp_pool_events`，`embedded_server/mod.rs:1171`），负载 schema：`{ "workspace_id": String, "added": [String], "removed": [String], "updated": [String], "trigger": "file"|"config"|"plugin"|"manual" }`（计数由数组长度派生）；事件经 `skill.reload` 之外的订阅面透出（Task 6）。

**Steps:**

- [ ] **Step 1: 写失败测试** — Given 带 2 个技能的临时目录装配的 runtime，When 新增一个技能目录后 `reload_workspace`，Then diff.added 含新名、事件通道收到一条广播（负载含计数与技能名清单）、活跃 session pending 置位。技能无变化时 diff 为空、无广播、无置位。并发两次 reload 串行完成。
- [ ] **Step 2: 实现原子落位改造（验收项非可选）** — `apply_skill_discovery` 重构为**向 scratch `SkillCatalog` 构建后 `replace_all` 落位**：内部新建 scratch catalog → 完整发现序列写 scratch（含 `discover_default_skill_roots`——需把它参数化为接受目标 catalog 或返回发现结果，消除「直接写 self.skills」的障碍，`workspace_scope.rs:94`）→ `set_user_disabled` 保留在主 catalog → `replace_all(scratch 快照)`。装配首次调用同一函数（首落位也是原子 swap，行为等价）。既有全部调用方零改动。
- [ ] **Step 3: 接线既有触发路径** — `resync_skill_catalogs`（`mod.rs:1797`）与 `refresh_plugin_consumers` 的技能重放段（`mod.rs:2106`）改调 `reload_workspace`（保留其余副作用如 hooks/MCP 刷新不动）。**签名保持 sync**：`reload_workspace` 用 `std::sync::Mutex`（无 await 点），`resync_skill_catalogs` 及其三个上游调用点（`config_handlers.rs:131/195`、`session_handlers.rs:5211`）签名零改动；async 调用方（watcher 任务、`refresh_plugin_consumers`）在自身 async 上下文内直接调 sync 函数（纯 CPU/IO 无阻塞时长问题，目录扫描毫秒级）。
- [ ] **Step 4: 跑** `cargo test -p mirri-embedded` 绿（重点既有 resync/plugin 套件不回归）。

---

### Task 5: mirri-embedded — notify watcher（技能目录监视）

**Files:**
- Modify: `rust/crates/mirri-embedded/Cargo.toml`（`notify = "6"`）
- Modify: `rust/Cargo.toml`（`[workspace.dependencies]` 增 `notify = "6"` 条目）
- Create: `rust/crates/mirri-embedded/src/skill_watcher.rs`
- Modify: `rust/crates/mirri-embedded/src/embedded_server/mod.rs`（runtime 构建后挂载 / remove 时卸载）
- Test: `rust/crates/mirri-embedded/src/skill_watcher.rs` 内测试 mod

**Interfaces:**
- `pub struct SkillDirectoryWatcher { /* per-runtime watcher 任务句柄 + 显式 shutdown 通道 */ }`
- `spawn_skill_watcher(runtime: &Arc<WorkspaceRuntime>, orchestrator: &SkillReloadOrchestrator, extra_roots: Vec<PathBuf>, plugin_roots: Vec<PathBuf>) -> Option<SkillDirectoryWatcher>` — 监听集合 = `runtime.scope.skill_root_paths()` + extra + plugin roots；候选目录不存在时监听父目录、创建事件补挂（对齐 `mirri-cli/src/watcher.rs` mcp watcher 先例，行 372-388）；`Err` 回调丢失时重注册重试（`WATCH_RETRY_DELAY` 模式）；静默期 ~1s（高于 config watcher 的 200ms）；触发 = `orchestrator.reload_workspace`。
- **shutdown 语义（必须显式）**：spawned 任务持有 watcher 永跑，drop 句柄不停止任务——`SkillDirectoryWatcher` 内置 shutdown channel（`tokio::sync::oneshot` / `CancellationToken`），Drop 时发信号、任务收到后退出 select 循环（防任务泄漏）。
- **watch set 动态刷新**：`reload_workspace` 完成后重算监听集合（extra dirs 配置可能变、plugin 根可能增删）并重挂（增删路径 diff 后重挂，幂等）。**防 Arc 环**：watcher 持 orchestrator 强引用（触发 reload），orchestrator 只持 `Weak<SkillDirectoryWatcher>`（或经注册表间接寻址）——双向强引用会阻止 runtime remove 后的释放。
- 失败语义：创建失败记 `warn!` 日志返回 `None`（该 workspace 自动感知停用），不影响其他功能。

**Steps:**

- [ ] **Step 1: 写失败测试** — Given 临时技能目录 + 已装配 runtime，When 向目录写入新 `SKILL.md`（防抖窗内），Then 静默期后收到 reload（orchestrator 被 mock/spy 或经事件通道断言）。写入无关文件（`.tmp` 等）不触发。watcher 挂在不存在的父目录下、目录被创建后能触发。**Drop 句柄后任务退出（等待短暂时间后无新触发、任务计数归零）**。
- [ ] **Step 2: 实现**（搬用 `mirri-cli/src/watcher.rs` 的 `spawn_watcher` 泛型骨架——将其复制/下沉为 embedded 内可复用形态，或直接内联精简版：recommended_watcher + channel + `wait_for_quiet_period` 语义 + 重注册。以 `mirri-cli` 侧不动为原则）。
- [ ] **Step 3: 生命周期接线** — runtime 构建路径（`build_workspace_runtime` 尾部 / `ensure_built` 之后）挂载；`WorkspaceRegistry::remove` 卸载（drop watcher 句柄触发 shutdown）。
- [ ] **Step 4: 跑** `cargo test -p mirri-embedded` 绿。

---

### Task 6: RPC 面 — `skill.reload`（五环契约链）

**Files:**
- Modify: `rust/crates/mirri-proto/src/rpc/registry.rs`（`both_kept("skill.reload", D::Skill)`，对齐 `skill.setEnabled` 行 490 形态）
- Modify: `rust/crates/mirri-proto/src/rpc/authoritative-methods.tsv`（新行）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/routes/agent.rs`（Route + handler）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`（`dispatch_skill_reload`：遍历 built runtimes → `reload_workspace` → 汇总 diff 计数返回；参照 `dispatch_set_skill_enabled`，行 4943；**另增事件订阅透出**：orchestrator 的 broadcast 经参照 `subscribe_mcp_pool_events` 的先例（约行 4934）挂成 `skill.catalogChanged` 类通知，供连接端订阅——事件负载即 Task 4 的 schema）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/responses.rs`（`SkillReloadResult` wire 类型，参照 `SkillSetEnabledResult` 行 425 所在文件——非 mirri-proto）
- Modify: `packages/node-sdk/test/fixtures/rust-engine-methods.ts`（新条目 + 计数 +1）
- Modify: `rust/crates/mirri-test-harness/tests/cross_backend_contract.rs`（serve 面计数断言 +1，行 2022 附近；补双端 shape 验证）
- Modify: `packages/node-sdk/src`（客户端薄封装 `reloadSkills()`——逐字镜像 `reloadPlugins` 形态：`session.ts:487` → `rpc.ts:753` 调用链，纯 RPC 转发零引擎逻辑；Global Constraints 已豁免此面）

**Interfaces:**
- `skill.reload {} → { workspaces: [{ workspace_id, added: u32, removed: u32, updated: u32 }], watching: bool }`（watching = 该 workspace 自动感知是否存活，供设置页呈现监听状态——R4）。

**Steps:**

- [ ] **Step 1: 先做五环登记**（registry → tsv → fixture → contract 计数）跑通 contract 测试（handler 可先返回错误 stub）。
- [ ] **Step 2: 写 handler 失败测试** — Given 装配 runtime + 临时技能目录新增技能，When `skill.reload` RPC，Then 返回 added=1 的 outcome；无变化时全 0。
- [ ] **Step 3: 实现 handler** 接 `SkillReloadOrchestrator`。
- [ ] **Step 4: 跑** `cargo test -p mirri-embedded -p mirri-test-harness` + fixture 单测绿。

---

### Task 7: TUI — `/skills reload` 内置命令 + 重载提示

**Files:**
- Modify: `apps/mirri-code/src/tui/commands/registry.ts`（`skills` 命令注册，参照 `plugins` 行 249）
- Modify: `apps/mirri-code/src/tui/commands/dispatch.ts`（`case 'skills'`，参照行 278）
- Create/Modify: `apps/mirri-code/src/tui/commands/skills.ts`（`handleSkillsCommand`：`reload` 子命令 → SDK `reloadSkills()` → 呈现「技能已重载：新增 N / 移除 M / 变更 K」或「无变化」；用法错误给提示文案；无子命令时列出用法）
- Modify: TUI 事件订阅（非阻断「技能已更新」提示——US7.2 界面端呈现是验收项：经 Task 6 的 `skill.catalogChanged` 通知通道呈现状态行/toast 级提示，含变更摘要；TUI 侧以其渲染能力用既有状态提示机制实现）
- Test: `apps/mirri-code/test/tui/commands/skills.test.ts`（已存在，直接加用例）

**Steps:**

- [ ] **Step 1: 写失败测试** — Given mock session，When `/skills reload`，Then 调 `reloadSkills()` 且状态行/提示呈现结果；未知子命令给用法提示。Given 收到 `skill.catalogChanged` 事件，Then TUI 呈现非阻断「技能已更新」提示（含变更摘要）。
- [ ] **Step 2: 实现**（命令注册进斜杠补全的命令组——US3.1 分组语义）。
- [ ] **Step 3: 跑** `pnpm --filter mirri-code run typecheck` + 既有 TUI 命令测试绿（frozen 包测试按需单独跑）。

---

### Task 8: 全链路集成验证 + clippy + changeset

**Files:**
- Test: `rust/crates/mirri-embedded/src/skill_watcher.rs` 或 `skill_reload.rs`（端到端：目录变更 → watcher → reload → session pending → 下一轮 turn 出现 reminder）
- Create: `.changeset/*.md`（`gen-changesets` skill 规则，anchor `@mirri-ai/release`，minor）

**Steps:**

- [ ] **Step 1: 端到端测试**（真 watcher + 真 turn flow，走通「文件 → 提醒」全链，含工具面不含 Skill 的负例 + **回放测试**：会话恢复/重放重现提醒且不产生激活事实/遥测——m-6）。
- [ ] **Step 2: `cargo clippy --workspace --all-targets -- -D warnings` 绿（后台，≥20 分钟）。
- [ ] **Step 3: 全量 `cargo test --workspace`（后台，≥20 分钟超时）绿。
- [ ] **Step 4: changeset**（用户可感知：技能热更新——minor）。

---

## Notes

- **与 plan 03（默认根去重）的顺序**：两者都改 `workspace_scope.rs` 的默认根发现区（plan 03 = ADR-011 路径归一去重，本计划 Task 2 = 提取根集合函数）。若 plan 03 已合并，Task 2 的根收集须在去重语义之上提取；若未合并，先落本计划，plan 03 rebase 时适配。实现时先 `git log --oneline -- rust/crates/mirri-core/src/scope/workspace_scope.rs` 确认现状。

- **web/desktop 消费端**（设置区块按钮 + 事件提示）不在本计划，后续批次落地（ADR-010 影响范围已注明「消费端批次」）；事件 wire 透出（Task 6 的通知通道）与 TUI 消费（Task 7）已纳入，web 端后续直接订阅同一通道。
- **开放项（web/ACP 投影，rust/AGENTS.md 契约 3）**：`skill.catalogChanged` 是新客户端可见事件，本期只落了 node-sdk rust 翻译层 + TUI 消费；**web 投影器与 ACP 翻译层未同步**——ACP 端会静默丢弃该通知、web 端订阅不到（本期两端的消费功能本身就不在范围，属「功能未落地」而非「静默失效」，但事件翻译层的补齐要随 web/desktop 消费批次一并做，不能只做按钮不做投影）。
- `mirri-cli` 的 `watcher.rs` 不做重构下沉（保持 CLI 侧不动），embedded 侧新写精简版——共享逻辑的收敛留待后续裁决。
- TS 侧 `skill.list` 等读面无需改动（catalog 原子替换后读面天然见新）。
