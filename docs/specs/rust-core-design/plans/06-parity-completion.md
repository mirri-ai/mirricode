# Rust 内核域计划 — 06: 全终态可用规划（内核 + Web + Desktop + ACP + TUI）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——本文件持**批次推进状态与循环协议**；开放项状态唯一源是 [../ROADMAP.md](../ROADMAP.md)。
> 架构主体: [../00-overview.md](../00-overview.md) · 缓存参照: `~/workspace/opensource/deepseek-harness/cache-hit-rate-design.md`（六战线框架，设计参照非代码依赖）· ACP 协议真源: 官方 `schema/v1`（本地 SDK types.gen.d.ts）

**Goal:** 让 rust 引擎达到用户定义的终态：**内核功能完整可用；Web 与 Desktop 所需的全部功能和接口完整可用；ACP 完整功能可用；TUI 完整功能可用。**

**Architecture:** 六路审计（引擎功能面 / 架构符合度 / 缓存命中率 / wire 写侧自核 / Web+Desktop 消费面 / TUI 路径 / ACP 合规）的产出合并为一个批次队列。最大结构性发现：**引擎侧事件投影已相当完整，三个客户端适配层（web 投影器前缀、SDK rust 事件翻译、ACP 词汇）各自截断了事件通道**——所以 P0 批是三处客户端适配止血，性价比远高于引擎侧单项。之后按"共享引擎缺口优先于单端缺口"推进：shell/文件/持久化/管理面各批同时解锁多端，ACP 合规与缓存 BLOCKER 独立纵切，最后切 TUI 默认引擎并跑四面终验矩阵。

**Tech Stack:** Rust workspace（cargo test / clippy / fmt，本地门禁）+ TS workspace（pnpm typecheck / vitest / Playwright）+ mirri-proto 契约链（注册表 ⇄ TSV ⇄ EMB 路由 ⇄ NAPI ⇄ contract-e2e fixture）+ ACP 官方 v1 schema。

## Global Constraints

- **本地门禁**（CI 先不做，按用户指示）：`cargo test --workspace` 全绿、`cargo clippy --workspace --all-targets -- -D warnings` 零告警、受影响 TS 包 typecheck + vitest 全绿。
- 测试命名 Given-When-Then；不改实现迁就旧测试（除非实现真有 bug）。
- changeset 走 gen-changesets skill；`major` bump 任何时刻需用户确认（硬门）。
- **ACP 相关测试必须对照官方 v1 schema 写**，不得沿用 rust 自认的旧草稿方言自证（当前 conformance.rs 全绿 = 自洽 ≠ 合规，这是 F 审计的核心教训）。
- **缓存类改动须可观测证明**（usage 三桶数值 / 前缀前后对比），不允许"应该会更好"式验收。
- 环境护栏：跑大 TS 套件前 `sysctl vm.swapusage`，swap >6GB 记录并延后（jetsam 教训）。
- 契约变更同步事实源链全环节。
- 每批开始前确认 `git status` 干净（保持可回滚粒度）；工作树中已有的 auth 互通改动先由用户处置后再进批。

## 每批循环（SDD 协议，同 05）

brief（侦察 + 设计，`.superpowers/sdd/parity-completion/task-<N>-brief.md`）→ TDD（红→绿）→ 评审（含 mutation）→ 本地门禁 → changeset → commit → ROADMAP/本档状态更新 → 批次报告。

## 停止规则（同 05）

用户裁决门 → 记队列跳过；mutation REJECT 两轮 → 暂停上报；环境阻断 → 记录替代验证；新架构问题 → 记 ROADMAP §5 候选不当场改。

## 用户裁决队列（不阻塞自主面）

| 裁决 | 解锁 | 状态 |
|---|---|---|
| zero-IO 取向（trait 化收敛 vs 改写不变量） | 架构收口（本计划未排批，用户表态后补） | 待表态 |
| B6 major 确认（23 Retired 方法移除） | ROADMAP 1.1 | 待表态 |
| TUI 默认引擎翻转时机（P11 前最终确认） | P11 | 待表态（终验矩阵已全绿，具备翻转条件） |
| 协作域 Q1-Q5 | ROADMAP 1.7 | 待表态 |

---

## 终态判定（四面的"可用"定义）

**内核完整**：wire 写侧覆盖全部状态域（重启不丢 goal/cron/permission/plan/tool store）；缓存两个 BLOCKER 关闭且三桶可观测；shell runner 真实现；文件面（multipart）真实现。

**Web 可用**：能完成「登录（OAuth 或 token）→ 建/恢复会话 → 流式对话（文本+工具卡片+thinking）→ 附件上传粘贴与回显 → 终端面板 → 模型/provider 管理与模型选择器 → MCP 面板 → 审批/提问交互 → steer/打断/排队 → goal 条 → 归档恢复」全链路，事件驱动实时（非轮询兜底）。

**Desktop 可用**：Web 可用 + 代码块"在编辑器打开"（fs 链路）+ 全新用户登录路径 + 关窗后台/托盘 + 原生通知。自动更新/深链为增强项。

**ACP 可用**：合规 v1 客户端（按官方 schema 解析，如 Zed/JetBrains）能完成「initialize → 认证 → 建会话（cwd + mcpServers overlay）→ 流式对话（message/thought chunk + tool_call）→ 工具审批 → 标准取消 → close → load/resume 恢复历史」全程。

**TUI 可用**：`MIRRICODE_RUST_ENGINE=1` 下完成「对话（流式+工具卡片+todo 面板）→ `!` 命令 → Ctrl+B 转后台 → /btw → /compact（锁正常释放）→ /goal → /mcp → skills 自定义目录 → resume 历史回放 → 插件/审批」全链路；终态为切默认引擎。

---

## 差距总账（六路审计，2026-09-05）

### 引擎侧（审计 A 功能面 / C 缓存 / 写侧自核 / B 架构）

- **wire 写侧**：生产只写 9 种 op（Metadata/TurnPrompt/TurnEnded/ContextAppendMessage/ContextAppendLoopEvent/ContextApplyCompaction/ContextSizeMeasured/ContextClear/UsageRecord）；goal/cron/permission 规则/plan mode/profile bind/tool store 重启全丢（GoalManager `goal.rs:73`、CronManager `cron.rs:100` 纯内存）；reducer 的 goal/plan/cron 分支为死代码
- **缓存 BLOCKER**：Anthropic `cache_control` 全树 0 匹配（TS kosong `anthropic.ts:319/344-354/964/1048` 可移植）；usage 三桶贯通缺失（`openai.rs:256-277` 减法丢弃 cached_tokens，`cacheReadTokens/cacheCreationTokens` 恒 0）
- **缓存 DEGRADE**：取消路径 partial 丢弃无 interrupted 锚点（`run_turn.rs:112-122`）、图片 base64 内联无旁路、micro-compaction 禁用、无"摘要必须变小"校验、无 request/header 漂移事件
- **shell**：`shell.run` 无真 runner（`session_handlers.rs:1910` error-shaped stub；terminal_pool.rs 全仓未用）；无 shell.* 事件投影
- **task.detach** 未实现（`routes/task.rs:31-36`）
- **文件**：无 multipart 上传/下载 REST；`message.send` 仅纯文本
- **事件族缺**：subagent.*（swarm UI 依赖）、turn.step.*、goal.updated、skill.activated、cron.fired、mcp.server.status、session.meta.updated
- **provider**：429 quota 不分类（全 429 可重试）、错误码 18 vs TS 68、Kimi 视频上传、`model.generate` chunk 不转发 WS
- **管理面**：agent profile 只读、config.diagnostics 恒空、用户工具无 RPC 行、models.dev 目录导入缺、Notification hook 无 trigger
- **架构债**（不阻塞可用，收口留裁决后）：zero-IO 违反未跟踪、05 文档大面积 STALE-DOC、双 dispatch 栈分歧、transport 孤岛——完整清单见 ROADMAP §4.5 C 级

### Web（审计 D）

- **P0 客户端 bug（疑）**：事件前缀裂缝——引擎发 `event.assistant.delta`，投影器 switch 全是无前缀名且新路径无剥前缀逻辑（`ws-rpc-api.ts:407-418` vs `agentEventProjector.ts:663-1422`；旧路径 `ws.ts:422-456` 有剥）→ 若坐实全站实时性失效
- **纯客户端适配（零引擎改动）**：MCP 面板仍消费废弃 `mcp.global.*`（引擎已有 `mcp.config.list/set/remove/setToolFilter/test` + `mcp.status`）；`message.steer` 引擎有客户端未接
- **阻塞**：附件上传（`uploadFile` throw `ws-rpc-api.ts:1061-1067`）、媒体回显下载、终端面板（`terminal.*` 四方法全 throw）、Provider 管理 + 模型选择器进不去（`refreshAllProviders` 抛错）、models.dev 导入、OAuth device flow 登录、agent profile 写面（UI 静默失败）、steer/abort/排队/侧聊（`session.createChild` 引擎 gap 行）、openFile/revealFile
- **降级**：会话列表字段写死（busy/model/usage/messageCount）、归档态不可还原、状态面板中性默认、`userMessageId` 恒空、subagent/turn.step/cron.fired 事件收不到、goal 条 `getSessionGoal` 硬编码 null（引擎缺 `session.goal` 汇总 RPC）、workspace 增删假成功、workspace 版 skills 恒空

### Desktop（审计 D）

- 100% 复用 web SPA（`mirri-desktop/src/main/index.ts:123-146` 加载 `127.0.0.1:58827`），全部 Web 缺口继承
- **特有阻塞**：无 preload/IPC 桥（代码块"在编辑器打开"想本地兜底都无接缝，`index.ts:167-170`）；全新用户 OAuth 不可用（靠读 server.token 注入 `#token=`，前提是先在 CLI 登录过）
- **特有缺失**：托盘/关窗后台、自动更新（无 electron-updater/publish 配置）、原生通知/角标（关窗后静默）、单实例锁、深链 `mirricode://`、开机自启

### ACP（审计 F，官方 v1 schema 对照）

- **P0 阻塞**：① `session/update` 词汇是旧草稿方言（`agent_text_delta`/`turn_started`/`turn_ended`/`tool_call{state}`），不在 v1 `SessionUpdate` 联合（`agent_message_chunk`/`agent_thought_chunk`/`tool_call`/`tool_call_update`/`plan`/`current_mode_update`/`config_option_update`/`usage_update`/…）——且声称 protocolVersion=1 属版本谎报；② permission 桥缺失（无 `session/request_permission` 反向通道，工具审批阻塞到 300s 超时挂死）；③ mcpServers 完全不读（D6 裁决的 Session Overlay 引擎侧也无槽位）；④ `session/new` 忽略 cwd（v1 required）；⑤ `session/load` 无历史重放、响应缺 configOptions/modes；⑥ 无 `$/cancel_request`
- **P1 降级**：authenticate/logout 桩（会话方法无鉴权门槛）、session/list 只列 running（跨进程恢复断链）、set_mode/set_config_option 参数名旧（`mode` vs `modeId`）、tool.result 无 name + tool_call 缺 content/rawInput/kind/locations、更新通道 256 容量 lag 静默丢包、session/delete 假成功、prompt 只抽 text 块但广告 image:true（不诚实）
- **P2 增强**：thinking delta、slash commands/available_commands、plan/usage/session_info、clientCapabilities 协商

### TUI（审计 E）

- **P0 阻塞（状态机卡死类）**：SDK rust 事件翻译层只放行 2 种事件（`translate.ts:123-167`，`assistant.delta` + failed `turn.ended`）→ turn 完成后 UI 永停 composing、工具卡片/todo 面板不渲染、`/compact` 后 isCompacting 锁死输入、队列 drain 停摆；`agentId` 恒 'main'（/btw 面板按 uuid 路由永不命中）
- **P0 阻塞（显式失败类）**：`!` bash 命令（引擎 shell.run stub）、Ctrl+B detach（引擎 task.detach）、`btw.start` 返回 error-shaped 对象与 SDK `Promise<string>` 契约冲突（`session_handlers.rs:1920-1927`）、`addAdditionalDir` stub 清空列表但 TUI 显示成功
- **降级**：`skillDirs` 构造参数被 rust 客户端静默忽略（`sdk-rpc-client-rust.ts:423-441`）、resume 返回形状与 v1 `ResumedSessionSummary` 兼容性**未验证有崩溃风险**、goal 事件无、cron 展示无、MCP 状态行无、config 警告无、429 提示不友好
- **已通**：会话管理/fork/undo/export、配置读写、审批/提问（reverse-RPC 已接线）、插件管理、文件编辑器、主题/通知（审批类）、剪贴板

---

## 批次队列（P0 → P11）

> 排序原则：客户端事件通道止血（三处，解锁全站实时性）→ 引擎共享缺口（一批解锁多端）→ ACP 独立纵切 → 管理面 → 事件族回填 → 缓存 DEGRADE → Desktop 壳 → 视图面 → TUI 切默认 + 终验。每批进 brief 重估，允许小调整不允许静默扩批。

### P0 — 客户端事件通道止血（3 子批）

**P0a Web 事件前缀验证与修复**（疑全站实时性第一问题）

- [ ] 实测验证：rust serve + web dev，断言 `event.assistant.delta` 是否落入投影器 default no-op（`agentEventProjector.ts:1419-1422`）
- [ ] 修复：`ws-rpc-api.ts` 事件入口剥 `event.` 前缀（或投影器前缀容忍），对齐旧路径 `ws.ts:422-456` 行为
- [ ] TDD：vitest 构造带前缀事件帧过 projector，断言全事件族投影生效（turn/tool/compaction/approval/question/background.task）

**P0b SDK rust 事件翻译层补全**（TUI 第 1 阻塞）

- [ ] `packages/node-sdk/src/rust-engine/translate.ts`：放行 `turn.started`、`turn.ended`（全部 reason：completed/cancelled/blocked/filtered/failed）、`tool.call.started/delta`、`tool.result`、`tool.progress`、`thinking.delta`、`agent.status.updated`、`hook.result`、`compaction.started/completed/cancelled/blocked`、`background.task.started/terminated`、`message.received`
- [ ] `turnId`/`agentId` 映射修正（引擎 wire 的 turnId 字符串 → v1 数字约定，或 TUI 侧容忍）
- [ ] TDD：对引擎 `event_projection.rs` 的 26 种投影逐种构造帧断言翻译产物；e2e：rust 引擎一轮对话后 TUI streamingPhase 复位 idle

**P0c Web 快速接线**（纯客户端，零引擎改动）

- [ ] MCP 面板 `mcp.global.*` → `mcp.config.list/set/remove/setToolFilter/test` + `mcp.list/status`（`ws-rpc-api.ts:1185-1273` 重写，`McpServersPanel.vue` 适配）
- [ ] `steerPrompts` 接 `message.steer`（`ws-rpc-api.ts:610-616`）
- [ ] TDD + Playwright：MCP 设置面板增删启停全链路

**解锁面**：Web 实时渲染恢复；TUI 状态机复活；Web MCP 管理可用。

### P1 — 缓存 BLOCKER ×2

**P1a Anthropic cache_control**（✅ 已落地 `9aed9fa3`，评审 APPROVE-WITH-FOLLOWUPS，agent-81）

- [ ] `mirri-kosong/src/anthropic.rs` 请求构造三处打点（系统末块/工具数组末项/末条消息末 block），移植 TS `anthropic.ts:319/344-354/964/1048` 的 CACHEABLE_TYPES 与注入条件
- [ ] TDD：wiremock 断言请求体 cache_control 位置形态；mutation：删一处打点测试红

**P1b usage 三桶贯通**（✅ 已落地 `aceb4ba9`，2026-09-05）

- [x] 解析层保留缓存数：`openai.rs`/`openai_responses.rs`/`google.rs`（cached_tokens 减法后保留存入 cache-read 桶）、Anthropic dispatcher `message_start`/`message_delta` 读 `cache_read_input_tokens`/`cache_creation_input_tokens`
- [x] `LlmUsage` 扩 `cache_read_tokens`/`cache_creation_tokens` → `turn_step.rs` UsageRecord 写全 → `agent_scope.rs` resume 初始化（`UsageInitialState`）→ embedded convert 泵 + `message.send` turn_end + `model.generate` usage 全输出
- [x] DeepSeek `prompt_tokens` 包含式计数陷阱确认（减法后保留）；**压力公式保持 input+output 不变**（缓存桶不计入 ContextMemory 压缩压力，公式语义变更记 followup F7）
- [x] TDD：kosong 三 provider + embedded 端到端 wiremock 断言贯通到 wire op 与上报面；修掉 shared.rs 消费侧聚合 replace→非零胜出合并（原实现会把 message_start 的 input 侧清零）

### P2 — wire 写侧持久化纵切（4 子批按域）

- [x] **P2a goal + plan mode**：GoalManager/plan 状态变更 record_op（goal.create/update/clear、plan_mode.enter/exit/cancel）→ reducer 状态化 → resume 重建 → e2e（创建→变更→close→resume→断言存活）（✅ 已落地 `79591a76`，mutation 12/12 红，评审 APPROVE-WITH-FOLLOWUPS（followups F11-F15 见 ROADMAP §4.5））
- [ ] **P2b cron**：~~CronManager 持久化（cron.add/delete/cursor）~~ ✅ 已落地 `3a408687`（写侧三变体集中在 `CronManager::add/remove/check_due`，write-before-mutate；reducer 状态化 `AgentState.cron_tasks`；resume 重建 `restore_tasks` 重算 next_fire_at、fired one-shot 休眠不重发；mutation 2/2 红，workspace 3269 绿 + clippy 零告警）。剩余：确定性 jitter（对齐 TS `app/cron/jitter.ts`）+ `cron.fired` 事件投影归 P7
- [ ] **P2c permission 规则**：~~`permission.rules.add`/`record_approval_result` 写侧 → "always allow" 重启存活~~ ✅ 已落地 `d9bec81c`（`agent.setPermission` 写 `permission.set_mode`；审批 ask 解析时记录 `permission.record_approval_result`（含 toolName/action/sessionApprovalRule/result），`scope:"session"` 的 approved 同步落 `PermissionManager` 会话审批集——审计实因"approve for session 决策未接线 record_approval"已顺手接上；reducer 状态化 `state.permission.session_approval_patterns`（去重，兼容 rust migration 的 `{rule,toolName}` 对象形态）；resume 灌回审批集，同 pattern 不再询问；`permission.rules.add` 按 TS v2 语义保持 transient 不持久化。mutation 3/3 红，workspace 3290 绿 + clippy 零告警）。注：`approval.decide`/`interaction.respond` 网关通道暂不携带 scope（`ApprovalDecision` 枚举无 scope 字段），session 审批目前仅经 reverse-RPC 应答路径
- [ ] **P2d profile bind / tool store**：~~`tools.set_active_tools`/`register_user_tool`/`update_store`、`profile.bind`/`config.update` 写侧与重建~~ ✅ 已落地 `cc3e4b51`（`agent.setActiveTools`/`registerTool`/`unregisterTool` 各 write-before-mutate 记录 `tools.*` ops；reducer 状态化 `state.tools.active`（Option，None=默认全开）+ `user_registrations`（按 name upsert/remove）+ `state.profile_name`（`profile.bind` seed active 集 / `config.update` 更新）；resume 把 active 集灌回 tool_manager、经 `resume_session_with_config` 基座回调重放用户工具注册（embedded `session.resume` 透传 reverse-RPC callback）。mutation 3/3 红，workspace 3290 绿 + clippy 零告警）。边界内不做的：`select_tools` 工具是渐进披露面（TS ephemeral 不持久化）；MCP 工具注册由宿主/config 驱动、resume 时重供给；`tools.update_store` rust 运行时无 KV store 面；profile 运行时无写面（load-only），仅 reducer 状态化
- [ ] 每域 mutation：删 record_op → resume 断言红（P2a 12/12、P2b 2/2、P2c 3/3、P2d 3/3 已验）

**解锁面**：内核"重启不丢状态"；Web goal 条/TUI goal 队列的持久基础；cron 全端可用。

### P3 — shell runner + PTY + task.detach

- [x] `shell.run` 真 runner：`terminal_pool.rs` 接线（或按 mirri-os pty trait 重构）；`session_handlers.rs:1910` stub 移除；`shell.output`/`shell.started` 事件投影；`shell.cancel` 可用
- [x] `task.detach`：前台→后台转换（pendingOutput 落盘 + detachTimeoutMs + `ForegroundTaskReleaseReason`，对齐 TS `agent/task/task.ts` 语义）
- [x] TDD：PTY 会话 e2e（create/write/resize/close）+ detach 转换语义
- [x] Web 终端面板接线（`useTerminal.ts`/`Terminal.vue` 对 `terminal.*` RPC——按 TS kap-server `routes/terminals.ts` + WS attach 形态）

✅ 已落地（P3i `1d80127f`：`shell.run` 前台窗口 + 超窗/`task.detach` 转 Bash 同族后台任务（RPC 答布尔 `backgrounded`，task id 走 `shell.started` 事件）；`shell.cancel` 杀前台；`shell.output`（volatile）/`shell.started`/`shell.completed`（durable）事件投影扩到 29 词表 + 配套 context messages；node-sdk 契约 fixtures、rust-engine 翻译器、web projector、ACP pump 四端同步。P3ii `f7a8e82f`：SessionScope 挂 session 级 `TerminalPool`（真 PTY + 可重放输出环，session close 杀全部终端）；统一 RPC 端点五个连接本地 `terminal_attach/input/resize/detach/close`（attach 先 ack → `since_seq` 重放 → 挂 live sink，重 attach 先装新再卸旧）+ `terminal_output`/`terminal_exit` 通知；REST `GET/POST /sessions/{id}/terminals` + `GET .../{tid}` + `POST .../{tid}/close`（close 用 `/close` 子路径——axum 0.8 路由语法不支持 v1 `:close` 后缀；`capabilities.terminal:true`）；web `WsRpcApi` 真实现（五个 fire-and-forget 控制帧 + 通知路由到 `onTerminalOutput/onTerminalExit` + REST CRUD 映射 `AppTerminal`，`useTerminal.ts` 零改动）。门禁：workspace 3309 绿 + clippy 零告警 + web 691 绿；mutation 2/2 红（attach sink 删除 → 流/退出双测红，还原绿）。评审完成（agent-90，APPROVE-WITH-FOLLOWUPS，评审员独立复跑 3309/0 + clippy 零告警 + web 691/691 + node-sdk 352/0，M1-M3 杀、M4 豁免裁决 + 扩展验证；F-P3-1..6 已收编 ROADMAP §4.5））

**解锁面**：TUI `!` 命令与 Ctrl+B；Web 内嵌终端；Desktop 终端（继承）。

### P4 — 文件面（multipart + 多 part 消息）

- [x] 引擎：`POST /api/v1/files` 上传（multipart 解析）+ `GET /api/v1/files/{id}` 下载（blob.rs 扩展为可寻址存储）；`message.send` 多 part content（图片 part 进 blob）
- [x] Web：`uploadFile`/`getFileUrl`/`getFileBlob` 实接（`ws-rpc-api.ts:1061-1073`）；附件条（选择/粘贴/拖拽 `useAttachmentUpload.ts`）+ 历史媒体回显（`MessageImage`/`AuthMedia.vue`/`useFilePreview`）；engine caps `file_upload:true`
- [x] TDD：上传下载往返 e2e + 多 part 消息发送渲染

✅ 已落地（P4 `bce286c7`：`FileStore` 落 mirri-persistence（`f_` id + index.json 元数据 + blob 文件，v1 TS FileService 语义含 expires_at 记录性字段与失联 prune）；REST `POST /api/v1/files`（手写 boundary 解析零新依赖、单文件、`DefaultBodyLimit::disable()` 对齐 TS 不设上限）+ `GET /api/v1/files/{id}`（content-type/disposition/etag + Range 206，404 走错误信封）——与 message.send 入口解析共享 `<mirri-home>/files` 根（CLI serve 双端显式注入）。`ContentPart` 增 `file` 变体（入站 file_id 引用形 + context memory 的 data URI 内联形），`message.send`/`message.steer` 的 `input` 接受 v1 TS REST 拼法（text/image/video 三种 source kind/file part），入口解析 file_id → data URI（媒体类型取自 files 元数据，绝不把裸 file_id 存进 context memory），wire 记录经 `AgentBlobService::offload_parts` 落 blobref（File part 的 offload/load 对称扩展），resume 重水化链零新增恢复逻辑。web `submitPrompt`/`steerSession` 从 flatten 文本改为结构化 parts（`promptContentToInputParts` 映射 + vitest），`uploadFile` 走新 REST，caps `file_upload:true`。门禁：workspace 3346 绿 + clippy 零告警 + web 697 绿；mutation 2/2 红（入口解析删除 → 解析/resume 双测红，还原后 cmp 一致；web 映射分支删除 → 5 断言红，还原后 23 绿）。评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-92，2026-09-05，独立复验一致；followup F-P4-1~8 收编 ROADMAP §4.5））

**解锁面**：Web/Desktop 附件与图片粘贴；多模态输入基础。

### P5 — ACP v1 合规纵切（6 子批对应 F 审计 P0）

- [x] **P5a update 词汇迁移**：`translate_engine_event` 重写为 v1 `SessionUpdate` 联合（`agent_message_chunk`/`agent_thought_chunk`/`tool_call`+`tool_call_update`），载荷 ContentBlock 结构；conformance 测试**对照官方 schema 重写**（当前自洽测试作废重立）
- [x] **P5b permission 桥**：serve 层反向 request/response 通道 + `session/request_permission` + approval/question 事件接线 + 失败兜底拒绝（参照 TS `interaction-bridge.ts`）——question→elicitation 深映射按派单边界记 followup（ask 干净报错不挂死）
- [x] **P5c mcpServers overlay + cwd**：ACP 面读取 initialize/session/new 的 mcpServers → 引擎新增 per-session ephemeral overlay 槽位（随 session 生灭、遮蔽三池同名，D6 裁决落地）+ `session/new` 支持 `cwd`
- [x] **P5d load 重放 + 配置面**：`session/load` 历史批量重放（update 先于响应 settle）；new/load/resume 响应带 `configOptions` + `modes`；set_mode/set_config_option 迁移 `modeId`/`configId` + 返回 configOptions + `current_mode_update` 通知
- [x] **P5e $/cancel_request + 通道**：标准取消路由到 prompt 取消路径（request-id 注册表 → 与 `session/cancel` 同路径：engine `message.abort` + in-flight request_permission 回 `Cancelled` outcome（F-P5b-4 收口）+ prompt 响应 settle v1 `cancelled`）；更新通道 256→1024 + Lagged 时 `tracing::warn!`（裁决 B：不做有界阻塞——背压级联拖死引擎 turn 比丢 UI chunk 严重，wire log 永不丢 + session/load 重放兜底）
- [x] **P5f 诚实化**：✅ 已落地（`5220e0b4` 引擎面 media-kind 校验 F-P4-2 + `fc73749c` 六主域 +2647 行）：auth gate 真查 mirri-auth 凭据（new/load/resume/fork/authenticate 五处 gate + logout 真删 token + AuthMethod 广告逐字段对齐 TS，广告登录面未实现记 F-P5f-4）；session/list 磁盘枚举停止自筛 running（v1 SessionInfo + `_meta` 扩展、cwd canonicalize 过滤、nextCursor 诚实 null）；session/delete 真删（引擎 archive + 宿主 remove_dir_all 会话目录，remote 降级 warn 不假成功）；prompt image → image_url data URL + MIME 校验（压缩未实现记 F-P5f-1、audio/blob warn-drop 广告 false、embeddedContext:true 裁定成立）；clientCapabilities 协商记录 + question.ask→elicitation/create 深映射（F-P5b-1 收编，418 行）；fork 挪 `_meta.unstableSessionCapabilities`（与 TS/schema 广告位分叉记 F-P5f-8）；G7 内容面 CREATE kind/rawInput/content + 终态 name。门禁：workspace 3429/0/10（+34）+ clippy 零告警；mutation M1-M7 全红。评审 APPROVE-WITH-FOLLOWUPS（agent-104，2026-09-05：mutation M1-M4 独立重做全红 + 恢复 cmp 字节一致、门禁复核与自报分毫不差、13 项自报偏离全部属实、同域收编 F-P5a-1/F-P5c-1/F-P5d-1/F-P5b-1/F-P5b-5/F-P5d-2/F-P4-2 核验通过、F-P5b-3 正当未收编；F-P5f-1..9 收编 ROADMAP §4.5，F-P5f-6 撤销）
- [ ] 验收：用官方 SDK（schema 校验）+ 一个真实 ACP 客户端（Zed 或 JetBrains）做冒烟

✅ 已落地（P5a `7a3bc61b`：`translate_engine_event` 全量迁移到官方 v1 词汇——`agent_message_chunk`/`agent_thought_chunk`（text ContentBlock）、`tool_call` CREATE（title=name、`status:"in_progress"`、kind/content 按 P5f 边界缺省）、`tool_call_update` 终态（`completed`/`failed`，status 型 progress → title 刷新），旧方言（`agent_text_delta`/`turn_started`/`turn_ended`/`state`/`toolName`/`output`）删除、turn 边界走 prompt 响应。conformance 对照官方 schema 校验：`@agentclientprotocol/sdk@1.3.0` schema.json 字节级快照 vendor 到 `tests/fixtures`（`$defs` 重根到 `SessionNotification` wrapper 一次编译进 `OnceLock`），mock turn（含 yolo Bash 工具轮）逐条校验 + 4 变体 positive + 旧方言反向守卫；`MockLlm` 增可选 thinking 脚本使 thought chunk 可端到端驱动。门禁：workspace 3350 绿 + clippy 零告警；mutation M1-M4 全红后还原。评审 APPROVE-WITH-FOLLOWUPS（agent-94，2026-09-05：四禁区零越界、红线 1-7 全过、mutation 独立重做全红后恢复 cmp 一致、fixture 与主区 node_modules schema cmp 字节一致、独立复跑 3350/0 + clippy 零告警、自报 5 偏离逐条核实属实；F-P5a-1..3 已收编 ROADMAP §4.5））

✅ 已落地（P5b `76f48645`：`approval.ask` 反向 RPC 桥到 `session/request_permission`——AcpHandler 经 `EngineAdapter::on_reverse_rpc` 懒注册桥，approval 载荷映射官方 `RequestPermissionRequest` 形状（sessionId 取 in-flight prompt 会话、toolCallId=引擎 tool_call 卡片同 id、canonical 3 options `approve_once`/`approve_always`/`reject`，kind `allow_once`/`allow_always`/`reject_once`，legacy `approve`/`approve_for_session` 别名兼容）；decision 映射对齐 TS approval.ts（`approve_always`→approved+scope:session 装 session allow rule、reject/未知/cancelled/畸形→拒绝侧）；**失败兜底一律 rejected**（60s 超时/断连/畸形回复，早于 300s ENGINE_TURN_TIMEOUT，绝不挂死）。stdio 传输重构为泛型 `JsonRpcChannel` 带 pending-request 表（id→oneshot，超时清表/断连排空，recv 拦截匹配响应就地结算）；serve 循环每请求独立 task（wait-mode prompt 不再阻塞反向 RPC 泵——旧串行结构必死锁）+ outbound 泵臂。conformance §H 5 场景（allow/allowAlways 二轮不再问/reject/不响应兜底/双通道 gate 先赢）+ transport 6 单测 + 映射 3 单测，请求载荷逐条过 vendored 官方 schema 校验。门禁：workspace 3364 绿 + clippy 零告警；mutation M1-M3 全红后还原。`question.ask` 未桥接（按派单边界，ask 干净报错不挂死，elicitation 记 followup）。报告：`.superpowers/sdd/parity-completion/task-p5b-report.md`）
✅ 已落地（P5c `ddf5ad4a`：`session/new` 读 `cwd` + mcpServers Session Overlay——**cwd = per-cwd EmbeddedServer 实例池**（AcpHandler `cwd_engines` canonicalize 键控 + `EngineFactory` 注入面，main.rs 装配层建池，默认 root 复用默认引擎；session→engine 路由表覆盖 prompt/close/delete/fork/load/cancel/set_*，session/list 跨引擎合并按 id 去重；cwd 非绝对/不存在 → invalid_params）——不动引擎 session 级 cwd，workspace 级 FS/permission 安全模型不变。**overlay = 第四种 MCP 来源**（D6）：`EmbeddedServer.session_overlays`（manager + 配置名集，名字级遮蔽在连接就绪前即生效，**overlay > workspace > user**），session.create/resume 透传 `{name, config}` 条目、在 register_mcp_tools 之前 prepare；连接后台 spawn（响应不等就绪，per-server 失败 warn-drop 不 fail 请求），settle 后代际检查防 re-register 竞态；close/archive 注销（disconnect_all，子进程消亡）；**不持久化**——load 时客户端重传、每次请求重注册。v1 转换（`mirri-acp/src/mcp_servers.rs`）：**stdio 以 `type` 缺席识别**（schema 四变体中仅 stdio 无判别），http/sse headers、stdio env 数组投影 record，acp/未知/畸形 warn-drop，重名 last-write-wins（mcp.ts 对齐）。conformance §I 8 测试（cwd 校验/工具可调用/close+delete 注销/同名遮蔽/per-cwd 路由+复用/acp warn-drop/load 重注册）+ 转换 5 单测，测试用 `mirri-acp-echo-mcp` 测试 bin（stdio echo MCP server，工具名 argv[1] 区分实例）。门禁：workspace 3377 绿 + clippy 零告警；mutation M1-M4 全红后还原。报告：`.superpowers/sdd/parity-completion/task-p5c-report.md`）

**解锁面**：ACP 对合规宿主完整可用。

### P6 — 管理面（provider/auth/profile/config）✅

- [x] ~~Provider CRUD~~ → models.dev 目录导入 + `refreshProviderModels`（scope 过滤、10s 超时、失败降级不报错、磁盘缓存回填、model.list overlay merge）
- [x] OAuth device flow：引擎 `auth.*` 已有（P5f `fc73749c` 诚实化 + gate/list/delete/image/clientCapabilities+elicitation 收口）
- [x] Agent profile CRUD：`profile.rs` 写面（upsert/delete 落盘 `<mirri-home>/profiles/*.toml` + builtin-override merge）+ 5 RPC（agent.listProfiles/getProfile/createProfile/updateProfile/deleteProfile）+ 5 REST 路由（`GET/POST /api/v1/agents`、`GET/PUT/DELETE /api/v1/agents/{name}`）+ essential/builtin 保护
- [x] `config.diagnostics` 真实现（三类悬空检测：default_provider/模型 alias/旧 [model] 迁移提示 + models.dev 降级 warning）
- [ ] 用户工具 RPC 行（`register_user_tool` 已有 engine 侧能力，补 RPC + 契约链）——独立子项未入本批 brief
- [x] TDD：CRUD 往返全字段 roundtrip 专测 + embedded 11 测 + http 3 测 + cross_backend wire-shape 对照

✅ 已落地（`aa190e43`，25 文件 +2537/−82，workspace **3454/0/10**（净 +25）+ clippy 零告警 + node-sdk 483/0；契约链五件套同步：registry 总 191/emb 177/srv 165/both 151、TSV +5、cross_backend pin 165 + wire-shape 测试、embedded route pin 177、node-sdk fixture 177 + getHistory snapshot；顺手修复 P5d 遗留 fixture 漂移（session.getHistory pin 176→177）；mutation M1-M3 红还原 cmp 一致；评审 agent-107 APPROVE-WITH-FOLLOWUPS——新 F-P6-7（Zero-IO 写侧裁决）/F-P6-8（大小写变体）/F-P6-9（scoped 计数失真）、F-P6-4 升 medium；报告 `.superpowers/sdd/parity-completion/task-p6-report.md` + `task-p6-review.md`）

**解锁面**：Web 设置页/模型选择器/登录；Desktop 新用户路径；TUI /agents 启停写面。

### P7 — 事件族与交互补全

- [x] 引擎事件投影补齐：`subagent.spawned/started/suspended/completed/failed`（swarm UI）、`turn.step.started/completed/interrupted/retrying`、`goal.updated`、`skill.activated`、`mcp.server.status`、`session.meta.updated`、`plugin_command.activated`、`cron.fired` — ✅ `60fe1642`（P7a，29→44 方法三消费端同步 + CronManager 首个生产 driver：30s tick 注入 cron_job origin turn；goal snapshot Complete→"complete" 修正；subagent.suspended/plugin_command.activated 按边界只做投影+消费端；goal/skill 工具侧与 workspace 级 mcp 广播记 followup，见 task-p7-report.md）
- [x] `session.createChild`（引擎 gap 行 → 实现）；web 侧聊/abort 接线 — ✅ `71c37bb6`（P7b：引擎三件均已有实现——createChild 为 B1 fork、会话级 abort = `message.abort` 无 turnId、steer 真实现但**整 turn 持 flow 锁致 mid-turn 死锁/迟到**，本批经共享 SteerBuffer handle 修复 + web abortSession/createChildSession 接线；mutation M1 红绿闭环，见 task-p7-report.md）
- [x] Web 会话列表字段回填（busy/model/usage/messageCount/lastSeq——engine SessionView 扩展）；归档态字段；状态面板真实数据（`session.status` 扩展）；`session.goal` 汇总 RPC（goal 条） — ✅ `5e6263aa`（P7c：双后端 SessionView 五运行时字段 + F5 live usage 进 usage_service + session.status 三新字段 + web toAppSession/getSessionStatus/getSessionGoal 真消费——getSessionGoal 直连既有 goal.get 未新增 RPC；archived 态记 followup F-P7-5，见 task-p7-report.md）
- [x] TUI `skillDirs` 透传（`sdk-rpc-client-rust.ts` 构造接引擎）；workspace 版 skills — ✅ `d1b65c06`（P7d：`createEngine(homeDir, skillDirs)` NAPI 扩参——非空替换默认 user/project 发现为 User 源（v1/v2 同契约）；brand 目录对齐 TS scanner（`<root>/.mirri-code/skills` project + `<mirri home>/skills` user-brand）；web `listSkillsForWorkspace` 读既有 `skill.list`（workspace 级 dispatch）不再恒空；门禁 workspace 3473/0 + clippy 零告警 + node-sdk 494/73s + web 710/0，mutation 3 红绿闭环，见 task-p7-report.md）
- [x] **resume 形状专项验证**：rust `session.resume` 返回 vs v1 `ResumedSessionSummary`（agents.main.replay/toolStore/background），不兼容则引擎补形状——TUI 历史回放有崩溃风险，必须实测 — ✅ `7225ce37`（P7e：resume/reload/restore 统一返回 v1 ResumedSessionSummary（summary 标量 + per-agent snapshot——config/context/replay/permission/plan/swarmMode/usage/tools/background）；视图侧改名 `toolCalls/toolCallId/isError` 并摊平 v1.0 嵌套 OpenAI tool call 到 v1.1 顶层（持久化 wire log 拼写不动）；create 期 permissionMode 补 `permission.set_mode` wire op 活过 resume；跨后端 session.restore 真分歧以 serve 字面量 pin 记录；门禁 workspace 3473/0 + clippy 零告警 + node-sdk 495/73s + web 710/0，mutation 2 红绿，见 task-p7-report.md）
- [ ] TDD：每事件一条发射断言 + relay/Web/TUI 端到端 — ✅ P7a 部分（8 条 embedded 族 e2e + 8 条 SDK translate 族测试 + 2 mutant 验证）

### P8 — 缓存 DEGRADE + provider 体验收口

- [x] 取消路径闭合：~~`run_turn.rs:112-122` abort 分支~~ ✅ `9cabb41c`（方式 B：partial 冲刷 + `[Request interrupted]` 锚点 + pending/open steps/deferred 闭合 + wire 记录，下个请求含已交付前缀）
- [x] micro-compaction 修剪启用 + "摘要必须变小"校验 ✅ `9cabb41c`（KEEP_RECENT=20/MIN_TOKENS=100 常量、pruned 复检跳过摘要调用；`apply_compaction` 拒绝变大摘要，`CompactionResult.applied` 门控 wire 写点）
- [x] request/header 漂移事件 ✅ `9cabb41c`（`request.header.changed` op：tools_changed/media_projection_degraded/media_projection_stripped/compaction_applied，8 写点 + reducer `request_header` 审计态）
- [x] 429 quota 分类 ✅ `bdc27700`（P8b-1：`provider.quota_exhausted` 不可重试 + retryAfterMs，Kimi 计费词表对齐 TS kimi-errors.ts）
- [ ] `btw.start` 返回真实 agentId；`addAdditionalDir` 真实现或诚实失败 — ❌ 时间窗耗尽记 F-P8-1/F-P8-2（addAdditionalDir 侦察完毕：local.toml `workspace.additional_dir` 语义，见 task-p8-report.md D6）
- [ ] Kimi 视频上传通道 + `model.generate` chunk WS 转发 — ❌ 记 F-P8-3/F-P8-4
- [ ] 图片附件旁路（体量大：ContentPart::ImageUrl base64 → 附件存储 + provider 从存储读取 + 退化链前缀交互设计）——❌ 记 F-P8-5

### P9 — Desktop 壳补齐

- [x] preload/IPC 桥：~~`fs:open`/`reveal`/`open-in` 引擎面 + Electron `shell.openPath` 本地兜底~~ ✅ `186fe386`+`d266ca1b`（`mirriDesktop` contextBridge 面（openPath/revealPath/openInEditor/openInApp/openExternal，全部经 main 侧校验后 shell.*/spawn）+ web 三态降级链：桥优先（会话根解析绝对路径）→ 引擎 fs.open/fs.reveal（/meta capabilities 声明才走，rust 引擎今日不声明）→ ExternalOpenUnavailableError 哨兵 + 复制路径降级提示；引擎面 fs:open 未实现，理由与 D-P9-1 记录见 task-p9-report.md）
- [x] 托盘 + 关窗后台常驻；原生通知/角标（关窗后可达）；单实例锁（`requestSingleInstanceLock`） ✅ `3c39063f`（close→hide 托盘常驻全平台 + 托盘菜单显示/自启/退出；主进程 Notification + badge/dock-bounce IPC + web 未读角标 watch；单实例锁聚焦已有窗口，ensure-server 端口/stale-kill 未动；`mirricode://` 深链 + 自启 `setLoginItemSettings` 同 commit）
- [x] 自动更新（electron-updater + publish 配置——**分发渠道属用户裁决项，先做技术就绪**）；深链 `mirricode://` ✅ `3c39063f`（electron-updater 动态 import + `MIRRICODE_DESKTOP_UPDATE_URL` env 门 + 打包门禁双条件，publish generic 占位；深链 session/<id> 三来源（open-url/second-instance argv/冷启 argv）→ web openSessionFromDeepLink 切会话）
- [ ] TDD/验收：打包产物本地冒烟（openFile 链路、托盘、通知） — ❌ 单测/降级链三态覆盖已绿（desktop 123 + web 736），打包冒烟记 F-P9-1（electron-builder 全量构建耗时长，非阻塞待办）

### P10 — 视图与搜索面

- [x] 全局消息搜索（`session.search` literal 读面 + Web 入口服务端化） ✅（brief 重估范围落地：全扫描 NFKC+大小写折叠 literal 匹配 + fingerprint cursor 分页 + persisted-only 会话只读回放扫描；`terms` 模式接受即降级 literal（F-P10-1）、tool call 内容不入索引（F-P10-2）；transcript 端点与 guiStore 经重估砍掉——web 无 UI 消费者、rg 全仓无此符号，B13 审计项关闭；web SearchSessionsDialog 非空 query 走服务端搜索、空 query 最近列表不变、非 rust transport 保持客户端过滤）
- [x] 按真实 UI 消费决定范围，防过度建设 ✅（重估结论见 task-p10-brief.md：仅落 rust RPC + web 消费闭环）

### P11 — TUI 切默认 + 四面终验矩阵（用户最终确认后）

- [x] **切默认前置核对**：E 审计前置清单 1-12 全关（E1/E12 静态 + E2-E11 定向复跑全绿，2026-09-06，swap 1.59GB 合规；证据归档 `.superpowers/sdd/parity-completion/task-p11-e-checks.md`）；`@mirri-ai/mirri-node` 原生绑定本机构建 + 真实 RPC 往返冒烟通过（darwin-arm64）
- [ ] `MIRRICODE_RUST_ENGINE` 默认翻转（experimental flag 出注册表或 default: true）——**用户确认门**
- [ ] **终验矩阵**（每面留证据归档 `.tmp/` 或 reports/）：
  - Web：Playwright 全链路（登录→会话→流式→附件→终端→管理面→审批）
  - Desktop：打包产物冒烟（服务器拉起→登录→对话→openFile）
  - ACP：官方 schema conformance 全绿 + 真实客户端冒烟
  - TUI：交互冒烟全清单（对话/`!`/Ctrl+B/btw/compact/goal/mcp/skills/resume/插件）
  - 内核：`cargo test --workspace` 全绿 + 缓存三桶数值实证 + 重启状态存活实证

---

## 批次推进状态

| 批次 | 内容 | 状态 | 完成 commit |
|---|---|---|---|
| P0a | Web 事件前缀修复 | ✅ | `48017740`（评审 APPROVE-WITH-FOLLOWUPS） |
| P0a-fu | P0a followup：resolution 事件 camelCase 读取、decide NotFound 幂等、停靠路径测试、reverse-RPC 注册时机 | ✅ 评审 APPROVE（M4-M6 全杀死，675 web 测试绿） | `796415cd` |
| P0b | SDK rust 事件翻译补全 | ✅ 评审 APPROVE（4 mutation 全红） | `a823e632` |
| P0c | Web mcp.config/message.steer 接线 | ✅ 评审 APPROVE-WITH-FOLLOWUPS（M1-M3 全杀死；F1-F3 见 ROADMAP §4） | `5e13f941` |
| P1a | Anthropic cache_control | ✅ 评审 APPROVE-WITH-FOLLOWUPS（M-A~M-D 全红；F-1 四断点上限 / F-2 连续 user 消息未合并 / F-3 多块 system / F-4 thinking 无回溯） | `9aed9fa3` |
| P1b | usage 三桶贯通 | ✅ 评审 APPROVE-WITH-FOLLOWUPS（M-A~M-E 5 必做 + M-F/M-G 加测全红；契约链 28 环闭合；HEAD 复现 kosong/proto/core 1332/embedded 297 全绿 + clippy 零告警；followup F5-F10 见 ROADMAP §4） | `aceb4ba9` |
| P2a-d | wire 写侧持久化（goal/plan/cron/permission/tool store） | ✅ 全部落地：P2a `79591a76`（goal/plan 两域写侧 + resume 重建 e2e；mutation 12/12 红，workspace 3247 绿 + clippy 零告警，评审 APPROVE-WITH-FOLLOWUPS，followups F11-F15 已收编 ROADMAP §4.5）；P2b `3a408687`（cron 域写侧三变体 + resume 重建；mutation 2/2 红，workspace 3269 绿 + clippy 零告警，评审 APPROVE-WITH-FOLLOWUPS，followups F16-F20 已收编 ROADMAP §4.5）；P2c `d9bec81c`（permission mode + session approvals 写侧/状态化/resume 灌回；mutation 3/3 红，评审 APPROVE-WITH-FOLLOWUPS（agent-88，mutation 3/3 杀，followups F21-F27 已收编 ROADMAP §4.5））；P2d `cc3e4b51`（tools active 集 + 用户工具注册写侧/状态化/resume 灌回 + profile bind 状态化；mutation 3/3 红，评审 APPROVE-WITH-FOLLOWUPS（agent-88，mutation 3/3 杀，followups F21-F27 已收编 ROADMAP §4.5））。批次尾门禁：workspace 3290 绿 + clippy 零告警 | `cc3e4b51`（P2d） |
| P3 | shell runner + detach + Web 终端 | ✅ 已落地：P3i `1d80127f`（shell.run 前台窗口 + detach 转后台任务 + shell.* 事件投影四端同步）；P3ii `f7a8e82f`（session 级 TerminalPool 真 PTY + terminal_* WS 帧/通知 + 4 条 REST 路由 + web WsRpcApi 真实现）。门禁 workspace 3309 绿 + clippy 零告警 + web 691 绿，mutation 2/2 红。评审 APPROVE-WITH-FOLLOWUPS（agent-90；F-P3-1..6 收编 ROADMAP §4.5） | `f7a8e82f`（P3ii） |
| P4 | 文件面 multipart + 多 part | ✅ 已落地（`bce286c7`；REST /files 两路由 + FileStore + message.send 结构化 parts 入口解析 + blobref wire 持久化/resume 重水化 + web 映射；门禁 workspace 3346 绿 + clippy 零告警 + web 697 绿，mutation 2/2 红。评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-92，followup F-P4-1~8 收编 ROADMAP §4.5）） | `bce286c7` |
| P5a-f | ACP v1 合规纵切 | ✅ P5a ✅ 已落地：`7a3bc61b`（update 词汇迁移到官方 v1 SessionUpdate 联合 + conformance 对照官方 schema 快照（`@agentclientprotocol/sdk@1.3.0`）逐条校验 + mock turn 4 变体 positive + 旧方言反向守卫 + MockLlm thinking 脚本；门禁 workspace 3350 绿 + clippy 零告警，mutation M1-M4 全红。评审 APPROVE-WITH-FOLLOWUPS，F-P5a-1..3 收编 ROADMAP §4.5）；P5b ✅ 已落地：`76f48645`（approval.ask→`session/request_permission` 反向 RPC 桥 + JsonRpcChannel pending-request 表 + serve 并发化 + 失败兜底 rejected；门禁 workspace 3364 绿 + clippy 零告警，mutation M1-M3 全红。评审 APPROVE-WITH-FOLLOWUPS（agent-96：M4 应答清表突变存活=测试缺口非生产缺陷，F-P5b-1..6 收编 ROADMAP §4.5）；P5c ✅ 已落地：`ddf5ad4a`（per-cwd EmbeddedServer 实例池 + mcpServers Session Overlay：overlay>workspace>user 名字级遮蔽、连接后台 warn-drop、close/archive 注销、load 重注册不持久化，stdio 以 type 缺席识别；门禁 workspace 3377 绿 + clippy 零告警，mutation M1-M4 全红。评审 APPROVE-WITH-FOLLOWUPS（agent-98，F-P5c-1..6 收编 ROADMAP §4.5））；P5d ✅ 已落地：`f34d404b`（session/load 重放：embedded 新增 `session.getHistory` 只读面（context memory 序列化 ContextMessage，ACP 不直读 wire.jsonl）→ `project_history_to_updates` 纯投影（user/assistant text→chunk、think→thought_chunk、tool_calls→tool_call CREATE+终态 tool_call_update，孤儿 tool 结果跳过）逐条 session/update 后 settle，读失败降级跳过重放；resume 不重放（v1 唯一差异项）；new/load/resume 响应带 modes（四模式 default/plan/auto/yolo，currentModeId 从引擎 plan.status+permissionMode 推导）+ configOptions（model/thinking/mode 三 select，数据源 agent.getConfig）；set_mode(modeId) 真生效（plan toggle 仅状态变化时翻转 + setPermission 映射）+ `current_mode_update`；set_config_option(configId+value) thinking/model/mode 生效 + 响应/`config_option_update` 通知；旧参数名 mode/name → invalid_params 不垫片；mirri-proto registry+tsv 增 session.getHistory 行（路由表 parity）。门禁 workspace 3384 绿 + clippy 零告警，mutation M1-M4 全红。报告：`.superpowers/sdd/parity-completion/task-p5d-report.md`。评审 APPROVE-WITH-FOLLOWUPS（agent-100，2026-09-05：mutation M1-M4 独立重做全红（M2 引擎+ACP 双红）+ 恢复 cmp 一致、门禁独立复现 3384/0/10 净 +7 精确对账 + clippy 零告警、红线 1-7 过六条半、六条自报偏离全部属实、越界五禁区全过；唯一扣分项 F-P5c-2 复现未修（派单特别核对项未自报），F-P5d-1..6 收编 ROADMAP §4.5））；P5e ✅ 已落地：`$/cancel_request`（`{"requestId": n}` 通知，SDK jsonrpc 层同形）→ ACP handler `handle_request` 在 prompt 入口同步注册 request-id→(session, engine) 映射，cancel 走与 `session/cancel` 共享的 `cancel_session_turn`（engine `message.abort` + F-P5b-4：in-flight `session/request_permission` 回 v1 `Cancelled` outcome + 晚到真回复按设计丢弃）；embedded wait-mode turn 注册 `in_flight_turns`（`InFlightTurn::Wait` 仅协作 cancel token，硬 abort 会搁浅 `turn.ended` wire 记录），cancel 后截断流收集为 EndTurn 的情形覆写为 `Aborted`；`session_prompt` 响应 stopReason 经 `acp_stop_reason` 映射 v1 词汇（aborted→cancelled、filtered→refusal、无对应→end_turn，TS `turnEndReasonToStopReason` 对齐）；更新通道 `UPDATE_CHANNEL_CAPACITY` 256→1024（P5e 裁决 B：无有界阻塞）+ serve 循环 Lagged 分支 `tracing::warn!`（可观测，恢复=下一通知续流 + session/load 重放）。门禁 workspace 3395 绿 + clippy 零告警，mutation M1-M5 全红（M5=engine wait 注册删除）。报告：`.superpowers/sdd/parity-completion/task-p5e-report.md`。评审 APPROVE-WITH-FOLLOWUPS（agent-102，2026-09-05，基线 `30cb4073`（与自报 `7285280c` 同树 amend 修标题笔误，记录在案）：mutation M1/M3/M4 独立重做全红 + 恢复 cmp 字节一致 TREE-CLEAN、**M2（按 session 全表取消）存活=测试缺口非生产缺陷（无并发双 prompt 精确性用例）**、门禁独立复现 3395/0/10 精确对账 + clippy 零告警、语义红线 1-6 全过、架构决策（容量 1024 裁决 B）落地一致、八条自报偏离全部属实、越界四禁区全过；F-P5e-1..7 收编 ROADMAP §4.5）；P5f ✅ 已落地：`5220e0b4`+`fc73749c`（auth gate 真查凭据五处 + logout 真删、list 磁盘枚举 v1 SessionInfo、delete 真删、image data URL + MIME 校验、question→elicitation 深映射 F-P5b-1 收编、fork `_meta` 挪位、G7 内容面 + 同域收编 F-P5a-1/F-P5c-1/F-P5d-1/F-P5b-5/F-P5d-2/F-P4-2；门禁 workspace 3429/0/10（+34）+ clippy 零告警，mutation M1-M7 全红。评审 APPROVE-WITH-FOLLOWUPS（agent-104：mutation M1-M4 独立重做全红 + 恢复 cmp、门禁复核分毫不差、13 偏离全部属实；F-P5f-1..9 收编 ROADMAP §4.5，F-P5f-6 撤销）） | `fc73749c`（P5f） |
| P6 | 管理面（provider/OAuth/profile/diagnostics/用户工具） | ✅ 已落地：`aa190e43`（agent profile CRUD 五 RPC + 五 REST 路由 + TOML 落盘 + essential/builtin 保护；config.diagnostics 三类悬空检测；refreshProviderModels models.dev 拉取/缓存/降级；契约链五件套同步 + P5d 遗留 fixture 漂移修复；门禁 workspace **3454/0/10**（净 +25）+ clippy 零告警 + node-sdk 483/0，mutation M1-M3 红还原。评审 APPROVE-WITH-FOLLOWUPS（agent-107：mutation M1-M4 独立重做全红、门禁六 crate 独立复现精确对账、REST wire 13 字段与 web mapper 对齐、25 文件无越界；新 F-P6-7/8/9、F-P6-4 升 medium，收编 ROADMAP §4.5）。用户工具 RPC 行未入本批，留独立子项 | `aa190e43` |
| P7 | 事件族 + 交互补全 + resume 形状验证 | ✅ 完成（agent-108）：P7a ✅ 已落地 `60fe1642`（事件族投影 29→44 方法 + 三消费端同步 + CronManager 首个生产 driver；门禁 workspace 3462/0 + clippy 零告警 + node-sdk 491/0 + web 697/0，mutation 2/2 红；followup F-P7-1~3 见 task-p7-report.md）；P7b ✅ 已落地 `71c37bb6`（mid-turn steer 死锁修复：共享 SteerBuffer handle 绕过 turn 长锁 + web abortSession/createChildSession 接线；门禁 workspace 3465/0 + clippy 零告警 + web 702/0 + node-sdk 491/0，mutation M1 红绿闭环）；P7c ✅ 已落地 `5e6263aa`（双后端 SessionView 五运行时字段 + F5 live usage 收口 + session.status 扩展 + web 真消费；门禁 workspace 3467/0 + clippy 零告警 + web 707/0 + node-sdk 491/0，mutation M1 红绿闭环）；P7d ✅ 已落地 `d1b65c06`（skillDirs 透传 createEngine 扩参 + TS scanner brand 目录对齐 + web workspace skills 接 skill.list；门禁 workspace 3473/0 + clippy 零告警 + web 710/0 + node-sdk 494/73s，mutation 3 红绿）；P7e ✅ 已落地 `7225ce37`（v1 ResumedSessionSummary 契约上 resume 三路径 + toolCalls 视图侧摊平 + create 期 permissionMode 持久化；门禁 workspace 3473/0 + clippy 零告警 + web 710/0 + node-sdk 495/73s，mutation 2 红绿）——**P7 全部子批完成，followup F-P7-1~9 见 task-p7-report.md**；评审 agent-110 **APPROVE-WITH-FOLLOWUPS**（隔离 worktree 独立复跑 3473/0 + clippy 零告警 + cross_backend 12/12，M1-M6 mutation 全红后字节恢复，四禁区核查通过，自报计数全部对账一致；新发现 F-P7-10~16 归档 ROADMAP §4.5，steer 语义族 F-P7-10..13 建议优先排期，见 task-p7-review.md）|
| P8 | 缓存 DEGRADE + provider 体验 + 图片旁路 | 🔄 部分：P8a ✅ 已落地 `9cabb41c`（取消闭合/修剪优先/变小校验/header 漂移事件四件，mutation M1-M5 红，workspace 全绿 + clippy 零告警）；P8b-1 ✅ `bdc27700`（quota 429 不可重试 + retryAfterMs，mutation M6 红）；P8b-2..5（btw/additionalDir/Kimi 视频/chunk 转发）与 P8c（图片旁路）时间窗耗尽记 F-P8-1..5 推迟——侦察与偏离记录见 task-p8-report.md；评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-112，2026-09-06：门禁独立复现 workspace 3490/0 + clippy 零告警分毫不差、P8a-only 独立 checkout 可编译、零越界零新增 IO；新发现 F-P8-9（medium，abort 闭合接线零红线测试）/F-P8-13（medium，retry_after_ms 未上客户端面）+ F-P8-10/11/12/14（minor）收编 ROADMAP §4.5，F-P8-9/13 建议随 P8b-2..5 收口批优先排期） | `bdc27700` |
| P9 | Desktop 壳（preload 桥/托盘/通知/单实例） | ✅ 完成（agent-113）：`186fe386`（mirriDesktop preload/IPC 桥——payload 校验工厂 + $EDITOR 启动映射）、`3c39063f`（托盘常驻/主进程通知+badge+dock-bounce/单实例锁/深链 mirricode:// + openSessionFromDeepLink/自启/更新就绪 env 门）、`d266ca1b`（web 三态降级链：桥→引擎 fs.open(能力声明)→复制路径降级提示 + 通知桥路径 + 角标同步）；门禁 desktop typecheck + 123 测试 / web typecheck + 736 测试（rust 无改动免 cargo），mutation 6 验红还原（1 个冗余防线 mutant 存活记录在案）；评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-114，2026-09-06，worktree /tmp/p9-review @ 2821ccb1：IPC 7 类校验/三态降级链/单实例/深链/托盘/更新就绪/通知/v1 fileLaunch 移植保真全部独立核实，计数对账一致，mutation 全部独立重做一致；新发现 **F-P9-8（medium，功能性阻塞）** tier-1 桌面桥在真实产品不可达——rust SessionView 不回显 cwd → sessionRootFor 三层解析链全断，"在编辑器打开"端到端死链，tier-1 测试全 mock 引擎不存在的 cwd 回显；+ F-P9-9 跨 scheme 测试缺口 / F-P9-10 能力缓存永不失效 / F-P9-11 notify 无 .catch / F-P9-12 报告计数失真（minor）——F-P9-8 建议随 P11a 收口批优先修复并补真实状态集成测试）；偏离 D-P9-1..3 与 followup 全量见 task-p9-report.md + task-p9-review.md | `d266ca1b`（P9c） |
| P10 | 视图与搜索面（brief 重估范围） | ✅ `4101f35c`+`9c25c9b6`（契约链五件套同步：registry 192/emb 178、TSV 184、EMB 路由表 178、node-sdk fixture 178+snapshot、web wire types；workspace **3511/0**（基线 3490 净 +21 精确对账：12 handler + 9 纯逻辑）+ clippy 零告警 + node-sdk 496/0 + web typecheck 零错 + web vitest 745/0；swap 1.59GB 合规全量；mutation M1-M4 红后字节还原；偏离 D1-D5 与 followup F-P10-1..4 见 ROADMAP §4.5 P10 实现记录 + task-p10-report.md）；评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-117，2026-09-06，worktree /tmp/p10-review @ e978a936：五环契约链/zero-IO 冻结追踪/TS 保真度六维/mutation M1-M4 独立重做/门禁全量计数逐项复现；新发现 **F-P10-5（medium）page_token offset 越界 slice panic**——探针实证，release panic=abort 下引擎崩溃，建议随 P11a 优先修复（一行级 + 回归测试）；+ F-P10-6（minor）D5"对齐 TS"论据失真更正——见 task-p10-review.md） | `4101f35c`/`9c25c9b6`/`e978a936` |
| P11a | TUI 切默认前置补齐（P8 遗留收口：F-P8-1 btw.start / F-P8-2 addAdditionalDir / F-P8-9 abort 红线测试 / F-P8-13 retryAfterMs 上线 + F-P8-12/14 顺手项 + F-P8-3 Kimi 视频 stretch；F-P8-4 chunk 转发已结案 `a76d8c53` 早于审计） | ✅ 完成（agent-118，2026-09-06）：`3cbdcb39`/`9e710729`/`6c20e913`/`01a692b4`（F-P10-5 越界防护随批）/`dd9095ec`（F-P9-8 引擎 cwd 回显随批）；门禁 workspace **3527/0/10**（基线 3511 净 +16）+ clippy 零告警 + node-sdk 496/74s + web typecheck 零错 + web vitest 746/0（含真实状态桥 e2e）；mutation M1-M7 红后字节还原；F-P8-3 Kimi 视频 stretch 未做记回；F-P8-4 结案 + F-P11a-1 drift 登记；详情见 ROADMAP §4.5 P11a 实现记录 + task-p11a-report.md | `dd9095ec` |
| P11a | （评审） | ✅ APPROVE-WITH-FOLLOWUPS（agent-119，2026-09-06，worktree /tmp/p11a-review @ d28618ee）：12 维度独立核实，门禁计数全项精确复现（workspace 3527/0 + node-sdk 496/74s + web 746/0 真二进制 e2e 亲跑 + typecheck 预存性独立对照），RM1-RM7 独立重做零存活（RM6 panic + RM7 重建二进制 web e2e 红活体复现）；新发现 **F-P11a-2（medium）MCP 注册绕过 btw 工具否决** + **F-P11a-3（medium）addAdditionalDir 无引擎侧生效面**（均建议随 P11 排期）+ F-P11a-4..7（minor）；详情见 ROADMAP §4.5 P11a 评审记录 + task-p11a-review.md | |
| P11f | P11a 评审 followup 修复批（F-P11a-2 工具白名单 / F-P11a-3b 上下文通知 / F-P11a-4 resolve 基准 / F-P11a-5 归一化+~ 展开 / F-P11a-6 注释 / F-P11a-7 loop-top 红线；F-P11a-3a 沙箱 allowlist 停止待裁决） | ✅ 完成（agent-120，2026-09-06）：`b6a4a8c6`（F-P11a-2）/`d13a1cf6`（F-P11a-3b/4/5）/`3abea6d8`（F-P11a-6/7 + changeset + plans/06）；门禁 workspace **3537/0/10**（基线 3527 净 +10 精确对账：tool_registry +3 / embedded +6 / turn_flow +1）+ clippy 零告警 + TS 面零改动；mutation M1-M6 红后字节还原；偏离 D-P11f-1..6 与 F-P11f-1/F-P11f-2 登记（3a 裁决：方案 A 共享 WorkspacePaths 句柄，落地 P11f-2 子批）；详情见 ROADMAP §4.5 P11f 实现记录 + task-p11f-report.md | `b6a4a8c6`/`d13a1cf6`/`3abea6d8` |
| P11f | （评审） | ✅ APPROVE-WITH-FOLLOWUPS（agent-121，2026-09-06，worktree /tmp/p11f-review @ 3abea6d8）：12 维度独立核实——F-P11a-2 咽喉点穷举（赋值点 9 处全表覆盖、MCP 进表三条生产路径无绕过）、门禁计数全项精确复现（workspace 3537/0/10/108 EXIT=0 + clippy EXIT=0 全量亲跑）、RM1-RM6 独立重做零存活、偏离 D-P11f-1..6 逐条裁定合理、停止规则 7 文件抽查属实、零越界零 agent 痕迹；新登记 F-P11f-3（minor，notify wire record 无测试锁定，建议随 P11f-2 补）+ F-P11f-4（minor，changeset 措辞待 3a 落地后成立）；详情见 ROADMAP §4.5 P11f 评审记录 + task-p11f-review.md | |
| P11f-2 | 沙箱 allowlist 子批（F-P11a-3a 方案 A：`WorkspacePaths` 共享句柄 + session 级宿主 + 三工具接线 + resume 恢复；顺带 F-P11f-3 wire record 红线） | ✅ 完成（agent-122，2026-09-06）：`89425f86` 单 commit（12 文件 +959/−116 含 changeset minor 双包）；门禁 workspace **3550/0/10**（基线 3537 净 +13 精确对账：mirri-core lib +5 / mirri-embedded lib +8）+ clippy 零告警 + TS 面零改动 + swap 1.59GB；红线 2/3 两阶段红证据（0/4→0/5）；mutation M1/M2 红后字节还原；偏离 D-P11f2-1..5 与 F-P11f2-1/2 登记见 ROADMAP §4.5 P11f-2 实现记录 + task-p11f2-report.md；评审 ✅ APPROVE-WITH-FOLLOWUPS（agent-123，2026-09-06，worktree /tmp/p11f2-review @ 89425f86：12 维度全过、门禁全项精确复现、RM-A~E,G 独立重做全红 + RM-F 存活（dedup 别名折叠，无安全影响）+ RM-G 探针证伪 :94 注释、F-P11f-4 关闭；新登记 F-P11f2-3（dedup 别名测试 + 注释更正）/ F-P11f2-4（mirri-server 种子，建议与 F-P11f2-1 同批）——见 task-p11f2-review.md） | `89425f86` |
| P11a | （E 核对段）切默认前置清单 E1-E12 逐项定向复跑 | ✅ 完成（2026-09-06，swap 1.59GB 合规）：E1/E12 静态证据 + E2/E3/E5/E6/E7/E10（embedded 定向 + core background_agent_tests 9 + session_events_tests 75）+ E4 btw 4 条 + E8 持久化 11 条 + E9 resume 16+1 条 + E11 原生绑定构建冒烟（release 43.32s + 真实 RPC 往返 config.diagnostics/session.list/-32601 错误路径）；证据归档 task-p11-e-checks.md | |
| P11 | TUI 切默认 + 四面终验矩阵 | ✅ 终验矩阵完成（2026-09-07，agent-125）：①内核 3×workspace 3550/0 + clippy 零告警 + 缓存三桶数值 + 重启存活 ②ACP conformance 45/45 + e2e_mock 5/5 + 真实客户端 10/10 ③Web Playwright 全链路（live 渲染/保序回放/审批/终端/管理面）④Desktop 换芯 .app（electron-builder 三连挂死改旧 DMG 换壳：新 SEA + web/ + 隔离冒烟 5/5——healthz/token/SPA/REST/WS 消息往返）⑤TUI 11/11（boot/对话/`!`shell/btw/compact/goal/mcp/skills/sessions/plugins/ctrl-b）。矩阵期四个产品修复：`46e5002a`（web 子命令 drop-in）、`ee2a07a3`（bundled TUI NAPI 绑定解析）、`0f37d6d8`（btw.start serve-wire 解包）、`f804c26c`（session.list SessionView→SessionSummary 映射，/sessions picker 崩溃）；changeset `247b2630`；证据 task-p11c-evidence.md（含 harness 伪影考古：pi-tui PasteBurst、面板异步挂载时序）。**默认翻转（P11b）保留用户硬门待裁决** | `46e5002a`/`ee2a07a3`/`0f37d6d8`/`f804c26c`/`247b2630` |

## 审计原始材料

六路审计报告全文在 `.tmp/audit/`（gitignored）：`rust-parity-*.log` / `arch-conformance.log` / `cache-hit-rate.log` / `web-desktop.log` / `tui-rust-path.log` / `acp-compliance.log`。差距总账的行级证据锚点以各报告为准。
