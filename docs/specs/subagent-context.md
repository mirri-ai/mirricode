# 子代理派发域现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-09 · 增量快照：2026-09-12（§6，见文末） · 等待语义快照：2026-09-13（§6.13） · 重启孤儿治理快照：2026-10-14（§7） · MCP 交付缺口快照：2026-10-21（§2.1） ·
> 配套设计文档：[subagent-requirements.md](subagent-requirements.md)（需求）· [subagent-architecture.md](subagent-architecture.md)（架构）·
> 相邻域：[collaboration-context.md](collaboration-context.md)（团队协作视角的现状速览）· [rust-core-design/context.md](rust-core-design/context.md)（引擎迁移视角）·
> 本文不描述目标、不含裁决——终态与裁决在设计文档。

## 1. TS v2 引擎现状（参照基线）

### 1.1 派发工具与执行器

- `agent/tools/agent/agentTool.ts`（`SubagentTool`，工具名 `Agent`）：
  - 参数：`prompt` / `description`（必填）、`subagent_type`（缺省 `coder`）、`resume`（与 `subagent_type` 互斥）、`run_in_background`、`model`（模型别名覆盖）。
  - 模型绑定三级：显式 `model` 别名 → 目标角色 `defaultModel` → 继承调用方模型别名与思考档位（`session/subagent/configSection.ts` `resolveSubagentBinding`）；别名经 catalog 校验，非法别名对外报错并列合法选项。
  - 角色白名单：`subagentAllowlistFor`（角色声明的可派发子角色名单），名单外派发被拒绝并说明可用集合。
  - 启动前的提示词前缀：`applyProfilePromptPrefix`（角色正文声明的 per-invocation 前缀，如 explore 的探索指令）。
  - 继承：子代理继承调用方的**权限模式**（`setMode`）与**用户侧挂载的自定义工具**（`inheritUserTools`）。
  - 后台可用性门控：`TaskList`/`TaskOutput`/`TaskStop` 三个工具全部激活才允许 `run_in_background`，否则报 "Background agent execution is not available..."。
  - 前台任务的 release 机制：`waitForForegroundRelease(taskId)` —— 前台任务可被用户 detach（返回 `'detached'`），此时工具把前台调用转成后台结果返回，子代理继续运行；"超时自动转后台"（`'timeout_detached'`）是 Bash 类工具的 `autoBackgroundOnTimeout` 行为，**Agent 工具未启用该选项**——Agent 前台超时直接以 `timed_out` 终态定局。
  - 结果格式：前台成功 `agent_id/actual_subagent_type/resolved_model/status: completed/[summary]`；失败区分 `timed_out`（"Agent timed out after …"）与停止（"The subagent was stopped before it finished. Reason: …"，用户中断另有专门文案）——**仅超时结果带续作提示**，且明确"子代理保留先前上下文，重做任何结果丢失的未竟工具调用"（`redo any unfinished tool call if its result was lost`，agentTool.ts:617-618），被停止的结果不带续作提示；后台结果带 `task_id`、`automatic_notification: true`、不得轮询的指引与恢复提示（`task.lost|failed|killed` 通知后仍可续作，**对话跨会话重启保留**，agentTool.ts:587）。
- `agent/tools/agent/subagent-task.ts`：`SubagentTask`（kind `agent`），`start(sink)` 把执行 handle 的 completion 桥进任务服务；cancel 时对用户中断落 `killed`，其余错误落 `failed`。
- resume 校验 `ensureOwnedIdleSubagent`：目标是子代理（元数据标签判定）、**必须属于当前调用方**（所有权校验）、**未在运行**（运行中 resume 被拒绝）。resumed 代理保留自己已记录的模型（"子代理不跟随父代理当前模型"）。

### 1.2 后台任务服务

`agent/task/taskService.ts`：

- `registerTask(task, { detached, timeoutMs, signal })`：超时经 `armManagerTimeout` 设 `timed_out` 终态并 abort 执行；`stop(taskId)` 与 `stopByUser` 会 **abort 任务自身的 AbortController**（真正的取消），落 `killed` 终态；`stopAllOnExit` 会话关闭时收敛。
- 容量上限：`[agent_task] maxRunningTasks`（`assertCanRegister` 对 detached 任务生效），超限报 "Too many background tasks are already running."。
- 完成通知：基于状态转换的自动通知（`<notification type="task.completed|task.failed|task.killed|task.lost|task.timed_out">` 等，`agent/task/notificationXml.ts`），`task.lost` 表示进程重启后失去归属的 running 任务。

### 1.3 swarm 调度器

`session/swarm/agentRunBatch.ts`（`AgentRunBatch`，`ISessionSwarmService.run` 驱动的核心批量调度器）：

- 起步斜坡：`INITIAL_LAUNCH_LIMIT = 5` 立即启动 + 之后每 `INITIAL_LAUNCH_INTERVAL_MS = 700ms` 启动一个；`MIRRICODE_AGENT_SWARM_MAX_CONCURRENCY` 环境变量覆盖并发上界。
- 供应商限流恢复状态机：子代理以限流错误失败 → `suspended` 事件通知 UI + **对同一代理实例重试**（`launcher.retry(agentId)`，保留已完成工作）→ 指数退避（`RATE_LIMIT_RETRY_BASE_MS = 3000`，factor 2）→ 容量收缩（每 2s 减 1，下限 1）/ 恢复（每 3min 加 1）。
- 每任务超时：`task.timeout` → abort 该尝试 → "Subagent timed out."。
- 中止语义：批量 abort 区分用户取消（全量 `aborted`，含 `state: started|not_started` 与对应文案）与一般失败（整体 reject）。
- 结果三元状态：`completed / failed / aborted`，附 `state: started|not_started`（未启动就中止的可辨识）。

### 1.4 配置与契约

- `[subagent] timeout_ms`（`session/subagent/configSection.ts`）：默认 `2h`，env `MIRRI_SUBAGENT_TIMEOUT_MS` 优先，env 覆盖不落盘。
- 事件面：`subagent.spawned / started / suspended / completed / failed`（`packages/protocol/src/events.ts`）；后台任务事件 `background.task.started` / `background.task.terminated`（`packages/protocol/src/events.ts:698,703`，终态与原因由 `terminated` 的 `status`/`stopReason` 字段承载）。
- 子代理身份：生命周期元数据 `parentAgentId` 标签，所有权经标签校验，非独立 session。

## 2. Rust 侧现状（mirri-core）

### 2.1 SessionSubagentHost（`src/subagent/host.rs`）

> **MCP 工具交付缺口（2026-10-21 快照）**：live 派发路径在 spawn 时只继承父 agent 的用户工具（`host.rs` 的 D-47 继承块），**不从会话工具收敛点快照 MCP 工具注册进子代理**——角色工具白名单（如默认 `agent` profile 的 `mcp__*`）声明了授权但过滤面为空，声明空诺。MCP 工具进入 agent 工具表的唯一路径是引擎的活跃 agent 广播（连接状态变化时遍历当时 live 的 agent），后 spawn 的子代理错过全部已发生事件。对照：**重启恢复路径已实现交付**（`session/manager/restore.rs` 的子代理水合从主 agent 工具表 `mcp_tools_snapshot()` 快照注册，注释明言「与 live spawn 相同的 MCP surface」）——两路径行为不一致，live spawn 是缺口侧。实例事故：会话审查中子代理因无 MCP 工具而层层再派子代理直至撞 `maxNestingDepth` 上限（`~/.mirri-code/v3_sessions/.../session-025773d1`）。终态裁决见 ADR-014（交付与过滤分离），修复登记 ROADMAP。

> **B4 落地（2026-09-13）**：host 已去 block_on（SDR-B4-01）——completion future 是纯 async 树（`turn_flow` 为 tokio Mutex、子代理工具锁为 parking_lot send_guard 读锁跨 await），无 `spawn_blocking` / `block_in_place` / `Handle::block_on`；子代理持有自己的嵌套 host（深度 +1，`maxNestingDepth` 默认 5、0 不限，SDR-B4-03）；绑定解析为三层 `resolve_subagent_binding`（显式 alias → 角色 model/effort → 继承父，D-48）；子代理继承父 LIVE 权限模式与用户工具、角色 promptPrefix 前缀（D-47）；`summaryPolicy` 走 `distill_summary` 提炼（D-61）；子代理 turn ops 经 `RoutedWireRecorder` 落自身 journal（D-55，见 §2.6 与 SDR-B4-02）。panic 隔离经 `catch_unwind` 保持旧 `JoinError` 面。以下为 B4 前的历史描述（保留作演进记录）：

- `spawn`（同步）：生成 agent id → 角色解析为 `AgentScopeConfig` → model 覆盖 + catalog 别名校验（非法别名报错并内联 curated 列表，上限 20 条）→ 继承父代理的 reverse_rpc / web_search / fetch_url / event_bus → **显式置 `subagent_host = None`（"No nested subagents in v1"，子代理不能再派发）** → 创建 TurnFlow → 注册进 `children` 跟踪表 → 同步发布 `subagent.spawned` → 构建 completion future。
- completion future（异步）：`create_agent` → 发布 `subagent.started` → 触发父会话 `SubagentStart` hook → `SubagentLlmFactory::create_llm` → `spawn_blocking` + `block_in_place` 内 `run_one_turn`（规避并发 swarm 的嵌套 block_on 死锁）→ `SubagentStopGuard` 保证所有出口（含 future drop）都补发 `SubagentStop` hook → 提取最后一条非空 assistant 文本 + 用量 → 发布 `subagent.completed` / `subagent.failed` → `cleanup_child` 删除跟踪表条目。
- `resume`（`host.rs:511`）：从 `children` 表取条目（**不存在即报 "not found (not created by this host)"**）→ `get_agent` → `SubagentStarted` → **不触发 `SubagentStart`/`SubagentStop` hook** → 复用同一 `TurnFlow` 再跑一个 turn → 发布 `completed`；**错误路径只清理不发布 `subagent.failed`**。
- `available_profiles`：每个角色的模型能力（catalog 按别名或原始模型名匹配）与可用模型列表，喂给工具描述。

### 2.2 派发工具（`src/tools/agent_tool.rs`、`src/tools/agent_swarm_tool.rs`）

- `AgentTool`：参数与 TS 对齐（prompt/description 必填、subagent_type/resume 互斥、run_in_background、model）；描述携带 `Available profiles` 与 `Available models`（curated 门控）。
- 前台/后台：后台 = `tasks.register` + `tokio::spawn` 立即返回 `task_id`；前台 = 注册后 `task.start` 等待——**内联在工具执行 future 里**（`agent_tool.rs` 前台路径），因此父回合 future 被整体 drop（`message.send` 的 stall watchdog）时驱动器随之消亡；`ForegroundOrphanGuard`（2026-09-18 孤儿缺陷修复）在 drop 时 fire 子 cancel token 并 `tasks.stop` 落定跟踪条目（first-terminal-wins 保护正常/超时/取消落定不被覆盖），stalled 回合不再把前台子代理孤儿化到 sweeper 的多小时超时。
- 结果格式：前台 `agent_id/actual_subagent_type/resolved_model/status: completed|failed/<result>/<resume_hint>`——**取消（Stopped）不置 error，输出 `status: completed` 且结果为空**；后台 `task_id/agent_id/status: running (background)/<resume_hint>/<next_step>`。
- `AgentSwarmTool`：`description` 必填、`subagent_type`（缺省 coder）、`prompt_template`（必须含 `{{item}}`）、`items`（≥2，上限缺省 3、`[subagent] maxSwarmItems` 或 `MIRRI_AGENT_SWARM_MAX_ITEMS`/`MIRRICODE_AGENT_SWARM_MAX_ITEMS` 可配、`0` 不限——Rust SDR-012，刻意偏离 TS 硬编码 128；**重复展开提示被拒**）、`resume_agent_ids`（**映射语义：agentId → 逐成员续作指令**，先于 items）、`model`；无 `run_in_background`、无每任务超时、无并发参数。
- swarm 执行（**2026-11 B3 已重写**）：`subagent/swarm_batch.rs` 调度器驱动——首 5 立即 + 每 700ms 一个的起步斜坡（D-42）、`host.spawn` 延迟至实际 launch（D-54）、每尝试超时（D-45）、取消三分（D-44）、429 同身份 requeue（D-43）；结果 XML 对齐 TS v2：`<summary>` 非零项（completed/failed/aborted）、`<resume_hint>` 固定文案紧随 summary、`mode` 仅 resume 行、`state` 仅非 completed 行、body 不转义、工具结果恒 success（SDR-B3-03）。旧实现（全部 spec 先同步建 handle 再 `buffered(5)`、cancelled 计入 failed）已退役。
- 单发错误：spawn/resume 单成员失败只定局该成员（state 按 agent_id 有无），**批继续**（TS `failedAttemptOutcome` 对位）。

### 2.3 后台任务设施（`src/task.rs` + `src/tools/task_*.rs`）

- `TaskManager`：register / append_output / complete / fail / stop / 心跳过期检测（`is_stale`）/ `terminate_expired`（`agent_timeout` 到期 → `Failed(TASK_TIMEOUT_MESSAGE)`）/ 输出截断（`MAX_TASK_OUTPUT_BYTES`，保留尾部、预览带截断标记）/ 按创建会话隔离输出 / 持久化（重启后 running → stopped 调谐，即 TS 的 lost 语义）。
- 接线缺口：`with_agent_timeout` / `terminate_expired` / `with_heartbeat_interval` **只有核心代码与测试，mirri-server 未接线调用**。
- `TaskStopTool`：只 `tasks.stop(&task_id)` 改状态，**不触发任何取消**；而运行中的子代理跑完后其 `settle(Completed)` 会把状态改回 completed/失败——"已停止"终态被覆盖。
- `TaskListTool` / `TaskOutputTool`：按会话隔离读取。

### 2.4 事件与 hooks

- 事件：`subagent.spawned / started / completed / failed` 发到 session 事件总线；子代理 turn 的 live 事件（`turn.step.*`、工具事件）经 `live_relay` 以独立 wire turn id 流到总线。
- hooks：`SubagentStart` / `SubagentStop`（父会话 hook 引擎），spawn 路径全出口配对（含未来 drop 的 Drop 兜底）；**resume 路径完全不触发**。

### 2.5 测试覆盖

- `src/subagent/host.rs` 单测：hook Start/Stop 配对（成功/LLM 失败/已取消）、model 别名校验（含 curated 内联与 20 条截断）、`available_profiles` 能力解析；**resume 仅一个"未知 id 报错"用例，无任何成功路径/并发/事件对称性用例**。
- `tests/subagent_integration_tests.rs`：TaskManager 输出记录 4 例（完成 / panic / 两种取消令牌各一）。
- `tests/background_agent_tests.rs`：心跳过期、超时终止、会话隔离、截断、持久化与重启调谐。
- `tools/agent_tool.rs` / `agent_swarm_tool.rs`：解析层与 mock handle 的发散用例；**无真实调度的集成测试**（取消、部分失败、并发、限流均未覆盖）。

### 2.6 会话记录与恢复现状

**TS v2（参照基线）**：
- 子代理身份元数据（含 `parentAgentId` 归属标签、装配信息）持久化于会话元数据的 agents 表（`sessionMetadataService.ts:123`），随会话状态落盘。
- **每个子代理有自己的独立记录流**（own wire journal）——"Resumed agents keep the model recorded in their own wire journal"（`sessionSwarmService.ts:16` 注释）；续作从自身记录流恢复记忆。
- 子代理 run 同时**镜像进派发者（父代理）的记录流**（`mirrorAgentRun`），父子关联与派发面呈现依赖该镜像。

**Rust（mirri-core）现状**：
- 子代理经 `create_agent` 注册进 session 的**内存 agents 表**（`session_scope.rs:120,450`），无身份元数据持久化（无 agents 表落盘形态；身份注册表在 `SessionSubagentHost.children`，进程内）。
- 子代理的 turn/工具事件经 live_relay 以独立 wire turn id 写入**会话的同一 `wire.jsonl`**（`session_scope.rs:217` 注释"persisted to the same wire.jsonl as the rest of the session"）——子代理记录与主会话记录**混流**。
- 恢复：`resume_session` 折叠该**单一 wire.jsonl** 重建（`session/replay.rs` "resume_session folds the session's wire.jsonl"），子代理事件作为会话日志的一部分一并折叠；**无按身份独立记录/回放/重建的载体**。

**与基线语义的差距**：Rust 无独立记录流 → 主会话日志被子代理事件污染（膨胀、难以按身份截断/归档）、按子代理回放与导出困难、续作记忆的重建绑定于整条会话日志的重放、与 TS own-journal 语义不一致（见差距矩阵 G21）。

> **事实勘误 + B4 落地（2026-09-13）**：本节旧描述有误——B4 之前子代理 turn 的会话 ops（`context.append_message` / `context.append_loop_event` / `turn.*` / usage）**根本没有落盘**（host 构造子 TurnFlow 时从未调用 `set_wire_recorder`），真正混入主 `wire.jsonl` 的只有子代理经 agent 级 recorder 触发的**会话域 ops**（goal/plan/cron）。B4 修正了方向：**新增** per-subagent journal（`agents/<id>/wire.jsonl`，`RoutedWireRecorder` 按 WireOp 枚举穷举 match 分域路由，SDR-B4-02）+ 主日志 `subagent.registered` 身份注册表 + 重启回放（`restore_subagents` 折叠子代理 journal 重建记忆与 AgentScope；`adopt_session_agents` 让 `Agent(resume=...)` 跨重启可用）。子代理 turn 产生的 op 类型不再落入主日志。

## 3. TS v2 vs Rust 差距矩阵

| # | 能力 | TS v2 | Rust（mirri-core） | 影响 |
|---|---|---|---|---|
| G1 | 子代理运行超时 | `[subagent] timeout_ms`（默认 2h）+ env，per-task 生效，`timed_out` 终态 + 超时文案与续作提示 | **已落地（2026-10，B1）**：typed `[subagent] timeoutMs` + env，Agent 工具前后台按 per-task 显式 deadline 注册（`register_with_timeout`），前台由工具超时包装即时定局、后台由会话级 sweeper（`terminate_expired` 先 fire signal 再转终态）真实取消；wire 上为 `failed` + 超时文案（不新增 `timed_out` 状态，避免三端事件同步） | 已收敛 |
| G2 | 前台 detach | 前台任务可被用户 detach（`'detached'`）转为后台继续；"超时自动转后台"为 Bash 类工具能力，**Agent 工具不启用**（超时直接定局） | 前台必然 await 到终态 | 长任务无法转为后台，用户没有中途解耦手段 |
| G3 | swarm 起步限流 | 首 5 + 每 700ms 一个；env 覆盖并发上界 | **已落地（2026-11，B3）**：`subagent/swarm_batch.rs` 调度器——首 `INITIAL_LAUNCH_LIMIT=5` 立即启动、其后每 700ms 放行 1 个（launch 计数独立于并发槽）；env 主名 `MIRRI_AGENT_SWARM_MAX_CONCURRENCY`（兼容 TS 拼写、非法值 fail-soft：warn + 忽略，SDR-B3-01） | 已收敛 |
| G4 | 供应商限流重试 | 指数退避 + 容量收缩/恢复 + `suspended` 事件 + 对同一代理实例 `retry`（保留已完成工作） | **已落地（2026-11，B3）**：`SubagentError::RateLimited` 分类（billing quota 429 除外）→ `subagent.suspended` 事件（`notify_suspended`）+ 同身份以 retry 形态重入队（`TurnFlow::retry_one_turn` 无新 prompt、不发新 spawned）+ 指数退避 3s×2 + 容量收缩（2s/−1/下限 1）/恢复（3min/+1、清 launch 闸）；唯一未完成任务直接 failed 终局（不无限重试）；批内限流模式不回退 | 已收敛 |
| G5 | swarm 结果语义 | `completed/failed/aborted` 三分 + `state=started/not_started` | **已落地（2026-11，B3）**：`SwarmOutcome{Completed,Failed,Aborted}` + `SwarmStartState{Started,NotStarted}`；取消双文案逐字 TS；结果 XML 对齐 TS v2（summary 非零项、`mode` 仅 resume、`state` 仅非 completed 行、body 不转义、恒 success——SDR-B3-03）；spawn/resume 单成员失败不再整批炸 | 已收敛 |
| G6 | 每任务超时（swarm 内） | per-task `timeout` → "Subagent timed out." | **已落地（2026-10，B1）**：`AgentSwarmTool` 每成员 lazy deadline（`buffered` 排队不烧预算），超时先真实 cancel 成员令牌（修复了 map 闭包丢弃令牌的前置缺陷）再落 `failed` + TS 文案 | 已收敛 |
| G7 | 嵌套派发 | 子代理装配后可按角色工具面再次派发 | **已落地（2026-09-13，B4）**：每个子代理携带嵌套 host（深度 +1，孙派发走 `caller_profile = 子角色` 的 R3-4 白名单臂；`maxNestingDepth=5`/`0` 护栏 + `[subagent] maxNestingDepth` 配置面，SDR-B4-03）；host 去 block_on 后嵌套为纯 await 树（SDR-B4-01），两层派发 e2e 以 10s 超时锁定不死锁。已知限制（评审轮 SF-5）：跨重启恢复的子代理不带嵌套 host，可 resume 续作但不能再派发孙代理（ROADMAP O12） | 已收敛 |
| G8 | 权限模式 / 自定义工具 / 提示词前缀继承 | 子代理继承调用方权限模式、用户挂载工具、角色前缀 | **已落地（2026-09-13，B4）**：LIVE 权限模式（fallback 父 config 快照）+ 父用户工具 `user_tools_snapshot` 在 turn 开始前注册 + 静态 `promptPrefix` 前缀（TS 动态函数式前缀不做，SDR-B4-01 记偏差）。**口径澄清（2026-10-21，SDR-014/ADR-014）**：继承清单不含角色工具面——子代理可用哪些工具由其角色的工具面声明统治（TS 同口径）；MCP 工具的 live 派发交付缺口见 §2.1 | 已收敛（工具面归属语义）；MCP 交付缺口开放（§2.1） |
| G9 | 思考档位携带 | 绑定解析带 `thinking`（模型 defaultEffort → 子代理思考档位） | **已落地（2026-09-13，B4）**：`resolve_subagent_binding` 三层绑定（显式 alias → catalog `defaultEffort`；角色 `default_model`/`model` + 角色 `thinking_effort`；继承父 model+effort），对位 TS `configSection.ts:91-112` | 已收敛 |
| G10 | 后台容量上限 | `[agent_task] maxRunningTasks`，超限拒绝 | **已落地（2026-10，B1 + 评审轮）**：`[task] maxRunningTasks` 经 `session_scope_config_from` 装配进 `TaskManager::register_background`，超限返回 TS 同文案；前台注册不受限；所有 detached 生产者计入容量（Agent/后台 bash/后台提问，SDR-010），Agent 派发在 spawn 前预检拒绝 | 已收敛 |
| G11 | 后台可用性门控 | TaskList/TaskOutput/TaskStop 未激活则拒绝后台 | **已落地（2026-10，B1）**：`Agent` 工具构造携带 background gate（Weak 句柄读 `ToolManager::is_tool_active`，避免环引用），`run_in_background` 在 spawn 前以 TS 同文案拒绝 | 已收敛 |
| G12 | 完成自动通知 | 状态转换自动通知（completed/failed/killed/lost/timed_out） | 仅经总线发布 `background.task.started/terminated` 事件（task.rs:341-360,504-506），无注入父代理上下文的模型通知；`TaskOutput` 支持阻塞等待（task_output.rs:63-74） | 父代理无自动通知，需轮询或阻塞等待，后台派发闭环不完整 |
| G13 | resume 运行中拒绝 | 所有权 + 运行态双重校验，运行中 resume 报错 | 文档声称拒绝但**实现无运行态标志**，运行中 resume 会排第二个 turn | 同一子代理并发双跑，状态错乱 |
| G14 | 完成后续作 | 完成/失败/丢失/被杀均可续作（会话内保留） | completion 结束即 `cleanup_child` 删条目，resume 报 "not found"；`<resume_hint>` 对已结束代理无效 | 续作提示名不副实，失败工作无法恢复 |
| G15 | 取消信号真实性（前台） | 用户中断/停止 → `status: failed` + 明确文案 | 取消 → `status: completed` + 空结果 | 主代理误判"成功完成"，可能掩盖中断 |
| G16 | TaskStop 真实停止 | stop → abort 执行 → `killed` 终态 | 仅改状态，运行继续，终态被后续 settle 覆盖 | "停止"失效、状态自相矛盾 |
| G17 | resume 事件/hook 对称 | resume 走同一运行面（事件/hook 一致） | resume 不发 hook、错误路径不发布 `subagent.failed` | 观测面悬挂：TUI 子代理停在"运行中"；hook 消费方漏事件 |
| G18 | swarm 子代理描述 | 每项 `描述 #序号 (角色)` | **已落地（2026-11，B3）**：每成员 `child_description` = `"{swarm 描述} #{1-based 序号} ({profile|resume})"`，随 `subagent.spawned.description` 到 web 任务面板成员行；工具显示名 `swarm (N subagents)` | 已收敛 |
| G19 | 取消/中止时资源回收 | 任务取消链路收尾 | **已落地（2026-10/2026-11，S0 + B3 收尾）**：`RunningGuard` Drop 置 `running=false`（S0，条目按 G14 保留）；B3 补齐观测后半——guard 携带 bus + `settled` 原子标志，未达终局分支即 drop（含 drop-before-first-poll）时补发 `subagent.failed{error:"cancelled"}`（SDR-B3-02，惠及前台 Agent 取消路径）；swarm 未启动项零事件零 spawn，由结果 XML `not_started` 行收口 | 已收敛 |
| G20 | 并发爬坡观测 | `spawned` 随调度渐进发布 | **已落地（2026-11，B3）**：`host.spawn` 延迟到调度器实际 launch 时调用（斜坡驱动），`subagent.spawned` 随渐进节奏发布（`should_launch_five_immediately_then_one_per_interval` 锁定） | 已收敛 |
| G21 | 会话记录载体 | 每子代理一条独立记录流（own wire journal，`sessionSwarmService.ts:16`）+ 派发面镜像（`mirrorAgentRun`） | **已落地（2026-09-13，B4）**：`agents/<id>/wire.jsonl` 独立 journal + `RoutedWireRecorder`（WireOp 枚举穷举 match，无通配臂，SDR-B4-02）+ 主日志 `subagent.registered` 注册表 + reducer fold + 重启回放（`restore_subagents` + `adopt_session_agents`）；主日志零子代理 turn op（A11 断言含 `request.header.changed`/`context_size.measured` 等 turn 期事件）。TS 的 `mirrorAgentRun` 派发面镜像未做（主日志的注册表 + 事件总线已承载呈现），记偏差 | 已收敛 |

## 4. 已确认的 Rust 侧缺陷清单（现状速查）

> **修复状态（2026-09-12）**：以下 5 条已全部修复（S0 正确性清偿批次）。各自带回归测试：`subagent::host::tests`（G13/G14/G17/G19 四条）、`tools::agent_tool::tests`（G15）、`task::tests` + `tools::task_stop::tests`（G16）。

1. ~~**resume 契约违背**（`src/subagent/host.rs:511` 一带）：运行中 resume 未被拒绝（G13）；结束后条目被删导致 resume 必然失败（G14）~~ → **已修复**：`ChildEntry` 增 `running` 标志（spawn/resume 检查并占位，运行中 resume 拒绝 "already running"）；所有落位改为 `settle_child`（置 not-running、保留条目至会话结束）而非删除——完成/失败/取消后均可续作，`<resume_hint>` 名副其实。`create_agent` 失败（agent 从未存在）仍走 `cleanup_child` 删除。
2. ~~**取消误报成功**（`src/tools/agent_tool.rs` `TaskManagerSink::settle` + `format_foreground_result`）：`Stopped` 不置 `is_error` → 前台结果 `status: completed`（G15）~~ → **已修复**：`TaskManagerSink` 增 `stopped` 位，前台结果按 `ForegroundOutcome`（completed/failed/stopped）渲染——stopped 为 error 结果、`status: stopped`、附「was stopped before completion」说明。
3. ~~**TaskStop 假停止**（`src/tools/task_stop.rs:102` + `task.rs`）：不联动取消，终态会被后来的 settle 覆盖（G16）~~ → **已修复**：`TaskManager` 增 `attach_signal` 信号注册表（Agent 工具注册处接线）；`stop`/`stop_all` 先触发信号再转状态——被停止的子代理真实取消；`transition` 改「首个终态优先」，迟到的自然 settle 不再覆盖 Stopped。
4. ~~**resume 事件/hook 不对称**（`src/subagent/host.rs` resume 路径）：无 Start/Stop hook；除 Aborted 外错误路径无 `subagent.failed`（G17）~~ → **已修复**：resume 路径与 spawn 同一运行面——`SubagentStart`/`SubagentStop` hook + `SubagentStopGuard` 配对；所有错误出口（agent 不存在、LLM 构建失败、turn 失败、panic、Aborted）均发布 `subagent.failed`。
5. ~~**取消路径资源泄漏**（`src/subagent/mod.rs` `SubagentTask::start` select 取消分支）：drop future → 底层 `spawn_blocking` 任务继续至 token 生效、`children` 条目与 TurnFlow 泄漏（G19）~~ → **已修复**：`RunningGuard`（Drop guard）在 future 构造时移入其捕获状态——无论是否轮询过，drop 时同步置 `running=false`（含 swarm `buffered` 排队未启动项的 drop-before-poll 场景）；条目本身按 G14 语义保留（会话内可续作），不再构成泄漏。swarm 排队未启动项缺终态事件的观测缺口仍开放（见差距矩阵 G19 后半）。

## 5. 与相邻域的关系

- **collaboration**（团队协作）：本域是协作域的上游机制——`collaboration-requirements.md` US7.1 将"subagent 与团队模型的关系"列为裁决项（单角色退化 vs 长期并存），裁决前本域保持现状边界；`collaboration-context.md` §1 的 subagent 速览为本域 §1 的粗粒度视图。
- **rust-core-design**（引擎迁移）：`rust-core-design/00-overview.md` 内核层的 Swarm/Subagents 是迁移架构锚点；本域架构文档按其分层落位（mirri-core 内核、mirri-server 接线）。
- **agent profile 域**（角色）：派发边界（A4 可派发子角色）、子代理身份（A3）的规范定义在 [glossary.md](glossary.md) 与 agent-profile 域；本域只承载派发/调度行为。

## 6. 增量盘点（第 2 轮侦察 · 快照 2026-09-12）

> 本节为**追加**的增量盘点，不改动 §1–§5 的既有内容；与 §1–§5 表述冲突时以本节为准（代码以 2026-09-12 为准）。本轮新增覆盖：任务管理面深盘（通知投递、持久化、输出管理、优雅停止）、定时任务（cron）子系统、任务工具三件套契约、后台提问、print 模式策略、swarm 增量、Agent 工具增量、消费端、v1 判定，并追加差距矩阵 G22–G36 与两处既有条目的事实漂移标注。

### 6.0 既有条目的事实漂移（代码演进，非文档改写）

- **D1（§1.2 配置节名）**：本文 §1.2 记后台容量上限为 `[agent_task] maxRunningTasks`；现实现的任务配置节为 **`[task]`**，并注册兼容旧节 **`[background]`**（同 schema，`[background]` 为基底、`[task]` 覆盖同名字段），`[task]` 下字段：`maxRunningTasks` / `keepAliveOnExit` / `bashAutoBackgroundOnTimeout` / `bashTaskTimeoutS` / `killGracePeriodMs` / `printWaitCeilingS` / `printBackgroundMode(exit|drain|steer)` / `printMaxTurns`；env 覆盖 `MIRRICODE_BACKGROUND_MAX_RUNNING_TASKS`、`MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT`（不落盘）（`agent/task/configSection.ts:30-88`）。
- **D2（§1.1 agentTool 行号）**：`agentTool.ts` 超时续作提示现位于 616-620 行（§1.1 引用的 617-618 漂移至此，事实不变）；后台结果 resume_hint 的 587 行引用仍准确。
- **勘误生效（2026-09-12，B0 治理批次）**：D1/D2 经裁决登记为正式勘误条目——§1.2 的 `[agent_task]` 节名以 D1 所录现值（`[task]`/`[background]` 双节及字段族）为准、§1.1 的行号引用以 D2 所录现值为准；§1 原文按"增量不改写既有内容"约定保留，冲突时以本节为准（与本节引言的冲突规则一致）。本域 ROADMAP §3 同步标注。

### 6.1 任务管理面深盘（v2 `agent/task/taskService.ts`，1479 行）

**持久化与恢复**（`agent/task/persist.ts`、`taskOps.ts`）：
- 布局：每 agent 一份 `<sessionDir>/agents/<agentId>/tasks/<taskId>.json`（原子写状态）+ `<taskId>/output.log`（追加写原始输出）；main agent 另有**只读回退根**（旧 session 级任务根，读旧不写新）（`taskService.ts:290-300`、`persist.ts:1-17`）。task id 形状校验 `{prefix}-{8位}` 防路径穿越（`persist.ts:27,63-67`）；旧 snake_case 记录读时归一化（`persist.ts:215-278`）。
- wire 侧 `TaskModel`：`task.started` / `task.terminated` 两个持久 Op 折叠出 taskId→info 的可回放 Map；`task.terminated` 附带 4KiB 的 `outputTail` 尾部快照（仅入 fold 不入 Model）（`taskOps.ts:33-76`、`taskService.ts:178-180`）。
- 恢复三段式：wire 回放建 ghost → 磁盘加载（`loadFromDisk`，新旧记录取新者，`taskService.ts:575-590,1377-1391`）→ `reconcile`：非终态 ghost 落 **lost** 并回写磁盘、发 `task.terminated`（`taskService.ts:592-599,958-973`）。
- 会话关闭孤儿：`dispose()` 中 detached 任务被杀时标记 `sessionClosedOrphan`，settle 落 **lost**（"was running, no live process"）而非 killed，避免与重启调谐打架（`taskService.ts:168-175,826-851,1052-1060`）。

**通知投递（模型侧闭环）**：
- 终态通知是**投给模型的合成 user 消息**（`TaskNotificationStepRequest`，`mergeable`、准入 `activeOrNewTurn`）：主 agent turn 运行中则折入当前 turn 的下一步，空闲则自起新 turn（`taskService.ts:223-239,1121-1149`）。
- 通知 XML：`<notification id category type source_kind source_id agent_id?>` + Title/Severity/body + children；severity 映射：completed=info、其余=warning（`taskService.ts:1221`）；agent 类任务失败时 body 附恢复指引（`Agent(resume=...)`、agent_id 与 task_id 的区分警示、"retains its full prior context... redo the last tool call"）（`notificationXml.ts:13-36`、`taskService.ts:1416-1440`）。children 为输出文件块（`<output-file path bytes>` 引导读文件）或 3,000 字节兜底预览块（无落盘输出时的 fallback，`NOTIFICATION_FALLBACK_PREVIEW_BYTES`；注意区别于 TaskOutput 工具的 32KiB 预览）（`taskService.ts:196,1332-1359,1196-1201`）。
- 去重与幂等：通知键 = taskId+status+notificationId 三元组；delivered 键登记进 checkpoint 化 wire Model（`TaskNotificationDeliveryModel`，`taskService.ts:124-135`）；**对话 undo 后对账**（reconcileAfterUndo：已投递集合回滚、pending 请求清理，`taskService.ts:554-567`）。
- 恢复期已终态任务的未投通知**静默补投**（直接 append 进上下文，不起 turn）（`taskService.ts:1159-1176`）。
- `TaskStop` 停止前先 `suppressTerminalNotification`，使工具结果成为模型看到的唯一应答（`taskStopTool.ts:60-61`、`taskService.ts:640-651`）。
- 会话关闭收敛 `stopAllOnExit`：先对全部活动后台任务抑制终态通知再停止（`taskService.ts:815-824`）。
- 压缩后重注入：compaction splice 触发 `activeTaskReminderPending`，下一 turn 经 contextInjector 注入"压缩前的后台任务仍在跑，勿重复派发，用 TaskList/TaskOutput/TaskStop 管理"指引 + 任务清单（变体 `background_task_status`，`taskService.ts:196-201,316-332,361-367`）。
- 事件与遥测：`task.notified` 上事件总线（externalHooks 消费，`taskService.ts:1237-1247`）；telemetry `background_task_created` / `background_task_completed`（含 kind/duration/status，`taskService.ts:1103-1119`）。

**输出管理**：内存保留环 1MiB（`MAX_OUTPUT_BYTES`）；**后台（detached）任务输出自首个片段起即全量落盘**（注册时 `outputPersistStarted: detached`，`taskService.ts:406,483`）；**前台任务**在内存缓冲，超过 1MiB 或被转后台（`startOutputPersist`）后才开始落盘（`taskService.ts:1000-1026`）；**进程类（kind `process`）任务输出超 16MiB 被强制终止**并提示改用重定向（`taskService.ts:178-191,984-1046`）；TaskOutput 工具预览 32KiB + `output_path` 指向磁盘全量日志、引导用 Read 工具按 ~300 行/页翻阅（`task-output/taskOutputTool.ts:35,51-67`）。

**停止与超时**：`stop/stopByUser` → 先 cancel/abort，等 **SIGTERM 宽限期**（默认 5s，`killGracePeriodMs` 可配）内自然定局，超时再 `forceStop` 升级，仍未定局才落 killed（`taskService.ts:737-806,193`）；超时经 `armManagerTimeout` → `timed_out`；`killed` 定局若 `timedOut` 已置则改判 `timed_out`（`taskService.ts:207-215,696-709`）；`keepAliveOnExit=true` 跳过退出收敛（供独立存活的进程类工作继续，`taskService.ts:815-824,863-865`）。前台任务被用户 detach（`'detached'`）/超时自动转后台（`'timeout_detached'`后套新 `detachTimeoutMs`）；`autoBackgroundOnTimeout` 仅 Bash 类工具启用（默认 true）、Agent 工具未启用（`bashTool.ts:154-166,259-265`、`taskService.ts:696-713`）。

**print 模式（`mirri -p`）后台策略**（`agent/task/printModeService.ts:1-24,141-169`、`printDefaults.ts`）：三策略——`exit`（直接结束）/ `drain`（抑制活动任务终态通知、等全部终态后结束，受 `printWaitCeilingS` 封顶）/ `steer`（默认：后台任务未清零则续 turn，受时长上限与 `printMaxTurns` 封顶）；print 模式下未显式配置时 bash 任务超时与子代理超时均默认 0（不设限）（`printDefaults.ts:23-29,54-68`）。

### 6.2 定时任务（cron）子系统（v2 现状，均为既有 subagent 文档未覆盖的新面）

- **工具三件套**（`agent/tools/cron/`）：`CronCreate`（参数 `cron`（5 字段本地时间）/ `prompt`（≤8KiB）/ `recurring`（缺省 true）；拒绝条件：总开关关闭、表达式非法、**5 年内无触发**、**每 session 超 50 个**、prompt 超 8192 字节、one-shot 首触发距今超 350 天；返回 `id(ULID)/cron/humanSchedule(英文人话)/recurring/nextFireAt(本地 ISO 带偏移，post-jitter)`，`cron-create.ts:17-49`、`cronCreateTool.ts:53-137,194-205`）；`CronList`（无参数、strict；每条含 `id/cron/humanSchedule/prompt(截 200 字节)/nextFireAt(亦为 post-jitter 值)/recurring/ageDays/stale`，`cron-list.ts:16`、`cronListTool.ts:90-134`）；`CronDelete`（`id` 须 ULID；not-found 报错不幂等——防模型把删除当幂等；已 fire 的 one-shot 已自动删除故同样 not-found，`cron-delete.ts:16-20`、`cronDeleteTool.ts:12-28,71-97`）。
- **表达式**：自研 5 字段（分 时 日 月 周），**本地时区**；不支持秒/年/`?`/`L`/`W`/`#`/名称缩写；dow 0..7（7=周日）；dom/dow 同时受限按 Vixie OR；搜索窗口封顶 5 年 + 迭代上限 10^7（`app/cron/cron-expr.ts:3,48-53,104-113,172-197,226-236`）。
- **调度引擎**（`session/cron/sessionCronServiceImpl.ts`，Session 级）：轮询默认 1s（`MIRRI_CRON_POLL_INTERVAL_MS`，null=关定时器）；**主 agent turn 运行中 tick 直接跳过**（fire 只在 idle 时投递，"held until idle"，:315-319）；投递 = 合成 user 消息（`<cron-fire jobId cron recurring coalescedCount stale>` + 原始 prompt 逐字包裹）经 prompt 注入（`activeOrNewTurn` 准入，:455-495；`app/cron/format.ts:27-41`）。
- **生命周期**：one-shot fire 后自动删除（:388-391）；recurring 触发后游标推进；**recurring 存活超 7 天标 stale，stale 任务 fire 一次后自动删除**（`STALE_THRESHOLD_MS`，:73,670-675,444-453）；`MIRRI_CRON_NO_STALE=1` 关闭。
- **jitter（反羊群）**：recurring 向后偏移 ≤ min(10% 周期, 15min)；one-shot 仅当 ideal 落在整点 :00/:30 才提前 ≤90s 且不早于 createdAt；偏移量由 task id 哈希**确定性**导出；`MIRRI_CRON_NO_JITTER=1` 关闭（`app/cron/jitter.ts:39-115`）。
- **coalescing**：休眠/长 turn 期间错过的多次触发合并为一次投递，origin 带 `coalescedCount`（模型解读："I missed some checks; only the latest state matters"）；迭代上限 10^4（:576-599,365-371）。
- **持久化**：每任务一 JSON 原子文档，存 **homeDir 级** `<homeDir>/cron/<workspaceId>/<taskId>.json>`（homeDir=`MIRRICODE_HOME` 或 `~/.mirri-code`）；**不持久化绝对 nextFireAt**，只存 `lastFiredAt`/`createdAt` 游标，恢复时重算（`app/cron/cronTaskPersistenceService.ts:53-85`、`sessionCronServiceImpl.ts:519-527,601-628`）；写穿 + 按 taskId 串行队列（:678-690）。
- **会话生命周期**：任务按 `tags.sessionId` 过滤，**不跨新 session 传递**；无主任务被当前 session 认领；恢复经 main agent wire `onDidRestore`（CronModel 回放 → 磁盘加载 → start，:154-183,262-283）；**例外：session fork 时克隆全部 cron 任务到新 session**（`workspace/sessionLifecycle/sessionLifecycleService.ts:494,637-648`）。
- **配置/开关**（`app/cron/configSection.ts`，仅 env 不落盘）：`MIRRI_DISABLE_CRON`（总开关）、`MIRRI_CRON_NO_JITTER`、`MIRRI_CRON_NO_STALE`、`MIRRI_CRON_DEBUG`、`MIRRI_CRON_MANUAL_TICK`（SIGUSR1 手动 tick）、`MIRRI_CRON_CLOCK`（时钟源 `system|file:<path>`）、`MIRRI_CRON_POLL_INTERVAL_MS`。
- 事件/遥测：`cron.fired`（含 recurring/coalesced_count/stale/buffered）。
- 测试：`test/app/cron/*`（表达式/jitter/clock/持久化/Rust wire 对齐）、`test/session/cron/*`（工具/服务/steer e2e），详见 `test/session/cron/cron-tools.test.ts` 等（见 §6.2 引用文件）。

- **Rust 落地（B5 批次，2026-09-13 快照）**：上列四项核心语义已在 rust 引擎对齐（详见 §6.2 各条对照）——
  - **时区**：表达式按任务日历求值，缺省**宿主本地**（`iana-time-zone` 探测 + `chrono-tz` 解析，解析失败回退 UTC），`CronCreate` 新增可选 `timezone`（IANA 名，`"UTC"` 为旧语义逃生门），wire DTO 增 `timezone` 键、旧记录无该键按本地解释（`mirri-core/src/cron/tz.rs:19-101`；`cron.rs:82-96,122-236`；`tools/cron_create.rs:52-88`）；
  - **jitter**：TS `jitter.ts` 双算法逐字移植——`fraction_from_id`（8 位 hex 快路径 `u32/2^32`，否则 djb2 wrapping i32）；recurring 前移 `min(10% 周期, 15min)×fraction`；one-shot 仅本地 :00/:30 提前 ≤90s 且不早于 createdAt；`MIRRI_CRON_NO_JITTER=1` 经 `CronOptions` 关闭（`mirri-core/src/cron/jitter.rs:30-124`、`cron.rs:280-303`）；
  - **coalesce**：`check_due` 从游标 base（`max(last_fired, created_at)`，对齐 TS `baseFromMs`）逐 ideal+jittered 前进计数（上限 10000），一次触发携带 `coalescedCount`，游标推进到 `lastDue` 保相（`cron.rs:691-807`）；恢复路径同样从 base 重算（可落在过去 → 首个 due pass 合并触发，TS catch-up 语义，`cron.rs:557-623`）；
  - **7 天 stale**：recurring age ≥ 7d 在 fire 时标 stale、投递后写 `cron.delete`（durable，resume 不复活）再移除；one-shot 永不 stale；`MIRRI_CRON_NO_STALE=1` 关闭（`cron.rs:66-74,809-815`）；
  - 事件/注入 origin 携带真实 `coalescedCount`/`stale`（`session/event_projection.rs:834-859`、`mirri-embedded/…/cron_driver.rs:183-192`）；`cron.list` RPC 增 `timezone`/`stale` 键（`session_handlers.rs:2929-2947`）；fork 不携带 cron 补回归锁定（`session/manager/tests.rs:1598`、embedded `tests.rs:24959`）；本地/显式时区触发、coalesce+stale 端到端见 embedded `tests.rs:25003,25059,25118`。
  - 尚未对齐（计划内推迟）：`humanSchedule`/秒级/别名（P-024）、50 上限/8KiB prompt/350 天校验、env 开关族其余成员。~~idle-only 投递与 `<cron-fire>` XML 信封（B2 通知闭环）~~ **已落地（2026-11，B2）**：cron driver 忙时 hold 到本地队列（不折入 steer，游标已推进、事件已发布，下一 idle tick 注入），闲时以 `<cron-fire jobId cron recurring coalescedCount stale>` 信封包裹 prompt 注入（`mirri-core/src/cron/format.rs` 逐字移植 TS `renderCronFireXml`——camelCase 属性 + `<prompt>` 体，web `extractCronPrompt` 直接兼容；测试锚点 embedded `b2_notification_tests.rs`）。~~`/cron` 管理面（B6）~~ **已落地（2026-11，B6）**，见下条。
  - **Rust 落地（B6 批次，管理面）**：`cron.delete` RPC（管理写面）= `CronManager::remove` 薄封装——durable `cron.delete` wire op（B5 既有，write-before-mutate），resume 后删除仍生效（embedded 测试锁 close→resume），未知 id 答 `not_found`（task.cancel 先例）；无新 wire op、无 reducer 改动。`cron.trigger` RPC（run-now 面）= `CronManager::trigger`（立即触发一次，`coalesced_count=1`/`stale=false`，锁内发布 `cron.fired`——与 `check_due` 同址）+ handler `cron_driver.rs::dispatch_trigger_cron_task`：`tokio::spawn` fire-and-forget 复用 driver 同一注入管线（`inject_cron_prompt`，canonical main agent pin + 首 agent 回退，origin `cron_job`，B2 后签名含 `target_agent_id`）。run-now 不动日程（D-2：零 wire op/游标不推进/不重排/不消耗 one-shot）且无 `manual` 事件标记（D-3：复用现行 `cron.fired`，event_projection/web projector/SDK translate/ACP 四端零改动）——均落档 **SDR-B6-01**。两 RPC 五环齐（registry `both_kept` RustFirst 行，计数 213/198/188/173 + TSV 头 213 行/198 embedded-order——计划 F5 的 208/193/183 基线在 B9（3 条 approval rule 行）后已移至 211/196/186，本批 +2×both_kept 落至终值，与计划预测差 +3 均来自 B9；EMB 路由；NAPI 通用 `callMethod`；node-sdk fixture skip 条目）；mirri-server 零代码经 engine fall-through；cross_backend 补 cron 域首例 shape 对比（`mirri-test-harness/tests/cross_backend_contract.rs`，经共享 core `session.cron().add()` 双后端播种）。web 侧（`/cron` 命令 + SettingsView cron tab + 三适配器）由 B6 web 轨落地。

- **Rust 落地（B2 批次，2026-11 快照）——通知闭环（§6.1 对照）**：
  - **通知模型与 XML**（`mirri-core/src/task_notification.rs`）：`<notification>` 文档逐字移植 TS `notificationXml.ts`（identity 属性 + Title/Severity/body/children；body 文本转义较 TS 更严格——文档安全于逐字，测试锁定）；severity 映射 completed=info 其余 warning；`timed_out` 词汇由 `Failed(TASK_TIMEOUT_MESSAGE)` 表达（type=`task.timed_out`、body "timed out."）；agent 类失败附 `Agent(resume=…)` 恢复指引（agent_id ≠ task_id 警示、"redo the last tool call"）；children 为 `<output-file path bytes>`（持久化日志存在时）或 3,000 字节尾预览 `<output-preview bytes total_bytes truncated>`。
  - **origin 结构化**：`PromptOrigin::BackgroundTask { task_id, status, notification_id }`（camelCase wire，与 TS `TaskOrigin` 互认；无 serde default——半空 origin 解析失败由回放的 warn-and-skip 容忍）。
  - **挂账面**：`AgentTask.detached`（`register`/`register_with_timeout` 前台 false；`register_background`/`spawn_background_*`/`register_with_id`（shell detach 转换）true；旧记录缺省按 detached 加载，TS parity）+ `notification_suppressed`（持久化）；`suppress_terminal_notification`/`suppress_all_active` API。
  - **投递引擎**（`mirri-embedded/…/task_driver.rs`，挂 sweep tick）：终态+detached+未抑制 −（context 扫描 ∪ steer buffer 扫描）= due；忙时（flow mutex 争用）park 进 steer buffer（`turn.steer` 带 identity 先记录后入队，before_step flush 折入运行中 turn 的下一步）；闲时逐通知新起 turn（cron 形状：`turn.prompt`+`context.append_message`+turn bracket；确认门——provider 不可解析则不记录、欠账保留）；闲时唤醒（buffer 全 background_task 条目时记 `turn.steer_flushed` 后逐条转投递 turn——搁浅态是 resume 回填/桥级超时；活 abort 因 continue hook layer-1 flush 永远不搁浅）。**唤醒按通知逐 turn 投递**（非合并单 turn：合并 prompt 单消息单 origin 会破坏 per-key 的 context 扫描账本与回放一致性，记为对计划「合一 turn」的有据偏差）。
  - **账本（SDR-B2-01）**：扫描式——`delivered_keys`（context history 的 BackgroundTask origin）∪ `parked_keys`（steer buffer）；undo 免对账代码（T8 测试锁语义）；恢复=回放后 context 扫描。
  - **抑制接线**：TaskStop 先抑制后停；会话关闭 `suppress_all_active` 后 `stop_all_with_grace`（keepAlive 分支不抑制）；print drain 等待前 `suppress_all_active`（TS printModeService parity）。
  - **恢复静默补投**（`session_handlers.rs::restore_task_notifications`）：resume 在 steer 回填后、driver spawn 前执行——未投递通知直接 `context.append_message` + memory append，不起 turn。
  - **steer flush 耐久化收窄**：flush 时仅对 background_task origin 的条目记 `context.append_message`（user steer 既有不记录重放契约不动；通用化另立项，SDR-B2-01 附注）。
  - **TaskList kind 行**：三 kind 同一管理面（`kind: process/agent/question`）。
  - 测试锚点：core `task_notification::tests`、`turn_flow_tests.rs`（flush 耐久化）、`tools::task_list`；embedded `tests/b2_notification_tests.rs`（14 e2e：闲投递/幂等/忙折入/唤醒/TaskStop 抑制/关闭抑制/print drain/恢复补投/undo 重投+负对照/cron hold+信封×2/后台提问答案送达）。

### 6.3 任务工具三件套与后台提问（v2）

- `TaskList`：`active_only`（默认 true，只列非终态）/ `limit`（1–100，默认 20）；`active_only=false` 会包含 lost（按已终止对待）；渲染 `background_tasks: N` + 逐任务 key:value，`---` 分隔（`task-list/task-list.ts:14-28`、`taskListTool.ts:35-55`）。
- `TaskOutput`：仅 `task_id`；**永远非阻塞**（无等待语义）；返回 `retrievalStatus(success|not_ready)` + 任务元数据 + 32KiB 尾预览 + `output_path`/`terminalReason(timed_out|stopped|failed)` + 截断标记与 Read 分页指引（`task-output/taskOutputTool.ts:35-120`）。
- `TaskStop`：`task_id` + `reason`（缺省 `'Stopped by TaskStop'`）；终态任务幂等回报现状不改写（`Task already in terminal state`）（`task-stop/taskStopTool.ts:44-78`）。
- 三者注册 domain 均为 `agentTask`，由任务服务模块导入时注册（`taskService.ts:96-98`）。
- **后台提问（kind `question`）**：`AskUserQuestion(background=true)` 注册 `QuestionBackgroundTask`（idPrefix `question`），用户作答后 settle completed 并走终态通知（"答案稍后自动送达，不要轮询"）（`ask-user-question/question-background-task.ts:21-70`、`askUserQuestionTool.ts:74,98-125`）。任务面实际承载三类 kind：`agent`（子代理）、`process`（bash 后台）、`question`（后台提问）。

### 6.4 swarm 增量（相对 §1.3 的补充）

- 每成员超时接入 `[subagent] timeout_ms`（`agentSwarmTool.ts:195` 传入 `timeout: timeoutMs`，v1 时代为独立参数面）。
- **`resume_agent_ids` 为映射（agentId → 逐成员续作指令）**，不再只是名单：每成员可带自己的续作 prompt（`agent-swarm/agentSwarmTool.ts:242-245`）；续作成员先于新条目（与 §1.3 一致）。
- swarm 显示名 `swarm (N subagents)`（`agentSwarmTool.ts:130-134`）；`subagent.suspended` 事件由 swarm 服务层发布（`sessionSwarmService.ts:117-124`）；限流重试对**同一身份**以 `kind:'retry'` 续跑（保留上下文，不发新 spawned；`sessionSwarmService.ts:198-226`）。
- 结果 XML：`<summary>completed: N, failed: N, aborted: N</summary>`（三分计数）+ 逐成员 `<subagent mode item state outcome>`（`agentSwarmTool.ts:307-338`）。

### 6.5 Agent 工具增量（相对 §1.1 的补充）

- **工具描述动态拼装**：基础 `agent.md` + 按"TaskList/TaskOutput/TaskStop 是否全启用"二选一的 `agent-background-enabled.md`（"完成结果在后续 turn 以合成 user 消息送达，不要轮询/等待"；含"不要后台化后立刻空等"指引）/ `agent-background-disabled.md`（"任何 run_in_background 调用在子代理启动前被拒绝"）+ 每类型描述（含 **Tools 面**（all/none/all except/受限清单）与能力标签行）+ curated 模型清单（`agentTool.ts:147-175`、`agent/agentTool.ts` buildProfileDescriptions:488-544）。
- **摘要提炼**：子代理 turn 结束后若最终 assistant 文本不满足角色的 summaryPolicy（minChars），自动以续问 prompt 追加 turn（最多 retries 次）直到产出合格摘要；turn 以 max_tokens 截断 → 定局失败（`Subagent turn failed before completing its final summary: reason=max_tokens`）（`session/subagent/runAgentTurn.ts:36-37,126-159`）。
- 镜像层 hook：`subagents.hooks.onWillStartAgentTask`（agentName/prompt/signal/modelAlias，抛错即取消子代理）+ `notifyAgentTaskStopped`（`session/subagent/mirrorAgentRun.ts:128-182`、`subagent.ts:35-61`）；`subagent.spawned` 事件携带 `swarmIndex/runInBackground/model`（`mirrorAgentRun.ts:34-47,96-126`）。
- 绑定解析三层与 §1.1 一致（`session/subagent/configSection.ts:91-112`）；模型别名错误内联合法清单（`configSection.ts:247-259`）；生命周期失败包一层模型定位提示（`wrapSubagentModelError`，:261-281）。
- 注册失败文案映射：`'Too many detached tasks are already running.'` → `'Too many background tasks are already running.'`（`agentTool.ts:435-441`）。

### 6.6 消费端（TUI / web / desktop）

**TUI（apps/mirri-code/src/tui）**：
- **`/tasks` 后台任务面板**：全屏三栏（任务列表 + Detail + 输出预览），每 1s 静默轮询刷新；打开时优先选中 running 任务（`controllers/tasks-browser.ts:49-112`）。键位：`↑/k ↓/j` 选择、`Tab` 切 all/active 过滤、`r/R` 手动刷新、`s/S` 停止（内联 `y/Y` 确认，5s 超时自动取消；终态任务忽略并提示 already terminal）、`Enter/O/o` 打开全屏输出查看器（`less +F` 式尾随 + 翻页键位）、`Q/Esc` 退出（`components/dialogs/tasks-browser.ts:238-300,364-380`）。列表行 taskId 按 kind 着色（agent/question/process）+ 状态徽章 + 描述；header 汇总 `N running / N completed / N interrupted / N total`（"interrupted" 为 failed+timed_out+killed+lost 的合并计数，`tasks-browser.ts:157,353`）；面板打开时默认 filter=all、只显示后台（detached）任务（`controllers/tasks-browser.ts:70`、`components/dialogs/tasks-browser.ts:129-140,335-359`）。工具描述引导模型指路 `/tasks`（`agent/tools/os/bash/bash.md:16`）。
- **前台 detach（Ctrl+B）**：编辑器捕获 `Ctrl+B`，仅当确有前台任务可 detach 时消费按键；优先 detach 运行中的 `!` shell 命令，否则把全部前台任务逐个转后台；flash `Moved N task(s) to background. /tasks to view.`（`mirri-tui.ts:2468-2540`）。前台卡片提示 `Press Ctrl+B to run in background`（Bash 运行满 10s 才显示，Agent 立即显示，`tool-call.ts:53-54,1015-1043`）。detach 后经 `background.task.started(kind=agent)` 把卡片翻成 `◐ backgrounded`（即使工具结果落地也保持 backgrounded，`session-event-handler.ts:1053-1079`）。
- **子代理/swarm 渲染**：前台子代理事件路由到父工具卡片（子代理文本/思考流、子工具调用、live 输出、usage/contextTokens 均入卡；无父卡则合成独立卡）；卡片阶段机 queued/spawning/running/done/failed/backgrounded，完成 `✓`、失败 `✗`、后台化 `◐`、活跃 spinner，统计行 `N tools · elapsed · tokens`（`controllers/subagent-event-handler.ts:70-131`、`components/messages/tool-call.ts:57,1095-1182,1767-1815`）；同一步骤 ≥2 个 Agent 调用合并组卡（`components/messages/agent-group.ts:1-27`）。swarm 成员网格：三位序号成员、braille 进度条、底部状态行（Orchestrating/Prompting/Working/Completed/Failed/Aborted/Cancelled/Queued/Rate limited），`subagent.spawned.swarmIndex` 对位，结果解析 `<subagent … outcome>` XML（回退旧格式），turn 取消时整群标记 cancelled（`components/messages/agent-swarm-progress.ts`、`subagent-event-handler.ts:227-239,519-544`）。
- **后台事件播报与徽章**：`background.task.started/terminated` → agent 翻卡片 / process-question 追加 transcript 状态条（`bash task started/completed/failed in background`、`timed out`、`stopped`、`lost`，detail 附 exit code / 停止原因 / `session restarted before completion`，`utils/background-task-status.ts:44-85`）；footer 两个独立徽章 `[N tasks running]`（bash）与 `[N agents running]`（子代理）（`components/chrome/footer.ts:242-311`）。回放期经 origin `background_task` 对等渲染（lost/killed/timed_out 改写 headline，`controllers/session-replay.ts:260-263,662-712`）。
- **cron fire 呈现**：`cron.fired` → transcript `CronMessageComponent`：标题 `Scheduled reminder fired`（missed 时 `Missed scheduled reminders`），stale/missed 用 warning 色；detail 行含 cron 表达式、jobId、one-shot、`N fires coalesced`、`N missed`、`final delivery`；正文为注入的 prompt 全文；回放剥 `<cron-fire>` 信封、Markdown 导出同（`components/messages/cron-message.ts:23-84`、`session-replay.ts:537-566,771-772`）。**TUI 无 `/cron` 命令**（registry 全集核对），用户仅能自然语言请求模型排定（`tui/constant/tips.ts:29`）；headless CLI（`run-prompt.ts:592-611`）对 cron/subagent/background 事件一律忽略不呈现。
- **swarm 命令**：`/swarm [on|off|<prompt>]`（manual 权限下弹确认框，footer `swarm` 徽章，`commands/{registry.ts:172-175,swarm.ts:20-55}`）。
- 终端级通知：turn 完成时 OSC 9 / BEL 一次（可配 `unfocused|always`，`utils/terminal-notification.ts:24-56`）。

**web（apps/mirri-web/src）**：
- 事件投影：`subagent.*`/`background.task.*`/`cron.fired` 全部进任务/消息模型（`api/daemon/agentEventProjector.ts:1096-1320,1380-1410`）；REST/WS API `listTasks/getTask({withOutput,outputBytes})/cancelTask`（`api/types.ts:1096-1098`、`ws-rpc-api.ts:1469-1510`、`daemon/client.ts:746-773`）。
- 任务面板：ChatDock 底部 pills（`Bash (N)` / `Sub Agent (N)` / `Todos`）切换内嵌 TasksPane；1s 轮询（运行中取 4KiB tail、终态一次性 32KiB，页面隐藏时暂停，`useTaskPoller.ts:11-13,116-199`）；行 = 状态 glyph + 名称 + kind 徽章 + 计时 + Stop 按钮（仅 running）；bash 行点击内联展开命令/输出 + 复制，子代理行打开右侧实时详情面板（`composables/client/useTaskPoller.ts`、`components/chat/{ChatDock.vue:193-248,ConversationPane.vue:243-269,TasksPane.vue:33-130}`、`AgentDetailPanel.vue`）。
- 子代理/swarm 卡片：AgentTool 卡片带 "Open" 按钮跳实时详情；swarm 卡片运行中默认展开、settle 折叠、成员手风琴行（`components/chat/tool-calls/{AgentTool.vue,SwarmTool.vue}`、`composables/swarmGroups.ts:39-90`）。
- cron：渲染为**用户气泡样式**（右对齐 accent 填充）的 CronNotice，meta 行人话化表达式 + `N fires coalesced`/`N missed`/`final delivery`/jobId/触发时间（`components/chat/CronNotice.vue:1-127`、`lib/cronHumanize.ts`）。**web 无 `/cron`、无 `/tasks` slash 命令**（`lib/slashCommands.ts` 仅 `/swarm`）。
- **web 无 detach（Ctrl+B 等价）交互**：任务面板只读 + stop（全仓核对无前台→后台转移入口）。

**desktop（apps/mirri-desktop）**：Electron 壳以 detached 子进程拉起 `mirri web` 并直接加载 web UI（`src/main/ensure-server.ts:262-275`、`src/main/index.ts:150-163`）——无独立任务/子代理/cron 消费代码，行为与 web 完全一致。

### 6.7 v1 判定（`packages/agent-core/src/services/task/`）

- **v1 `services/task` 为 legacy 死代码**：`ITaskService` 只是"面向 daemon 的 REST 防腐层"——对引擎 `ICoreProcessService.rpc.{getBackground,stopBackground,getBackgroundOutput}` 的薄包装，仅 `list/get/cancel` 三方法，**不拥有**注册/驱动/持久化/通知；有损映射（kind：`process→bash`、`agent→subagent`、`question→tool`；status：`timed_out→failed`、`killed→cancelled`、`lost→failed`）（`services/task/task.ts:1-25,49-81,142-166`）。
- 全仓无生产消费者：仅自身单测与 barrel 导出；其原宿主（v1 HTTP daemon）已不在仓库；kap-server 的 `/api/v1/sessions/{sid}/tasks` 路由实际解析 **v2** `IAgentTaskService` 并复刻同一张映射表（`packages/kap-server/src/routes/tasks.ts:1-47,278-304`）。
- TUI 默认 v2（`MIRRICODE_LEGACY_FLAG` 才走 v1 harness，且 legacy 路径的任务访问走 node-sdk RPC 直达引擎，不经此服务）；`mirri web` 默认 kap-server（v2）（`apps/mirri-code/src/cli/experimental-v2.ts:4-13,64-75`）。
- **结论：本域无 v1-source 能力项**——任务/子代理/定时任务全部以 v2 为基线；v1 `services/task` 仅作历史记录保留于此。

### 6.8 增量差距矩阵（G22–G36）

| # | 能力 | TS v2 | Rust（mirri-core） | 影响 |
|---|---|---|---|---|
| G22 | 终态通知作为对话轮投递 | `TaskNotificationStepRequest`（activeOrNewTurn：mid-turn 折入/idle 新 turn）+ 恢复期静默补投 + undo 对账 + 通知键幂等去重（taskService.ts:223-239,554-567,1121-1176） | **已落地（2026-11，B2）**：投递引擎挂 task driver tick——忙时折入 steer buffer（`turn.steer` 先记录后入队）+ 运行中 turn 的 before_step flush 折入下一步；闲时新起 turn（确认门：provider 不可解析则欠账保留）；闲时唤醒（buffer 全通知条目时转投递 turn，不搁浅）；恢复静默补投（session.resume 直接 append 不起 turn）；undo 对账免代码（扫描式账本，SDR-B2-01）；`<notification>` XML 逐字移植（含 output-file/output-preview children 与 agent resume 指引） | 已收敛 |
| G23 | 压缩后活跃任务重注入 | contextInjector `background_task_status` 变体 + "勿重复派发"指引（taskService.ts:196-201,361-367） | **已落地（2026-11，B2）**：压缩 apply 后注入 running 任务清单 system-reminder（`format_background_task_reminder`，B2 前已有）+ guidance 文案对齐 TS（补 "completion arrives via automatic notification"）+ 回归测试锁 | 已收敛 |
| G24 | 任务输出全量落盘 | `<agentId>/tasks/<taskId>/output.log` + output_path + Read 分页 + 4KiB 终态尾部入 wire（persist.ts、taskOps.ts:44-49） | 已有对应 `task_persist.rs`（`<tasks_dir>/<id>.json` + `<id>/output.log`，task_persist.rs:1-25）| 基本对齐（本轮新核实的 Rust 侧进展，较 §3 旧有印象更新） |
| G25 | TaskStop 抑制终态通知 | 停止前 suppressTerminalNotification，工具结果为唯一应答（taskStopTool.ts:60-61） | **已落地（2026-11，B2）**：TaskStop 先 `suppress_terminal_notification` 再 `stop_with_grace`（顺序对齐 TS）；会话关闭 `suppress_all_active` 后收敛；print drain 等待前抑制——均持久化，恢复后不通知 | 已收敛 |
| G26 | 定时任务（cron）全套 | 三工具 + Session 级调度（本地时区、idle 投递、jitter、coalesce、7 天 stale、one-shot 350 天/5 年窗校验、50 上限、8KiB prompt、homeDir 持久化、fork 克隆、env 开关族） | **已落地（2026-09-13，B5）**：本地时区缺省 + 可选显式 `timezone`、jitter 双算法、coalesce（游标推进对齐 TS `baseFromMs`）、7 天 stale 自动删除（cron.delete durable）、持久化仍不存绝对 next_fire_at、fork 不携带（回归锁定，DEC-4/SDR-006 增强方向）——实现事实见 §6.2「Rust 落地」。**剩余差距**：50 上限/8KiB/350 天校验、env 族其余成员与 fork-克隆（Rust 有意为不携带）。idle-only 投递与 `<cron-fire>` 信封**已落地（2026-11，B2）**：driver 忙时 hold（不折入 steer）、闲时以信封包裹注入（`cron/format.rs` 逐字移植 TS renderCronFireXml，camelCase 属性 + `<prompt>` 体，web extractCronPrompt 直接兼容）。**管理面已落地（2026-11，B6）**：`cron.delete`/`cron.trigger` RPC（五环 + cross_backend 首例，web `/cron` 面板由 web 轨落地），run-now 语义与无 manual 标记落档 SDR-B6-01 | 核心语义与管理面（D-29/P-020）均已收敛；剩余：50 上限/8KiB/350 天校验、env 族其余成员、fork-克隆（Rust 有意为不携带） |
| G27 | 后台提问（question 类任务） | AskUserQuestion(background=true) 注册任务、作答后通知送达（question-background-task.ts） | **已落地（2026-11，B2）**：后台提问（`spawn_background_detached`，无信号）作答定局后答案随通知送达模型——通知 children 携带 output 块（持久化日志为 output-file，模型可 Read/TaskOutput 取答案） | 已收敛 |
| G28 | print 模式后台策略 | exit/drain/steer + 时长/turn 封顶 + print 下超时默认不设限（printModeService.ts、printDefaults.ts） | **已落地（2026-10，B1）**：`session.handlePrintMainTurnCompleted` / `session.waitForBackgroundTasksOnPrint` 两 arm 落实现（print_mode.rs），模式解析 `显式 > keepAlive?drain:steer`，drain/steer 封顶（`printWaitCeilingS`/`printMaxTurns`），CLI print 循环消费；print 缺省（子代理/bash 超时=0）为内存覆写不落盘（SDR-009） | 已收敛 |
| G29 | swarm 续作名单映射 | `resume_agent_ids` 为 agentId→逐成员续作指令映射（agentSwarmTool.ts:242-245） | **已落地（勘误 + 2026-11 B3 锁定）**：缺口描述失实——Rust 实现本就是映射语义（schema `additionalProperties: string`，逐成员 prompt）；B3 补回归测试锁定（`should_send_each_resume_member_its_own_instruction`）+ `swarm_item` 持久进 `ChildEntry`、resume 成员经 `swarm_item_of` 重标注原 item（TS `getSwarmItem` 对位） | 已收敛（勘误记录见 §6.11） |
| G30 | 子代理摘要提炼 | summaryPolicy 不达标自动续问提炼；max_tokens 截断定局失败（runAgentTurn.ts:36-37,126-159） | **已落地（2026-09-13，B4）**：`ProfileSummaryPolicy`（minChars/continuationPrompt/retries）+ `distill_summary`（spawn/resume 双面共享）+ max_tokens 逐字 TS 文案失败 | 已收敛 |
| G31 | 会话关闭孤儿 lost | sessionClosedOrphan → settle 落 lost（taskService.ts:168-175,1052-1060） | **已核验并落地（2026-10，B1）**：close 路径 `stop_all_with_grace` 同步落盘 Stopped，重启加载直接见终态不二次调谐——Rust 以 persisted-killed 实现 TS 的 lost 语义，不新增 `lost` wire 状态（SDR-007 裁决） | 已收敛（语义等价实现） |
| G32 | 优雅停止 | SIGTERM 宽限（默认 5s，可配）+ forceStop 升级（taskService.ts:737-806） | **已落地（2026-10，B1）**：`stop_with_grace`/`stop_all_with_grace`（`[task] killGracePeriodMs` 可配，缺省 5s）先发信号、宽限内自然定局保留、超时才落 Stopped；后台 bash 任务接取消令牌（`spawn_with_signal`），进程终止复用 mirri-os 既有 SIGTERM→5s→SIGKILL 升级（双层宽限，SDR-008） | 已收敛 |
| G33 | keepAliveOnExit | 跳过退出收敛供独立存活工作（taskService.ts:815-824,863-865） | **已落地（2026-10，B1）**：`[task] keepAliveOnExit` 经 SessionScopeConfig 流转，close 时跳过收敛、任务保持 Running 并可继续自然定局；仍 running 者在下次重载时调谐为 Stopped | 已收敛 |
| G34 | 任务配置面 | `[task]`/`[background]` 双节 + env 覆盖（configSection.ts:30-88） | **已落地（2026-10，B1）**：typed `[task]`（`[background]` 为遗留基底，逐字段覆盖语义与 TS 一致）+ `[subagent]`，camelCase wire 发射（纠偏原 snake_case 透传），env 族（MIRRI_TASK_*/MIRRICODE_BACKGROUND_*/MIRRI_SUBAGENT_TIMEOUT_MS），数值下界加载期校验 | 已收敛 |
| G35 | swarm 每成员超时 | 接入 `[subagent] timeout_ms`（agentSwarmTool.ts:195） | **已落地（2026-10，B1）**：`AgentSwarmTool::new` 携带 `subagent_timeout`（`None`=不设限），与 G6 同一实现 | 已收敛 |
| G36 | 三类任务统一管理面 | agent/process/question 三 kind 同一任务面与通知面（taskService.ts、question-background-task.ts） | **已落地（2026-11，B2）**：三类 kind 共享同一通知面（`<notification>` XML 按 kind 出标题/恢复指引，question 的 timed_out 词汇由 Failed(TASK_TIMEOUT_MESSAGE) 表达）+ TaskList 输出补 `kind:` 行 | 已收敛 |

> 说明：G24 反映 Rust 侧自 §3 旧有盘点后的**新进展**（task_persist 已落地），非差距；G26 的 Rust cron 面已存在但语义与 TS 有系统性差异（时区、jitter、coalesce、stale），列入 ROADMAP 开放项待裁决。

### 6.9 出域工具核对（TodoList / goal，2026-09-12）

- **TodoList**：会话共享（Session scope）的待办清单工具，单 schema 双模式（全量替换/只读查询）；变更经 main agent 的 wire 日志以 `tools.update_store`（key `todo`）op 持久化（`TodoModel`），会话内回放/undo 一致，**无跨会话存取、无后台执行**——todo 域对 `IAgentTaskService`/`registerTask`/background 零引用（`agent/tools/todo-list/{todo-list.ts:21-37,todoListTool.ts:4-6,44-70}`、`session/todo/sessionTodoService.ts:4-9,89-117`）。
- **goal 四工具**（CreateGoal/GetGoal/SetGoalBudget/UpdateGoal，仅 main agent 注册）：turn 边界驱动的自治循环控制（active 继续 / complete、blocked 终止并产出总结或阻塞原因 prompt），goal 域对任务服务同样零引用——是持久 agent 状态而非后台执行单元（`agent/tools/goal/{create-goal,get-goal,set-goal-budget,update-goal}/*.ts`、`agent/goal/`）。
- **结论**：两者与后台任务生命周期零耦合，为 SDR-003 的出域裁决提供现状依据；本域文档不承载其需求。
### 6.10 B1 批次落地快照（任务/子代理生命周期与安全 · 2026-10）

> 依据 [baseline-gap-landing 总计划](../../plan/baseline-gap-landing.md) B1 批次；对应缺口 D-40/45/49/50/59/62/63/64/65，矩阵条目 G1/G6/G10/G11/G28/G31/G32/G33/G34/G35（§3/§6.8 各行已就地更新为「已落地」）。本节记录实现事实与四项 SDR 的落点（SDR-010 为 B1 评审轮补录）。

**Rust 落地事实**（测试锚点：`mirri-core` `task::tests`/`tests/background_agent_tests.rs`/`tools::{agent_tool,agent_swarm_tool,task_stop}::tests`，`mirri-embedded` `tests/b1_task_lifecycle_tests.rs`/`tests/b1_print_mode_tests.rs`）：

1. **配置面（D-65）**：`[task]`/`[subagent]` typed 化（`TaskConfig`/`SubagentConfig`/`PrintBackgroundMode`），`[background]` 保留 toml 载荷作为遗留基底、`[task]` 逐字段覆盖（TS `configSection.ts:50-56` 同语义）；wire 发射 camelCase（原 snake_case 透传属实现漂移，纠偏对齐 TS schema；顶层键转换、`env` 子表键名原样保留——env 变量名大小写敏感）；env 族 `MIRRI_TASK_*`（canonical）+ `MIRRICODE_BACKGROUND_*`/`MIRRICODE_SUBAGENT_TIMEOUT_MS`（v1 别名）；数值下界（maxRunningTasks≥1、printWaitCeilingS≥1、printMaxTurns≥1）在 `from_toml` 与 `config.set` 合并路径双端校验，遗留 `[background]` 基底同受校验。**camelCase 纠偏的客户端核查（评审轮补录）**：TUI/SDK 侧无任何消费者按 snake_case 键名解引用 wire `background`/`subagent` 节（node-sdk `config-mapper` 整域透传不解键；TUI 只读 providers/models 族）；TS 引擎读的是磁盘文件（snake_case，未动）而非 wire。
2. **每任务超时（D-40）**：TS 的 per-task per-kind 语义以**显式 per-task deadline** 实现——Agent/AgentSwarm 工具注册任务时带 `[subagent] timeoutMs`（0=不限→`None`）；`with_agent_timeout` 保留为无显式 deadline 任务的全局回退（测试锁定），**不**在 SessionScope 装配全局超时（避免给 bash 后台任务误上 2h 上限）。前台超时由工具内 `tokio::time::timeout` 即时定局（`Agent timed out after Ns` + resume 提示），后台由每会话 sweeper（`task_driver.rs`，tick 缺省 1s）兜底：`terminate_expired` **先 fire signal 再转终态**（recon 证实原实现从不触发取消信号——仅改状态的超时是孤儿制造机）。
3. **容量与门控（D-49/D-50）**：容量在 `register_background` 入口生效（前台注册不限，与 TS assertCanRegister 的 detached-only 口径一致）；**所有 detached 生产者都计入**——后台 Agent 直接注册、后台 bash 经 `spawn_background_with_signal`、后台提问经 `spawn_background_detached`（SDR-010：评审核实 TS `activeTaskCount` 统计全部 detached 条目后修正口径）；Agent 工具在 `host.spawn` **之前**做容量预检（`can_register_background`），拒掉的派发不铸造 `subagent.spawned` 事件；门控以 `ToolManager::is_tool_active`（两条激活路径都填 `enabled_tools`）为准，Agent 工具构造时注入捕获 **Weak 句柄**的 gate 闭包（强 Arc 会形成 manager→tool→gate→manager 环引用泄漏），`run_in_background` 在 spawn 前拒绝；`register_background` 的检查与插入在同一临界区（TOCTOU 关闭），`background_ids` 随首个终态转移修剪。
4. **优雅停止（D-63）**：`stop_with_grace`/`stop_all_with_grace`（`killGracePeriodMs` 可配，缺省 5s）先发信号、宽限内自然定局保留、超时才落 Stopped；无信号任务无法在窗口内定局、跳过等待即时升级；后台 bash 持有取消令牌（`spawn_background_with_signal`），停止时 OS 进程真实死亡（`should_kill_the_real_process_when_a_background_bash_task_is_stopped` 以 pgrep 验证）；后台提问**不挂信号**（无法取消的 detached 工作，停止即时升级不烧宽限，SDR-010）。**双层宽限见 SDR-008**。
5. **关闭语义（D-62/D-64）**：close 走 `stop_all_with_grace`，**persisted-killed ≡ lost**（SDR-007）；`keepAliveOnExit` 跳过收敛。
6. **print 策略（D-59）**：`session.handlePrintMainTurnCompleted`/`session.waitForBackgroundTasksOnPrint` 两 arm 落实现（print_mode.rs；无新 RPC，registry 原有行内补齐）；steer 的 turn 计数/时长 deadline 为 per-session 内存态（`print_states`），不持久化。**print 缺省覆写见 SDR-009**。

**已知事实（有意不改）**：`EmbeddedServer::dispose` **不**收敛会话任务——dispose 后后台任务继续跑到进程退出、重启时被调谐。close 路径已覆盖语义收敛（进程存活期内 close 与 dispose 的差异仅是"是否已经 close"），dispose 联动收敛留给需要的批次显式立项。

**后置修正（2026-09-15，活性信号）**：B1 T4 的 activity-as-liveness 原实现只覆盖终态输出——`TaskManagerSink::append_output` 仅在子代理定局（完成/失败/取消/panic）时发生，运行中的引擎步骤不经过它，导致长于心跳窗口（缺省 5min）的运行中子代理被每会话 sweeper 每秒误报 "background task is stale"。修正：`ActivityTracker` 的全部观测点（turn/step/LLM 请求/工具调用）实时盖戳 `last_activity_ms`；`SubagentHandle` 携带子 TurnFlow 的 tracker（host spawn/resume/adopt 三处接线）；Agent 工具的 `drive_with_activity_heartbeats` 以窗口 1/4 的节奏轮询该时钟，把**新增活动**转发为 `TaskManager::heartbeat`（后台与前台路径同覆盖）——真实停摆（runner 死亡、引擎静默）仍按原语义判 stale（`agent_tool` 测试锁定双向行为，红→绿验证）。

**测试事实**：swarm 成员取消令牌原在 map 闭包被丢弃（`|(spec, handle, _)|`）——D-45 的前置修复（捕获令牌）随本批次一并落地（§6.8 G6/G35 行）。

### 6.11 B3 批次落地快照（swarm 调度器 · 2026-11）

> 依据 [baseline-gap-landing 总计划](../../plan/baseline-gap-landing.md) B3 批次；对应缺口 D-42/43/44/52/53/54/60，矩阵条目 G3/G4/G5/G18/G19（后半）/G20/G29（§2.2/§3/§6.8 各行已就地更新为「已落地」）。语义基准：TS `agentRunBatch.ts`（调度器权威参照）+ `agentSwarmTool.ts`（结果渲染）。

1. **调度器（D-42/D-54/D-45）**：`subagent/swarm_batch.rs::run_swarm_batch`——`INITIAL_LAUNCH_LIMIT=5` 立即 + 每 `INITIAL_LAUNCH_INTERVAL_MS=700` 放行 1 个（launch 计数独立于并发槽，TS `scheduleNormalLaunch` 对位）；`host.spawn` 在实际 launch 时才调用（spawned 事件随斜坡渐进）；每尝试 `tokio::time::timeout` lazy deadline（B1 语义保持：超时先真实 cancel 成员令牌再 await）；结果按任务原序返回；单成员 spawn/resume 失败只定局该成员、批继续。
2. **限流 requeue（D-43）**：成员以 `SubagentError::RateLimited` 失败（`EngineError::ProviderRateLimit` 精确分类；billing quota 429 是独立终态不重试）→ `host.notify_suspended`（`subagent.suspended` 事件）+ 同身份以 `ResumeRequest{retry: true}` 重入队（`TurnFlow::retry_one_turn` 重跑失败 turn、**无新 user 消息**、无新 spawned——TS `promptService.retry()` 对位）+ 指数退避 `RATE_LIMIT_RETRY_BASE_MS=3000 × 2^retry_count` + 容量状态机（**事件驱动收缩**：requeue 时若距上次收缩 ≥2s 则 −1、下限 1；恢复 3min/+1 且清全局 launch 闸，基线随每次 requeue 刷新；批内不回退普通模式）+ **全局 launch 闸**（TS `nextRateLimitLaunchAt` 对位）：限流模式下每次 launch（普通与 retry 同规）需过闸并按 base 间隔重新布闸、每次 requeue 亦按 base 外推——429 风暴后的重发被逐个 base 间隔串行化。TS 的 not-ready 翻倍分支在 Rust 无对应阶段（Rust requeue 均为完成期失败 = TS ready 路径），有意不实现（绝不快于 TS）；**唯一未完成任务**（TS `isOnlyUnfinishedTask`）直接 failed 终局不重试；取消与限流竞态时以取消三分定局（RateLimited 亦落 Aborted/Started）。
3. **取消三分（D-44/D-53）**：`batch_cancel`（工具 `ctx.cancel.child()`）触发时——已 launch 成员经令牌树（成员令牌为其 child）取消后落 `Aborted + Started`（TS 双文案逐字：`"The user manually interrupted this subagent batch before this subagent finished."`）；未 launch 成员 `Aborted + NotStarted`（`"…before this subagent was started."`）、零 spawn 零事件；**退避窗口中被 park 的 requeue 成员已 launch**——按 started 定局并保留 agent_id（TS `finishWithUserCancellation` 按 started || agentId 判定），供 `resume_agent_ids` 续作。
4. **drop 兜底（SDR-B3-02）**：`RunningGuard` 扩展——未达终局分支的 drop（含 drop-before-first-poll，即并发 buffer 被取消的排队成员）补发 `subagent.failed{error:"cancelled"}`，G19 后半收口，同时惠及前台 Agent 取消路径；每终局分支 `mark_settled()` 防重复。
5. **成员描述与重标注（D-52/D-60）**：`child_description` = `"{swarm} #{1-based} ({profile|resume})"` 逐成员；`SpawnRequest.swarm_item` 持久进 `ChildEntry`，resume 成员经 `SubagentHost::swarm_item_of` 重标注原 item（TS `getSwarmItem`）；工具显示名 `swarm (N subagents)`。**已记录偏差**：`subagent.spawned.swarmIndex` 沿用 B1 的 0-based 值（TS 为 1-based 且与描述同源）——消费端仅做排序/配对（web `swarmGroups` 按 `swarmIndex` 排序），改基是跨后端 wire 值变更、超出本批「零词汇改动」范围，留待后续批次裁决。
6. **结果语义（SDR-B3-03）**：`format_swarm_results` 对齐 TS `renderSwarmResults` 逐字形状——summary 只列非零项；`<resume_hint>` 固定文案、条件 = 存在非 completed 且存在任一 agent_id、紧随 summary；`mode` 仅 resume 行；`state` 仅非 completed 行；body 不转义（web `parseSwarmResult` 按 "producer writes body unescaped" 契约消费）；工具结果**恒 success**。
7. **env 并发上界（SDR-B3-01）**：主名 `MIRRI_AGENT_SWARM_MAX_CONCURRENCY`（B11 规范名口径）+ 兼容 TS 拼写 `MIRRICODE_AGENT_SWARM_MAX_CONCURRENCY`；非法值 fail-soft（`tracing::warn` + 忽略）——TS throw 会炸 session 装配，不采纳。容量种子以「已完成 + 在跑（未失败）」近似（TS `startedSuccessCount`/onReady 无 Rust 对应物）。
8. **勘误（D-60/G29）**：缺口描述「仅名单语义」失实——Rust `resume_agent_ids` 自始就是映射（schema `additionalProperties: string`）；B3 以回归测试锁定 + `swarm_item`/`swarm_item_of` 补齐 TS `getSwarmItem` 的重标注面。
9. **测试事实**：core 单测（`swarm_batch.rs` 7 条：斜坡/上限/env 解析/取消三分/requeue 退避/唯一终局/容量收缩；`host.rs`：分类/suppress/suspended/retry 同身份/未终局 drop；`agent_swarm_tool.rs`：取消 XML 三分/描述/item 重标注/D-60 映射）+ embedded 集成（`b3_swarm_scheduler_tests.rs`：429→同身份 retry 全链路，事件流 spawned=2/suspended=2/failed=0，退避为真实 3s——无装配缝，接受）。

### 6.12 B7 批次落地快照（任务总览与前台转后台 · 2026-09-13）

> 依据 [baseline-gap-landing 总计划](../../plan/baseline-gap-landing.md) B7 批次；对应缺口 D-30/D-31/P-023，并入 B2 计划 T11（web 通知卡片，F-12 核验结论为缺失、本批补齐）。Web 轨 `0f4f8eab`→`8b3dc217`；Rust 轨 `3ed01d2e`→`4ac17f52`。

1. **跨会话任务总览（D-30/P-023）**：`task.list` 保持会话级（bare `AgentTask[]`）；新增 `task.listWorkspace`（`both_kept` RustFirst 五环——registry 四元组终值 214/199/189、TSV/fixture/route pin 同步）按 built runtime 的 live 会话聚合，行带 `sessionId` 归属；**未知 workspaceId / 无会话 → 空数组而非报错**（overview 是视图非寻址面，计划 D-1 明示裁决）。v3 RPC + cross_backend shape pin（`5f3bd112`）。
2. **前台转后台（D-31/P-023）**：模型 Bash 前台调用原先完全不注册任务——`tools/bash.rs` 补 `run_foreground_tracked`（TaskManager 前台登记 + release permit，TS `waitForForegroundRelease` 对位）；`task.detach` 扩展可选 `toolCallId` 寻址（klient 只发 toolCallId 时 serde 必须可选；两键同给 toolCallId 优先——TSV note + debug log pin；双缺省诚实 null）；detach 延续接线 `attach_signal` + select!（D-63 真实杀灭，测试断言 fake child kill flag）；`BackgroundTaskStarted` 先于同转换 `Terminated` 发出（永不晚序）；legacy `!` shell detach 的 signal-less 停止为独立缺口（precedent 一致，B4/B12 域）。
3. **B2 T11 通知卡片（web）**：`TurnRole` 增 `'notification'` + `NotificationNotice.vue`（completed→success 色、其余 warning）；`messagesToTurns.ts` 在 `isDisplayableUserMessage` 前拦截 `background_task` origin、剥信封、建 notification turn；`eventReducer.ts:164,526` 回声守卫（reducer 产生的合成行不重复投影）。**web `/tasks`（工作区总览）与 TUI `/tasks`（会话内面板）语义不同**——SDR-B7-01 素材。
4. **web 总览面**：`TasksOverviewPanel.vue`（SettingsView `'tasks'` tab，v3 gate + v1/v2 clamp 到 'general'）+ `/tasks` 斜杠命令 + Sidebar 速开；2s 轮询 + visibilitychange 暂停；纯逻辑 `lib/taskOverview.ts`（join/过滤/计数）；前台 Bash 卡片「转后台」按钮（SideChat 抑制——侧聊工具调用按主会话寻址会错靶）+ `toolCallBackgrounded` AppEvent + reducer `backgroundedToolCalls`（monotonic，result 落地后徽章仍在——US12.2 不伪完成）。
5. **SDR-B7-01 素材（待 B12 前落 SDR）**：①detach 后剩余超时未实现（TS `applyDetachTimeout`——本批仅用户主动 detach；agent_timeout sweeper 兜底仍在且会真实杀灭；无 per-task deadline，现行边界=用户 stop/自然退出）；②moved-to-background 的 `status: running` 硬编码（计划接口锁定签名，通知自愈）；③`wait().unwrap_or(-1)` 与 `!` 路径 finish_detached 既有拼写一致。
6. **既有事实（非本批改动，供后续）**：web `ws-rpc-api.ts` 既有 `listTasks` 的 `RpcTask`（camelCase 扁平）与 v3 `task.list` 真实 wire（snake_case tagged enum）不符靠 `??` 兜底——B7 新增 `listWorkspaceTasks` 用独立正确映射，未动既有路径。

### 6.13 等待语义专项盘点（快照 2026-09-13）

> 起因：goal 模式下主代理以非阻塞模式后台派发子代理/bash 后，做其他工作期间或需要结果时陷入反复调用 `TaskOutput` 确认结果的循环（每次调用即时返回 not_ready 快照），直到任务定局——token 空耗。本节盘点"模型可用的任务等待语义"现状与轮询循环的结构根因。

**TS v2（现行主基线）——无任何阻塞等待入口**：

- `TaskOutput`（`agent/tools/task/task-output/`）：schema 仅 `task_id` 一个参数（`task-output.ts:14-16`），执行为纯快照读取（`taskOutputTool.ts:86-124`），`retrievalStatus: success|not_ready` 二值；工具描述明文 "This tool is always non-blocking … never waits for the task to finish"、"Do not use TaskOutput to wait for a result you need before continuing — if your next step depends on the task's result, run that task in the foreground instead"（`task-output.md:7-8`）。**非终态快照（not_ready）不带任何每调用级的动态劝阻**——v2 无 v1 式的 next_step 纠正指引，劝阻只存在于静态工具描述中（与 v1 仅等待超时才出纠正指引的对比见下）。
- v2 工具注册面全集无 wait/sleep/block 类工具（rg `TaskWait|waitForTask|wait_for|block until|awaitTask|waitUntilTask|WaitOnTask` 零命中；注册清单含 Skill/WebSearch/Edit/Glob/Cron×3/Goal×4/TaskStop/TaskList/TaskOutput/Agent/FetchURL/AgentSwarm/Read/Bash/TodoList/Grep/Write/SelectTools/AskUserQuestion 等，无等待类）。
- 后台派发结果与描述面均带强反轮询文案：Agent 后台结果 `next_step` "The completion arrives automatically in a later turn — do NOT wait, poll, or call TaskOutput on it"（`agentTool.ts:585`）；Bash 后台结果同措辞（`bashTool.ts:446-450`）与前台 detach 场景（`bashTool.ts:434-441`）；后台提问同族（描述 "you do not need to poll, sleep, or check on it"、结果 "answer will arrive automatically … Use TaskOutput … for a non-blocking status/answer snapshot"，`askUserQuestionTool.ts:74,144-150`）；`agent-background-enabled.md` "Never launch in the background and then immediately wait on it (by polling TaskOutput, sleeping, or otherwise)"；压缩后重注入指引 `ACTIVE_BACKGROUND_TASK_GUIDANCE`（`taskService.ts:198-201`）。
- **内部等待原语存在但不暴露给模型**：`IAgentTaskService.wait(taskId, timeoutMs, signal?)`（契约 `agent/task/task.ts:98`，实现 `taskService.ts:867-908`，waiters 数组 + Promise.race + setTimeout）——v2 内部唯一消费者是 print `drain` 模式（`printModeService.ts:114`）；`waitForForegroundRelease(taskId)`（`taskService.ts:910-932`）仅供 Bash/Agent 前台执行路径的 detach 消费（`bashTool.ts:289`、`agentTool.ts:451`）。print `drain` 是引擎侧真正的阻塞等待（逐任务抑制通知后 `wait()` 至清零，`printWaitCeilingS` 封顶，`printModeService.ts:91-139`），但属驱动层策略、对模型完全透明（等待期间模型拿不到结果，除非等待前自行读过快照）。

**v1（`packages/agent-core`，legacy）——曾有且 schema 仍留 block/timeout（休眠态）**：

- v1 `TaskOutput` schema 含 `block`（default false；描述明文 "Discouraged — background tasks notify automatically on completion; use only when the user explicitly asked you to wait"）与 `timeout`（default 30s，max 3600s）（`packages/agent-core/src/tools/background/task-output.ts:45-59`）；执行体对非终态任务先 `manager.wait()` 再重取快照（:121-129），等待到期返回 `retrievalStatus: 'timeout'` + 纠正指引 "Do not block on it again — continue with other work or hand back to the user; you will be notified automatically when it completes."（:149-153）。
- **v1 自身的描述/schema 矛盾**：工具描述文件已同步为 v2 契约的 "always non-blocking" 文案（`task-output.md:7-8`，随 v2 引入批次 commit `4bc9a367` 更新），但 schema 的 `block/timeout` 参数仍在——描述宣称永不等待，参数面留有等待开关。
- 通路状态：TUI 默认 v2（`MIRRICODE_LEGACY_FLAG` 才走 v1 harness，见 §6.7），该参数处于休眠 legacy 状态，不构成现行产品能力；但其"超时返回部分输出 + 纠正指引"的形态是 P6 提案（requirements §17）的直接先例。

**Rust（mirri-core）——block/timeout 在 HEAD 存活**：

- `TaskOutput` 参数 `block`（default false，"When true, wait for the task to finish before returning"）+ `timeout`（default 30s，min 0，max 3600s）（`rust/crates/mirri-core/src/tools/task_output.rs:63-74`）；执行体走 `TaskManager::wait_for_completion`（`task_output.rs:119-136` → `task.rs:1563-1580`，50ms 间隔异步轮询，不占 OS 线程/worker）；**超时不是错误**——照常返回部分输出快照并追加 `message: task still running; partial output shown`（`task_output.rs:173-175`）。单测 3 条锁定（block=false 立即返回 / block=true 等到完成 / 超时返回部分输出，`task_output.rs:291-352,435-459`）。
- 来源：HEAD 谱系上随 `281c34d8`（2026-08-29，"Rust core" 大导入提交）进入；平行分支 `feat/rust-core` 谱系的 `70f63de8`（2026-09-01，"P0 compatibility fixes — tool params, session layout, output keys"，**未合入 HEAD**）佐证其按 v1 工具参数兼容的动机；无任何提交讨论过移除。
- **文案事实漂移（HEAD）**：`agent_tool.rs:502-525`（`format_background_result`）后台派发结果仍指引 "Use TaskOutput to check progress and retrieve the result."——与 TS v2 的 no-polling 契约相悖；no-polling 对齐修复在**未合并分支** `feat/rust-tool-calling`（`f51213ad`，2026-09-11 "align background-task guidance with the no-polling contract"，含 `suppress_notification` 参数、移除轮询指引）。差距矩阵 G12 行"TaskOutput 支持阻塞等待（task_output.rs:63-74）"的表述在 HEAD 属实。
- 其他暴露给模型的等待机制：无（`tools/` 全部 34 个工具枚举（`tools/mod.rs` `BUILTIN_TOOL_NAMES`）无 Wait/Sleep/Block 独立工具；`AgentSwarm` 为前台同步聚合语义；Bash 后台仅开关无等待参数）。

**goal 模式轮询循环的结构根因（分析性结论，非文案缺陷）**：

- goal 自治循环驱动：turn 以 `reason: completed` 结束 → `turn.ended` 事件同步 enqueue 合成 user 消息（`GOAL_CONTINUATION_PROMPT`，origin `system_trigger: goal_continuation`，admission `newTurn`）（`agent/goal/goalService.ts:146-175, 831-864, 923-956`；step 上限变体 `:177-182`）。该 prompt 及 `goal/` 目录全部注入文案**无任何后台任务措辞**（rg 唯一 "task" 命中为无关的用户任务数据字样）。
- 通知准入交互：任务通知 `activeOrNewTurn`（忙折入当前 turn 的下一步 / 闲自起新 turn，`loopService.ts:196-219`、`stepRequestQueue.ts:38-55`）；goal continuation 为 `newTurn` 且在 `turn.ended` 同步入队、先于 `pumpTurns`（`loopService.ts:506-575` 时序）——通知不会被 goal continuation 吞掉；除 turn.ended 发布与 pumpTurns 之间的极小入队窗口内定局的通知可能延迟一个续航回合外，先后纯由通用准入规则决定（任务在 goal turn 运行中定局则折入该 turn，模型能收到）。
- **轮询循环成因是结构性的**：goal turn 一结束立即注入 "Continue working toward the active goal" 新 turn；若模型判定后台结果是完成目标所必需（如"等子代理结论才能裁决目标"），其仅有的检查手段均为非阻塞快照（`TaskOutput`/`TaskList`，前者每次调用即时返回 `not_ready` 且无动态劝阻）——模型下一步再次调用，直至任务定局；期间任务通知帮不上忙（模型不结束 turn 时通知折入当前 turn 的下一 step，与轮询交错，模型仍按自己的节奏反复确认）。即：**驱动是 goal 续航循环本身 + 缺乏"合法等待"入口/指引，而非任何 prompt 要求轮询**。
- 对应提案：P6（后台任务阻塞等待入口，requirements §17）与 P7（goal 续航与后台任务协调：continuation 提示携带进行中任务清单与勿轮询指引）。

## 7. 重启孤儿治理专项盘点（快照 2026-10-14）

> 起因：2026-10-14 实发事故——agent 在主工作区后台发起全量 rust 测试（带 `disable_timeout` 声明），服务重启后该任务成为无主进程，持续占用 CPU 与编译缓存锁数十小时；同日另有多条 5–20+ 小时级遗留后台任务被任务档案记录证实。本节盘点跨服务代际的进程回收现状，供 SDR-013 / ADR-012（Epic E14）引用。

**Rust（mirri-core / mirri-os）**：

- 进程组隔离：`mirri-os/src/process.rs` spawn 时 `process_group(0)` 使子进程成为组长，kill 语义为整组终止——**组的边界已存在，缺的是跨代际的"谁去对账"**。注：`killpg` 路径（process.rs:96-113）并无负值/零值守卫，现状安全仅靠构造事实（pgid 恒等于存活子进程 pid）；一旦 pgid 从磁盘档案读回再发组信号即跨越信任边界——ADR-012 e 的重验不变量由此而来。
- 任务档案：`task_persist.rs` 持久化状态与输出；`AgentTask` 与 `TaskKind::Process`（task.rs:296-344/104-121）均无 pid/pgid/启动时刻字段——**pid/pgid/启动时刻均未随档案落盘**，重启后档案无法回答"进程还在不在"。
- **加载时调谐**：`TaskManager::with_persistence`（task.rs:613-658）在每次加载时把 Running 任务无条件调谐为 Stopped 并写回；会话恢复路径同律（`session/manager/restore.rs:128-137`），keepAliveOnExit 任务亦然（`session/manager/mod.rs:1334-1336`）——**任何会话被打开，其"未定局"档案即被销毁**，这是普查器设计必须裁决的结构耦合（ADR-012 c）。
- **超时执行者的空档**：调用级超时由 bash 工具层自行武装（`tools/bash.rs:917-946`，`TaskManager::spawn_background_*` 传 timeout=None，deadline 为纯内存字段）；`terminate_expired` 的唯一驱动是 embedded 的 task_driver，**且 driver 随会话消失即退出**（task_driver.rs:135-140）——会话关闭后 keepAlive 存活任务与 mirri-server 部署下，现有超时闸门无执行者（ADR-012 f 的服务级清扫循环由此而来）。
- 服务实例号：`mirri-server/src/state.rs` 每次进程启动生成新 UUID 且已暴露于元数据端点——**代际标识已存在，未与任务档案关联**。
- 心跳过期：`mirri-embedded/embedded_server/task_driver.rs` 的 stale 检测为 warn-only（代码注释明示 "logs — but never kills"），且其管的是"任务健康"不是"进程身份"。
- 既有收敛路径：`stop_all_with_grace`（会话 close 同步收敛，SDR-007）覆盖"服务正常关闭"；`keepAliveOnExit`（SDR 无关，G33）任务在重载时被调谐为 Stopped（档案层）——**物理进程无人过问**。
- 超时：bash 后台缺省 600s（`DEFAULT_BACKGROUND_TIMEOUT_SECS`，TS 对齐），`disable_timeout=true` 可声明不限时；无总寿命上限兜底。

**TS v2（参照基线）**：`background/persist.ts` 的恢复枚举只处理文件损坏（ghost），无孤儿进程回收；`keepAliveOnExit`（`[task]` 节，G33）语义同为"档案保留、进程无主"。**两代引擎在此缺口同构**——TS 无可借鉴的对应机制，E14 为 Rust 侧新设计（TS 侧随引擎退役不再补）。

**跨平台启动时刻来源（可行性确认）**：macOS `proc_pidinfo` 系接口可取进程创建时刻；Linux `/proc/<pid>/stat` 第 22 字段（starttime）为进程创建时刻（时钟滴答）。两者可统一抽象为"存活 + 启动时刻"查询，已有依赖基础（mirri-os 依赖 nix）。

**触发事件证据链（2026-10-14，JST）**：任务档案时间线证实多条超长 detached 任务——04:03→09:09（约 5h）的测试任务、10-12 号遗留的 20+ 小时任务；全部带 `disable_timeout`/no timeout 字样（调用级超时被显式绕开，纯超时机制防不住，需总寿命兜底——SDR-013 d 的直接动因）。
