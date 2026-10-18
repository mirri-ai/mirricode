# Rust 内核域自主推进计划 — 05: Autonomous Advancement

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——本文件持**批次推进状态与循环协议**；开放项的项状态唯一源是 [../ROADMAP.md](../ROADMAP.md)。两者职责分开：ROADMAP 回答"每项是什么状态"，本文件回答"按什么顺序、以什么协议推进"。
> 架构主体: [../00-overview.md](../00-overview.md) · MCP 域: [../../mcp-architecture.md](../../mcp-architecture.md) · 协作域: [../../collaboration-requirements.md](../../collaboration-requirements.md)

**Goal:** 在无需用户逐步确认的前提下，按 SDD 协议自主清偿 ROADMAP 全部自主可推进项，直到撞上用户裁决门、触发停止规则、或全部完成。

**Architecture:** 自主面按价值与依赖切成批次（安全修复 → 卫生债 → MCP P1 → MCP P2 → 视野 2 重估）；每批独立走完 SDD 循环；用户裁决项单列队列，不阻塞主线。批次内跨域（rust / TS 分离）时拆成多个 commit 保持单一职责。

**Tech Stack:** Rust workspace（`cargo test` / `clippy` / `fmt`）+ TS workspace（pnpm typecheck / vitest）+ mirri-proto 契约链（注册表 ⇄ TSV ⇄ EMB 路由 ⇄ NAPI ⇄ contract-e2e fixture）。

## Global Constraints

- **提交授权**：用户已预先授权推进期内**每批 conventional commit**（附 changeset、无 co-author、不透露 agent 身份）；`major` bump 任何时候仍需用户确认（硬门）。
- **质量门禁**（ROADMAP §6）：`cargo test --workspace` 全绿（基线随批次更新）；`cargo clippy --workspace --all-targets -- -D warnings`；受影响 TS 包 typecheck + vitest（含 contract-e2e fixture 更新）。
- **测试命名** Given-When-Then；不改实现迁就旧测试——除非实现真有 bug（批次 1 正是此情形：测试固化了错误行为，测试随实现一起修）。
- **环境护栏**：跑大 TS 套件前 `sysctl vm.swapusage`，swap 用量 >6GB 则记录并延后该验证（2026-09-04 jetsam 事故教训）。
- **不夹带无关重构**：C4 拆大文件等保持独立批次，不混入功能批。
- **契约变更**同步事实源链全环节（TSV / NAPI / fixture）。
- 每批开始前确认 `git status` 干净（保持可回滚粒度）。

## 每批循环（SDD 协议）

```
brief（侦察 + 设计，写入 .superpowers/sdd/autonomous-advancement/task-<N>-brief.md）
  → TDD 实现（测试先行，红→绿）
  → 评审（含 mutation；REJECT 则修复重审）
  → 质量门禁（Global Constraints）
  → changeset（gen-changesets skill）
  → commit（conventional，无 co-author）
  → ROADMAP 项状态更新 + 本文档批次状态勾选 + 账本 progress.md
  → 批次报告（改了什么 / 验证证据 / 遗留）
```

## 停止规则

1. 撞用户裁决门（major bump、产品决策、范围外发现）→ 记入裁决队列、跳过该项、继续下一自主项。
2. mutation 评审 REJECT 修复两轮仍不过 → 该项暂停上报，可继续其他独立项。
3. 环境阻断（内存 / jetsam / 依赖缺失）→ 记录证据、执行替代验证、上报。
4. 发现需架构裁决的新问题 → 记入 ROADMAP §5 候选，不当场改架构。

## 用户裁决队列（不阻塞自主面，随时可解锁）

| 裁决 | 解锁 | 状态 |
|---|---|---|
| B6 major 确认（23 个 Retired 方法移除，pin 179→156） | 1.1 移除、1.6 v1 迁移盘点、3.6 TSV 刷新 | 待表态 |
| 协作 Q1-Q5 表态 | 1.7 架构文档 → 协作落地（3.4 N2 随 Q4 裁决） | 待表态 |
| npm 渠道评估的产品输入 | 2.5 切换发布评估 | 待表态 |

---

## 批次 0 — 文档收尾 ✅

- [x] 15 个治理文档提交（用户已提交 `0242f75f`）
- [x] vitepress build 实证：`build complete in 8.45s`，`srcExclude 'specs/**'` 覆盖 plans/ 移动（2026-09-04）

## 批次 1 — 安全修复：`extract_file_accesses` 字段错配（ROADMAP 3.5）✅

**事实链（侦察已完毕）**：
- 内建工具读 `path`：`rust/crates/mirri-core/src/tools/read.rs:116`、`write.rs:103`、`edit.rs:85` 均为 `input.get("path")`
- 策略钩子读 `file_path`：`rust/crates/mirri-core/src/turn_flow/hooks.rs:937` `args.get("file_path")`
- 后果：`accesses` 恒空 → `SensitiveFileAccessAskPolicy`（`.env`、SSH key、credentials）与 `GitControlPathAccessAskPolicy`（`.git/` 控制路径）**对真实工具调用永不触发**，落穿到 `DefaultToolApprovePolicy` 自动放行
- 现有测试固化了错误行为：`hooks.rs:1000`（`{"file_path": ".env"}`）、`hooks.rs:1082`（`{"path": ...}` 断言 accesses 为空）

**Files:**
- Modify: `rust/crates/mirri-core/src/turn_flow/hooks.rs:921-946`（实现 + 文档注释，注释中 "carry a reliable `file_path`" 表述一并修正）
- Test: `rust/crates/mirri-core/src/turn_flow/hooks.rs:978-1085`（同文件 tests 模块）

- [x] **Step 1: 改写测试先失败**——`{"file_path": ".env"}` 全部改为 `{"path": ".env"}`；`should_produce_no_accesses_when_file_path_argument_is_missing`（1081 行）前提反转，改为缺参情形 `serde_json::json!({})`；新增一条端到端语义测试：Read 带 `{"path": "/home/u/.env"}` 时 `extract_file_accesses` 产出 1 条 access
- [x] **Step 2: 确认失败** — `cargo test -p mirri-core turn_flow::hooks`：新断言 FAIL（4 例：Read/Write/Edit 提取 + 敏感文件链路）
- [x] **Step 3: 修实现** — `hooks.rs:937` `args.get("file_path")` → `args.get("path")`；文档注释同步为 `path`
- [x] **Step 4: 全绿** — `cargo test -p mirri-core`（1291 passed）+ 全 workspace 3040/3040 + clippy 全绿
- [x] **Step 5: changeset + commit** — patch；`fix(core): extract file accesses from the built-in tools' path argument`（`363951e3`）+ 顺手清偿预存在 clippy 门禁债（`5517574e`）
- [x] **Step 6: ROADMAP 3.5 置完成 + 账本 + 批次报告**

## 批次 2 — 卫生债（ROADMAP 3.7 / 3.8 / 3.9）✅

**Files:**
- Modify: rust 全仓 fmt 漂移（45 处 + `rpc.rs`）
- Test: `rust/crates/mirri-server/src/ipc.rs`（侦察现有覆盖后补 Given-When-Then 测试）
- Modify: `packages/agent-core-v2` 中 `roleAdditional` 表单字段（`profile-shared.ts` / `agentProfileCatalog.ts`，grep 定位）

- [x] **Step 1: fmt 批** — `cargo fmt --all` → `cargo fmt --all --check` 零漂移 → `cargo test --workspace` 全绿（exit 0 / 104 个测试二进制 ok）→ commit `chore(rust): apply the pending cargo fmt drift across the workspace`（`7b7b65c4`）
- [x] **Step 2: ipc.rs 测试** — `tests/common/mod.rs` 加 `start_with_ipc()` 变体（IPC socket 随 TestServer 启动）+ `tests/ipc_tests.rs` 4 个 Given-When-Then 集成测试（请求应答 / 通知+响应帧静默 / 垃圾行 -32700 null-id 错误帧 / 未知方法 -32601 回显 id）；`cargo test -p mirri-server` 126 全绿 + clippy 干净 → commit `test(server): cover the IPC transport with unix-socket integration tests`（`812445b5`）
- [x] **Step 3: roleAdditional 移除（TS 面）** — 侦察结论：表单字段在 web（`apps/mirri-web/src` 0 引用）与 TUI（`apps/mirri-code/src` 0 引用）源码均已不存在，残留仅 `agentProfileCatalog.ts` 中误导性注释（宣称 v1 UI 使用该机制）→ 清除注释；`profile-shared.ts` 的 `renderSystemPrompt(roleAdditional)` 属内建 profile 渲染路径（4 个内建 profile 调用），非表单字段，随 1.3 TS-v2 退役一并消亡 → typecheck + catalog 43 测试绿 → commit `eb86f2d0`
- [x] **Step 4: ROADMAP 3.7/3.8/3.9 置完成 + 账本 + 批次报告**

## 批次 3 — MCP P1 配置读写面（ROADMAP 1.2 P1）

需求源：[../../mcp-architecture.md](../../mcp-architecture.md) Rollout P1 节 + 裁决 A5；现状锚点 [../../mcp-context.md](../../mcp-context.md)。方法名与参数结构以 spec 为准，brief 阶段精读。

**Files:**
- Create: klient `contract/global/mcp.ts`（侦察 `contract/global/` 现有模式后对齐）
- Modify: mirri-proto 注册表（`mcp.config.*` 5 方法）→ EMB 路由表 → embedded / serve 实现
- Modify: 插件 capability state 工具过滤扩展（对齐 spec P1 范围）
- Modify: 契约链下游 — TSV、NAPI、contract-e2e fixture

- [x] **Step 1: brief** — 精读 mcp-architecture.md P1 + A5；侦察 klient contract 模式与注册表/路由/fixture 现状；产出 `task-3-brief.md`（4 方法非 5——mcp.config.test 属 P2）
- [x] **Step 2: TDD** — 子批 3a rust 纵切（klient 契约 `79635527` / mirri-mcp 写路径 `c3a9ff41` / wire+路由 `a4e6f1c5`）；3b TS v2（agent-core-v2 域 `f11c1e5e` / klient facade `713aa09c` / node-sdk+CoreAPI `16c6e86a` / kap-server 路由 `52009906`）；3c 插件过滤双侧（rust `ae80f49a` / TS `036f64c9`）。env 展开值级收敛未在本批做（A5 裁决重审：现文件层 load 时文本级展开双侧一致，值级收敛并入 C5/3.2 卫生线）
- [x] **Step 3: 评审** — 3b-1 独立评审 APPROVE（agent-41）；3c 独立 mutation 评审 REJECT→4 findings 全修复（`5c1dff02`）→复检通过（详见账本）
- [x] **Step 4: 门禁** — `cargo test --workspace` 全绿 + clippy -D warnings 零告警 + TS 面全绿（agent-core-v2 1429 / klient 75 / node-sdk 452 / contract-e2e 140）；mirri-acp conformance 全 workspace 并发下偶发 5s deadline flake 已实证为环境性（单套件 3.08s 20/20）
- [x] **Step 5: changeset（minor）+ commit** — `.changeset/p1-mcp-config-surface.md`（mirri-node / mirri-code / klient / mirri-code-sdk，`a4e6f1c5`…`036f64c9` 共 8 commit）
- [x] **Step 6: ROADMAP 1.2 P1 置完成 + 账本 + 批次报告**

## 批次 4 — MCP P2 三池拆分（ROADMAP 1.2 P2）

需求源：mcp-architecture.md P2 节（用户级共享池 / workspace 池 / 插件捆绑池）。

**Files:** brief 阶段按 spec 定位（连接生命周期、池归属判定、失效重连）。

- [x] **Step 1: brief** — `.superpowers/sdd/autonomous-advancement/task-4-brief.md`（D1-D7 裁决 + 4a-4d 子批划分）；连接爆炸治理目标复述：同 home 的 user 层 N-workspace → 1 共享连接（进程级 `shared_user_mcp_pool_for_home`）
- [x] **Step 2: TDD** — 4a rust 三池拆分 `d2bce00a`（LayeredMcpConfig + UserMcpPool + EMB 双池路由/遮蔽/工具 fan-out + D7 三测）；4b wire 面 `d17df41e`（mcp.status 无遮蔽全局视图 + mcp.config.test 契约链全通，计数 pin 185/171/160/148 联动）；4c TS v2 `mcp.config.test` 全链 `dafa90b3`（降级决策：TS 池拆分超子批体量，独立 followup 批；A5 测试时展开/scratch manager/enabled 钉 true）。惰性 connect 测试盲区：懒连接本身属 P4（裁决 A3），盲区随 P4 懒连接实现补，不在 P2。`ACP mcpServers warn-and-drop` 重裁完成：D6 Session Overlay 裁决写回 mcp-architecture.md §4.2（第四种来源随 session 生灭、不进三池、遮蔽优先级最高、warn-and-drop 仅对 `type:'acp'`、rust ACP 不读 mcpServers 记缺口）
- [x] **Step 3: 评审** — 4a/4b/4c 各自 mutation 评审（4c agent-44 一轮 APPROVE，其 3 个覆盖钉建议随批补入）。C6 触发式 Services 包评估：user 池以组合根注入的共享注册表形态落地（`shared_user_mcp_pool_for_home`），未自然成型为 DI Services 包——C6 维持独立候选（ROADMAP 3.3）
- [x] **Step 4: 门禁 + changeset（minor）+ commit** — `d2bce00a` + `d17df41e` + `dafa90b3`；门禁：cargo test --workspace 零失败 + clippy -D warnings 零告警（4a/4b）；4c TS-only（tsc + 受影响面 223+71/klient 29/node-sdk tsc）。changeset `p2-mcp-pool-split`（4d 统一生成）
- [x] **Step 5: ROADMAP 更新（1.2 P2 ✅、3.3 C6 评估、§5 两行闭环）+ 账本 + 批次报告** — TS v2 池拆分（App-scope user pool + ISessionMcpHandle 组合 + kap-server global + v2 mcp.status 发射）记 conditional followup：v2 退役线（1.3）前是否投入由重估裁决，大概率随退役消化

## 视野 2 — 批次 4 完成后重估排序（候选项池）

**重估结论（2026-09-04，批次 4 收口后）**：引擎 gap → 卫生深水区 → 分发线 → 契约真源迁出。引擎 gap 内部以"用户实测痛点 > CLI 可感知缺口 > 观测面"排序：

- **批 5：Google LLM adapter 接线**（首选）——ROADMAP §4 唯一实测确认的重要缺口：`ProviderLlmFactory` 的 `ProviderType::Google` 分支返回 Err。侦察结论：transport 层（`mirri-kosong/src/google.rs` 226 行，GoogleGenAIProvider + SSE dispatcher + conformance 测试）已完备，缺的是 `Llm` 适配器层（Messages → Google GenAI 请求体、StreamEvent → LlmEvent），形态对齐 openai.rs/anthropic.rs（~650-900 行 + wiremock 测试）；TS 参照 `packages/kosong/src/providers/google-genai.ts`（970 行）
- 批 6 ✅（2026-09-04，`399f4a43`）：注册表 stub 转正——范围修正后仅两项可转正：`config.getExperimentalFeatures`（委托 `flag.explainAll`）与 `mcp.startupMetrics`（`McpStartupTiming` 实测）；`task.detach` / `shell.cancel` 移入 MISSING 引擎功能建设线（TS detach 语义依赖前台→后台转换模型，rust 尚无；`shell.run` 无真 runner），`config.diagnostics` 保持 stub（rust config 无告警机制，独立候选）
- 批 7 ✅（2026-09-04）：`TaskPersistence.read_output` 接入 `TaskOutputTool`——`TaskManager::output_snapshot`（持久日志为准，内存 retained 回退）+ 工具渲染（`output_bytes` 行、`[Truncated. Full output: <path>]` 横幅、Read 工具分页 hint）；TS `getOutputSnapshot` 对位。followup：rust 无 `outputWriteQueue` 等价物（快照可落后一拍 append，微秒级）；RPC 面 `output_for_session` 仍走内存视图（本批 scope 外）
- 批 8a ✅（2026-09-04，`278d7e9f`）：`thinking.delta` 垂直落地（SessionEvent 变体 + 投影 + relay arm + TS fixture/契约套件，词汇表 16→17；评审 REJECT→三处文档计数修正）。**事件面侦察拆批结论**（agent-48）：8b = `tool.call.delta`（kosong `StreamEvent::ToolCallPart` 需补 call_id：openai 首 chunk 已有 id、openai_responses 有 item_id、anthropic/google 无 parts）→ 9 = relay seq reset on resume + `session.updated` resumed 发射序（侦察发现两处真缺陷：resumed 在 relay 安装前发布于无订阅者的新 bus；seq/epoch 重启语义未持久化水位）→ 10 = compaction 事件（引擎已在）→ 11 = hook.result → 12 = background.task live → 13 = tool.progress（最深）
- 批 8+ 候选：事件面（tool.progress / thinking.delta / tool.call.delta / hook.result / compaction / background.task live / 服务器事件发射面）→ session reindex → Manual Compaction 6 项 → ErrorCode 错误元数据 → Kimi / Custom provider
- **卫生深水区**：C4 拆大文件（config.rs 4929 行、manager.rs 1955 行、injection.rs）；C5 剩余原语收敛（原子写 4 份、裸 `fs::write`、tracing init 统一；批次 3/4 新积累：N3 PluginStore 裸写、stdio cwd 锚定跨引擎漂移归一）
- **分发线**（ROADMAP §2）：2.1 reqwest → rustls（独立批，网络栈全量验证）→ 2.2 `sync-version` 脚本 + CI 一致性检查 → 2.3 rust-native-build 6 target 矩阵 → 2.4 install.sh musl 支持 → 2.5 切换发布评估（含用户输入，裁决门）
- **契约真源迁出**（1.3 前置）：mirri-proto 注册表经 ts-rs / schemars 生成 TS 契约，或契约独立成包；contract-e2e fixture 改造——与 1.3 TS-v2 退役线联动（批次 4 的 TS 池拆分 conditional followup 一并在此线裁决）

进入前重估价值排序，每项独立成批走同一 SDD 循环。

**排除项**（不在本计划范围）：MISSING 62 + MISSING-ONE 47 引擎功能缺口（另行立项）；SubagentHost v1 范围外四项；B6 / 协作域（用户裁决门后进）。

---

## 批次推进状态

| 批次 | 状态 | 完成 commit |
|---|---|---|
| 0 文档收尾 | ✅ | `0242f75f`（用户提交）+ vitepress 实证 |
| 1 安全修复 | ✅ | `363951e3` + `5517574e`（clippy 债） |
| 2 卫生债 | ✅ | `7b7b65c4` + `812445b5` + `eb86f2d0` |
| 3 MCP P1 | ✅ | `79635527` + `c3a9ff41` + `a4e6f1c5` + `f11c1e5e` + `713aa09c` + `16c6e86a` + `52009906` + `ae80f49a` + `036f64c9` + 评审修复 `5c1dff02`（changeset `p1-mcp-config-surface`） |
| 4 MCP P2 | ✅ | `d2bce00a` + `d17df41e` + `dafa90b3`（changeset `p2-mcp-pool-split`；TS v2 池拆分为 conditional followup——随 1.3 退役线重估） |
| 5 Google adapter（视野 2 首批） | ✅ | `dbc57531`；评审 REJECT→修复→复检 APPROVE；含 dispatcher call-id 合成修复（引擎级并行工具配对缺陷）；changeset `google-llm-adapter` |
| 6 stub 转正（experimentalFeatures + startupMetrics） | ✅ | `399f4a43`；范围修正：`task.detach` / `shell.cancel` 移入引擎功能建设线，`config.diagnostics` 保持 stub（独立候选）；评审 REJECT→TSV 注记修复；changeset `b6-stub-promotions` |
| 7 read_output 接入 TaskOutputTool | ✅ | 评审一轮 APPROVE；TS `getOutputSnapshot` 对位（唯一 delta：无 append 写队列）；changeset `b7-task-output-persisted-log` |
| 8a thinking.delta 事件落地 | ✅ | `278d7e9f`；词汇表 16→17；TS fixture/契约套件同步；评审 REJECT→文档计数修正；changeset `b8a-thinking-delta-event` |
| 9 relay seq 连续 + resumed 发射序 | ✅ | 评审 REJECT（水位 seed 竞态）→ dispatch 侧排空旧 relay 后再 seed + 断言收紧 `seq == 4`；`session.updated {change: 'resumed'}` 从不可达变可达；changeset `b9-resume-seq-and-announcement` |
| 10a compaction 事件（manual 路径） | ✅ | 词汇表 17→20（mapped 7→10）；附带修复 manual cancel no-op + 双重 begin 两个引擎缺陷（attempt id 作用域化 settle）；评审 REJECT（stale settle 竞态 + 测试缺口）→ 修复 + 4 新测试复检 APPROVE；changeset `b10a-compaction-events` |
| 10b compaction 事件（auto 路径 + blocked） | ✅ | 词汇表 20→21（mapped 10→11）；`CompactionBlocked` durable 投影（turnId None 缺席、serde skip 守卫）；TurnFlow bus seam 贯通 hooks 阈值站点 + overflow 恢复站点（四事件对称）；评审一轮 APPROVE（4 项非阻塞 followup 记账本）；changeset `b10b-compaction-auto-events` |
| 11 hook.result 事件（UserPromptSubmit） | ✅ | 词汇表 21→22（mapped 11→12）；hook_engine 补 TS message 链（exit-2 stderr / structured deny reason / stdout 回退）；append 分支渲染 user 消息、block 分支渲染 assistant 消息（content 回退链 message→reason→默认）；空 reason 归一化 None 让消费端默认文案生效；undo 尾清理纳入 HookResult；评审一轮 APPROVE（M1 裸 deny 默认文案 + N1/N3 当批修，M2 多消息单信封记声明分歧，TM1/TM2 补钉）；changeset `b11-hook-result-event` |
| 12 background.task live 事件 | ✅ | `c073e0df`；词汇表 22→24（mapped 12→14）；TaskManager 事件旁路（register 发 started、首 Running→终态发 terminated，stop_all 锁外逐任务）；TaskKind 三类判别（bash=process/agent_tool=agent/ask_user=question），约 53 个 register/spawn 站点迁移；AgentTask 增 kind+ended_at（legacy 文件兼容）；klient agentTaskInfoSchema + contract-parity AgentTaskWire 同步；评审 APPROVE-WITH-FOLLOWUPS（F1 roundtrip 测试、F2 klient mirror、F3 reconcile ended_at 三项当批修）；workspace gate 首跑抓出 test-harness 两处漏网站点后补齐；changeset `b12-background-task-events` |
| 13 tool.progress 事件（引擎级工具更新通道） | ✅ | `19446c97`；词汇表 24→25（mapped 14→15）；`ToolUpdate`/`ToolProgressHandle`（no-op-by-default）+ `ExecutableToolContext.progress` 字段（约 40 站点迁移）；`LiveEventCallback` Box→Arc（sink 进 'static 工具 future）；穿参链 turn_step→execute_tool_calls→run_tool_batch→execute_tool_batch→ctx 构造点包 per-tool sink；SessionEvent::ToolProgress volatile 投影（turnId 缺席沿 started/result 适配）；引擎级严格序测试（started→progress×2→completed）+ relay 端到端；评审 APPROVE-WITH-FOLLOWUPS（SHOULD-FIX 换行损伤当批修；生命周期守卫/turnId consolidated note/params-struct/embedded 全链路 e2e 四项 followup）；真实发射器拆批 13b；changeset `b13-tool-progress-event` |
| 13b bash 流式发射器（tool.progress 首个真实发射器） | ✅ | `3d6953f4` + gate 修复 `48f50564`；mirri-os `take_stdout/take_stderr`（owned handle 镜像 take_stdin）；`read_and_forward`（8KB 块读 + UTF-8 前缀安全 carry + forwarded 偏移防重发 + 最终 `String::from_utf8` 保持旧 read_to_string 错误语义）；stdout/stderr `try_join!` 并发读修复旧顺序读管道死锁；后台路径传 None（对齐 TS 走 TaskOutput 面）；单测 19→21（含评审捐测 invalid-byte-midstream / EOF-mid-sequence 错误语义）+ embedded e2e 全链路（须 setPermission yolo，否则 block 路径假绿——顺带修复旧 e2e 同病）；评审 APPROVE-WITH-FOLLOWUPS（评审者注入 7 对抗测试全绿后移除，SHOULD-FIX 两项当批修）；`48f50564`：conformance RESPONSE_TIMEOUT 5s→30s（workspace gate 负载下冷启动 5s 死线 flake，绿路径零影响）；followup：mirri-os `run_to_completion` 顺序读死锁（零调用者）、invalid-UTF-8 前缀 progress 先于错误的行为分歧（TS 用替换字符）；changeset `b13b-bash-streaming`（patch——无契约面变化） |
| 8b tool.call.delta 事件（工具参数流式增量） | ✅ | `bf11d45d`；kosong `StreamEvent::ToolCallPart` 增 `call_id`/`tool_name`（openai 首块身份；openai_responses 用 pending.call_id 非 item_id，保证与块 ToolCall 一致）→ `LlmEvent`/`LiveEvent` 镜像 → `SessionEvent::ToolCallDelta`（volatile）→ `event.tool.call.delta`，词汇表 25→26（mapped 15→16）；name omit-not-null（协议 zod 拒 null）；None-call_id part 在 relay 丢弃（rust 块事件流尾冲刷，TS header 缓冲不可移植）；测试 kosong 4 + core 4 + relay 2（identity/drop-None）+ e2e 1；TS 分支表+计数+代表帧同步（142 绿）；评审 APPROVE-WITH-FOLLOWUPS（6 mutation：4 CAUGHT，2 gap 当批修）；followup：anthropic/google 不发 parts、TS 原生 mock 不发 parts；changeset `b8b-tool-call-delta-event`（SDK minor） |
| 14 ErrorCode 错误元数据（retryable 上 wire） | ✅ | `4f7e7da9`；`domain_code_retryable` 单源判定表（仅 `context.overflow` true，未知码 false）+ `domain_error_data` 增 retryable（双后端获益）；`TurnFailed` 增 `error_code`，投影 error 对象真实化；`EngineTurnError::domain_code()`（fire-mode 接通，abort 站点 `abort.user_cancelled`）；NAPI 后缀 + `RustEngineRpcError.retryable`；TS fixture turn.ended error.code 放宽 z.string()（rust 专属命名空间适配）；评审 APPROVE-WITH-FOLLOWUPS（M1/M2/M3/M4b CAUGHT，M5/M7 gap + 陈旧文档三项 followup 当批修）；gate 首跑抓出 mirri-server e2e 旧断言后同步；环境事件：磁盘满删 incremental 10G；followup：动态 provider retryable 传播、TS 词表 60+ 码细分对齐；changeset `b14-error-metadata`（SDK minor） |
| 15 Kimi provider adapter | ✅ | `ProviderType::Kimi` + `KimiLlm`（内层 `OpenAILlm` 委托 + `build_base_request`/`provider()` pub(crate) 接缝 + 顶层 `thinking` 方言覆写——`{type:"disabled"}`/`{type:"enabled",effort}`，永不发 `reasoning_effort`）；工厂 `MIRRI_API_KEY`（必需）/`MIRRI_BASE_URL`（默认 `https://api.moonshot.ai`，transport 追加 `/v1/chat/completions`）；解析点：profile `parse_provider_type`、`provider_to_string`、model_handlers "kimi" arm + moonshot 默认 base fallback；8 新测试（serde round-trip / profile 解析 / 工厂 env 链 + wiremock / adapter 4 effort 映射 + SSE 流 / model.generate wire 断言 thinking 方言）；mutation 评审 REJECT（Medium 映射未钉）→ 补测试 → APPROVE（10 mutation 全 CAUGHT 或有据接受）；gate GATE:1（TEST+CLIPPY 双 0）；followup：quota 429 非重试分类（需错误分类基建，与批 14 动态 retryable 同链）、max_completion_tokens/`$`-builtin 工具/prompt_cache_key/视频上传（rust 无对应面）、model_handlers google arm 仍 Err（工厂有 adapter 但 config 路径未接——批 16 已修）；changeset `kimi-provider-rust-engine`（CLI patch） |
| 16 Custom provider（protocol 解析） | ✅ | `model_handlers.rs` 镜像 TS `resolveProtocol` 链：alias `protocol` 优先 → type 为 wire protocol → kimi vendor → invalid_params "must declare a wire protocol"；custom/未注册 type + 显式 protocol = 任意 OpenAI 兼容/anthropic endpoint 走 config 驱动；附带 google config 路径接 GoogleLlm（修批 15 记账的不一致，删旧 error 测试）；修 `ModelAlias.protocol` 陈旧 doc；6 新测试（custom+protocol wiremock / protocol 优先于 type / deepseek 无 protocol 报错 / google-genai wiremock / kimi+显式 protocol 保方言 / openai+effort 发 reasoning_effort 反向钉住）；mutation 评审一轮 APPROVE（6 mutation：4 CAUGHT；survivor=默认 URL 与 "google" 别名容忍——与批 15 已接受 gap 同类 + 4b 结构性 guard 已补测试钉住）；gate GATE:1；changeset `custom-provider-protocol-resolution`（CLI patch）；限制：env-only 工厂路径 Custom 仍 Err（无配置面，需 AgentScopeConfig endpoint 扩展——遗留裁决候选） |
| 17 C5 原子写收敛（首项） | ✅ | `mirri-persistence::fs_utils::write_file_atomically` 单一 durability 原语（unique temp `.<name>.<pid>.<nanos>.tmp` + create_new 0o600 + fsync + rename + best-effort 目录 fsync + 失败路径清理），8 站点全迁移：auth token store、core config.toml、task_persist、AtomicDocument、migration 写回、mcp oauth tokens、mcp config、plugin installed.json（后者原为裸 `fs::write`，torn-write 风险消除；mcp 两处原无 fsync）；加强项：全部 temp/目标收紧 0600、unique tmp 名（原 4 处固定名并发互踩）；mcp/auth/plugin-host 新增 persistence 依赖（无环）；8 原语测试（含并发完整性 + 失败路径清理）；mutation 评审 APPROVE（8 mutation：5 CAUGHT；survivor=fsync 类不可单测 + create_new→truncate 由 unique-name guard 覆盖）；C5 剩余（env 展开值级收敛、tracing init 统一、stdio cwd 锚定）留后续批 |
| 18 C4 首切片：config.rs 拆分 | ✅ | `mirri-core/src/config.rs` 4896 行 → `src/config/` 六模块：mod（类型 + load 链 + migration 区）/ env_layer（env 展开与 MIRRI_* 覆写 impl 块 + env 自由函数）/ wire（wire 投影 ×2 impl 块 + 脱敏 + case/toml-json 转换）/ raw_toml（raw 文档持久化 impl 块）/ home（home 目录解析）/ tests（原 2664 行测试字节级原样）；纯搬移：公共 API 经 mod.rs `pub use` 零变化，跨模块私有引用 6 处提升 pub(crate)（KNOWN_* ×3、merge_map_entry_passthrough、expand_env_vars_with、apply_env_with）；move-only 评审 agent-62 **APPROVE**（全清单核验 + 23 方法逐体 diff 字节一致 + tests.rs cmp 字节一致 + workspace check 绿）；验证 1326/0 零测试修改；ROADMAP 3.1 路径修正（原误记 mirri-embedded）+ 3.2 tracing init 侦察关闭（acp stderr+EnvFilter / cli 显式 level 语义本应不同）；环境事件：磁盘 911Mi 危急再清 incremental 1.3G |
| 19 C4 第二切片：session/manager.rs 拆分 | ✅ | `mirri-core/src/session/manager.rs` 1965 行 → `manager/` 三模块：mod（类型 + 16 方法双 impl 块：构造/create/snapshot/fork/close/list/get）/ restore（resume_session + replay_agent_state impl 块 + 8 自由函数：migrated_records_to_wire_ops、build_agent_config_from_state、restore_session_scoped_state、parse_* ×3、fire_session_*_hook ×2）/ tests（843 行字节级原样）；纯搬移：6 处 pub(crate) 提升（replay_agent_state、fire_* ×2、parse_* ×3，后者经 `#[cfg(test)] pub(crate) use` 再导出避开非测试构建 unused 告警）；move-only 评审 agent-63 **APPROVE**（18/18 方法体一致、16 top-level item 一致、cmp 字节一致、workspace check 绿）；验证 1326/0 零测试修改 |
| 20 C4 第三切片（收官）：turn_flow/injection.rs 拆分 | ✅ | 1753 行 → `injection/` 八模块：mod（Injector trait + InjectionManager + re-export）/ capability_gap / permission_mode / plan_mode / goal（+8 helper）/ todo_list / tools_diff（每注入器一文件，banner 切割点精确定位）/ tests（890 行字节级原样）；**零可见性变更**（三切片中最纯）；move-only 评审 agent-64 **APPROVE**（7/8 文件 cmp 字节级一致，唯一偏差 = capability_gap 头部多行 //! doc 形状差，纯外观）；侦察修正：tests 中 "budget guidance" 为测试名字子串非调用——误判的提升已回退（教训：grep 计数排除 fn 名命中）；C4 至此收官 |
| 21 C5 收官：MCP env 展开值级收敛（A5 落地） | ✅ | `load_config_file` 删文本级展开（parse 原文——同时消除 env 值含引号破坏 JSON parse 的注入面）；新 `expand_server_config_values` 值级原语（仅字符串值、对象键不动、单遍不递归、未定义→空串并收集变量名）+ `expand_json_values` 递归 walker；`do_connect` 单点接入——startup/watcher 热重载/plugin sync/plugin manifest（原完全不展开，本批补齐）/`mcp.config.test` 全路径统一覆盖；连接失败错误与 Failed 状态追加 `(undefined env vars: A, B)` 诊断（A5 要求）；wire 读面零改动（P1 批 3 的 read_raw_config_file 本就原文）；两处 load 展开断言翻转为原文+connect 展开（语义变更即目的）+ 3 新测试（原文保留 / env: 前缀 connect 展开 / 未定义收集）；mutation 评审见账本；环境事件：磁盘 102Mi 三度告急 → `cargo clean` 90G（陈旧多轮哈希副本）→ 85G 干净重建 |
| 视野 2 | ✅ 自主项全部收口（2026-09-06，批 30：三剩余候选评估后均无 actionable 缺口——typed task kinds 批 12 已落地（ROADMAP 行陈旧已划）、records 多版本迁移链 Phase 1/12 已落地（同上）、session reindex 无 TS 基准按定义模糊关闭；前缀一致性校验批 29 已降级低价值候选）；批 29 完成（六项收官：wire record durable 化）；批 28 完成（六项之三 prompt 缓冲回放）；批 26 完成（六项之一 Pre/PostCompact hook + additionalContext）；批 25 完成（分发线 2.4 install.sh musl；分发线自主项 2.1-2.4 全部落地）；批 24 完成（2.3 打包脚本 + CI 矩阵；workflow_dispatch 首跑待 push 后验证）；批 23 完成（2.2 sync-version）；批 22 完成（2.1 reqwest→rustls）；批 21 完成（C4+C5 双收官）；**仅剩用户裁决队列**：2.5 切换发布评估、B6 major 确认（1.1）、协作域 Q1-Q5（1.7）、1.3 退役线节奏、工厂路径 Custom 的 AgentScopeConfig endpoint 扩展（低优先）；停泊候选（不阻塞收口，待未来计划文档立项）：MCP P3/P4、config.diagnostics、Notification hook、C6 DI 包形态、N2 collaboration 二选一、MISSING 引擎功能建设线 | |
| 25 分发线 2.4：install.sh musl 支持 | ✅ | 删 `docs/public/install.sh` musl/Alpine 拒绝分支（原 121-128 行）——musl 用户走 linux-x64/arm64 target 下载静态链接二进制（2.1 已保证产物零 openssl）；验证：bash -n + `_detect_target` 提取单测（darwin-arm64）+ grep 零 musl 残留 + install.ps1 无冲突；mutation 1/1 CAUGHT（stash 恢复拒绝分支 → grep 命中，守卫有牙齿）；changeset `allow-musl-install-script`（CLI patch）；待办：Alpine 容器端到端（需 2.5 发布后真实 release） |
| 26 Manual Compaction 六项之一：Pre/PostCompact hook + additionalContext | ✅ | `04124910`；盘点结论（六项 TS↔rust）：compaction 事件 ✅批10、Pre/PostCompact hook 本批 ✅、steer 缓冲回放/prompt refresh/wire record 仍 gap、前缀一致性校验在 `compaction_round`（同步路径）已有但 `FullCompaction::begin`（异步后台）缺——记入后续批候选；实现：`CompactionBeginParams.hook_additional_context`（新字段）+ `hook_additional_context_messages` helper（空串→无消息；文本→单条 origin=hook_additional_context user 消息）+ `run_compaction_llm` strip 后追加（TS `historyForModel` 对齐）+ auto/overflow 路径补齐收集传递 + dispatch 层 Pre/PostCompact 触发（取消/失败分支不 fire，守卫作用域化避免 guard 跨 await）；5 新测试（helper ×2 + 核心注入/不泄漏 + e2e 触发/取消）；mutation 5/5 CAUGHT（M1 trigger 篡改 / M2 dispatch 断链 / M3 取消守卫删除 / M5 origin 去除 / M6 round 断链）+ M4 strip 顺序有据跳过；GATE:1 双 0（3213/0 + clippy 零告警）；changeset `manual-compaction-hooks`（CLI patch） |
| 28 Manual Compaction 六项之三：prompt 缓冲回放 | ✅ | `ef53d449`；TS 基准 = promptService enqueue/startNext 在 `fullCompaction.compacting !== null && loop !== running` 时把 prompt 留 pending，`onDidFinishCompaction` 回放 startNext；rust 缺口更深——`run_one_turn` 无 manual compaction 守卫，turn 可与 compaction 并发且 apply 从 begin 快照重建 memory 会覆盖窗口内新增消息（数据丢失级）；实现：`FullCompaction` 增 `settle_tx`（watch<u64>）+ `settle_manual` 发信号（幂等 bump）+ `subscribe_settle`；`wait_manual_compaction_settle` helper（check-subscribe-recheck 循环免竞态）；`run_one_turn` 顶部接入——在 turn-flow mutex 下等待故 begin 无法从背后插入（request_manual_compaction 拒绝活跃 turn 且需同锁）；已知耦合：等待窗口计入 ENGINE_TURN_TIMEOUT 300s 预算（summarizer 有自身重试链，实践中远低于此）；steer 无需缓冲（只发生在 turn 中，与 manual compaction 互斥）；3 新测试（idle 立即返回 + settle 唤醒 + e2e 门控时序：gated summarizer 期间 message.send 后台任务 300ms 内不触 LLM（call count==2）→ release → send 完成 + call count==3 + 缓冲 turn 消息在 apply 后存活）；mutation 2/2 CAUGHT（M1 删 run_one_turn 等待→e2e 红；M2 settle 不发信号→单测 1s 超时红）——评审中发现并修复单测敏感性缺陷（`futures::poll!` 不驱动 spawned 任务，settle 先于任务首查导致 vacuous pass；改 sleep 让任务先 park）；GATE:1 双 0（3220/0 + clippy 零告警）；changeset `buffer-prompts-during-manual-compaction`（CLI patch） |
| 29 Manual Compaction 六项收官：wire record durable 化 | ✅ | `35a8bf45`；缺口侦察：`WireOp::ContextApplyCompaction`/`ContextSizeMeasured` 词汇、reducer、restore replay 全部先行就绪，唯一缺口是生产代码从不发射（只有测试手工构造）；TS 基准 = `contextMemoryService.applyCompaction` 在 apply 时 dispatch apply op + size op；实现 = 三路径同形态发射双 op：manual（`compaction_handlers` apply task，与批 27 refresh 合并同块，先 emit 再 refresh）/ auto（`turn_flow/hooks` before_step，`TurnFlowHooksConfig.wire_recorder` 新字段 + `run_one_turn` 构造处传入；`async move` 块外先 clone 避免借 self）/ overflow（`turn_flow/mod`，直接 `self.wire_recorder`）；ContextSizeMeasured 用 apply 后 `memory.history().len()` + `tokens_after`；1 e2e 新测试（message.send → beginCompaction（summary 含 marker）→ 第二轮消息 → close → resume，断言重放 history 含 marker + 后继消息 + 首条 origin==CompactionSummary）；mutation 2/2 CAUGHT（M1 删 manual 发射 → resume 断言红；M2 summary 置空串 → marker 断言红；auto/overflow 站点无专属测试记 accepted gap——发射模板三处同形，manual 有 e2e 钉住）；GATE:1 双 0（3221/0 + clippy 零告警）；六项总账 5✅ + 前缀校验降级为低价值候选（批 28 缓冲后 turn 不再与 compaction 并发，begin 路径剩余窗口大幅缩小）；changeset `persist-applied-compaction-to-wire-log`（CLI patch） |
| 27 Manual Compaction 六项之二：prompt refresh | ✅ | `5e9d0cc8`；TS 基准 = `compactionWorker` 成功后 `profile.refreshSystemPrompt()`（重读 instructions/plugin sections/tool policy，失败 warning 不传播）；rust 形态 = 急切组装（创建时 file/AGENTS.md/dir tree 一次性注入 config），故 base（profile 解析态）与 workspace 后缀分离：AgentScope 新增 `base_system_prompt`（RwLock<Option<String>>，直接构造的 scope 无 base → refresh no-op）+ `set_system_prompt`；`SessionScope::refresh_agent_system_prompt`（重读 AGENTS.md + 重生成 dir tree，base 固定，返回 bool）；后缀构造抽 `agents_md_suffix`/`dir_tree_suffix` 共享（创建与刷新字节级一致，None 分支保持 trim_start 旧行为）；manual apply 成功后 best-effort 调用（apply task 捕获 session_manager/session_id/agent_id）；4 新测试（核心重建含新 AGENTS.md 不含旧 / 无 base no-op / 未知 agent no-op + e2e compaction 后 V2 生效）；mutation 4/4 CAUGHT（apply 断链 / base 改用当前 prompt 重复追加 / 不捕获 base / 不写回）；GATE:1 双 0（3217/0 + clippy 零告警）；changeset `refresh-system-prompt-after-compaction`（CLI patch） |
| 24 分发线 2.3：release-package + CI 矩阵 | ✅ | 打包脚本（产物格式与 TS 侧字节级兼容：平铺 zip + mirri(.exe) 根级 + 0o100755 + `<digest>  <name>\n` sha256）+ rust-native-build.yml（6 target 矩阵镜像 _native-build.yml runner；musl 走 zigbuild）；本地验证全绿 + mutation 2/2；无 changeset（纯基建无 npm 面）；待办 = workflow_dispatch 首跑 |
| 23 分发线 2.2：sync-version + CI 防漂移 | ✅ | `rust/scripts/sync-version.mjs`（sync + --check 双模式；npm version 为真源；17 lock 条目三向一致校验）；workspace 0.1.0 → 1.11.0 实际对齐（D6 已裁决）；ci.yml lint job 挂 --check；GATE:1 双 0；mutation 2/2 CAUGHT（workspace 漂移 / lock 漂移均 exit 1；脚本内部逻辑类 mutation 属 CI 消费面——与 check-nix-workspace.mjs 同约定）；实现期自捕两个脚本 bug（matchAll 隔块漏检 + section 正则截断） |
| 22 分发线 2.1：reqwest → rustls | ✅ | workspace Cargo.toml reqwest 翻转（default-features=false + rustls-tls + 显式 blocking）；lock 中 openssl 全移除；三 Linux target tree 零 openssl；kosong wiremock 全绿 + live TLS 真握手绿；tls_smoke.rs 双守卫（lock 纯度 + ignored live 握手）；GATE:1 双 0；mutation 2/2 CAUGHT（M1 删 rustls-tls → live 冒烟红；M2 恢复 default → lock 守卫红）；changeset `rustls-http-stack` |
