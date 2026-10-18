# 多 Agent 团队协作域现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-04 · §2 更新：2026-09-12（协作工具模式门控批 `03c3ecb1`/`08bbd989`/`929983d9`）·
> 配套设计文档：[collaboration-requirements.md](collaboration-requirements.md)（US）·
> 本文不描述目标、不含裁决。

## 1. TS v2 引擎的现有协作能力

**Agent 域 wire 面**（klient 契约）：`klient/src/contract/agent/rpc.ts` 的 prompt/steer/activateSkill/listCommands/runCommand/cancel/undoHistory/setPermission/cancelCompaction/activatePluginCommand/getContext/getTools；域服务契约含 `agentProfileContract`（getModel/setModel/getThinking）与 `agentTaskContract`（list/stopByUser/stop/readOutput）。~~wire 上有 `swarm.enter/exit/get`~~（**勘误（2026-09-12，B0）**：不实——客户端契约无此 RPC；TS 侧蜂群控制经 REST prompt 的 `swarm_mode` 标志（kap-server）+ node-sdk 进程内服务调用，方法名仅存在于 Rust 引擎映射，详证见 §5 与 §7-C）；**没有 agent.create**——主 agent 由 session.create 急切供给，子 agent 由工具内部创建。

**Swarm 模式**：`agent/swarm/swarm.ts` `IAgentSwarmService`（enter/exit/restoreEnter，trigger='manual'|'task'|'tool'）——纯状态标记 + 提醒注入 + turn 结束自动退出 + "AgentSwarm 必须独占一次 tool batch" veto。不改变任何协作语义。

**Subagent 机制**：`Agent` 工具（prompt/description/subagent_type 默认 coder/resume/run_in_background/model）；后台模式依赖 TaskList/TaskOutput/TaskStop 工具，由 `session/subagent/subagent-task.ts` + `app/task/taskService.ts` 支撑。`SessionSwarmService` 批量调度（create/run）；父子关系是 `parentAgentId` 元数据标签 + 所有权校验，非独立 session。事件面：`subagent.spawned/started/suspended/completed/failed`（`packages/protocol/src/events.ts`）。

**Agent 定义**：`packages/agent-profile/src/schema.ts` `AgentFileDef`——name/description/whenToUse/defaultModel/`tools`（工具白名单，省略=全部）/`subagents`（可派发的子 profile 白名单）/capabilitiesRequired。**只有工具约束，没有职责/协作角色概念**。

**agent 间通信**：无直接通道。子 agent 结果经工具 XML 返回给父 agent 转述（"A subagent's result is only visible to you"）；除父子外无 peer 通信。

## 2. Rust 侧（mirri-collaboration 与 mirri-core）

**mirri-collaboration**（5 src 文件 + 749 行测试，附 Draft 设计文档 `docs/superpowers/specs/2026-08-28-autonomous-collaboration-design.md`）：
- `domain.rs`：CollaborationMode / RoleDefinition（role name/description/system_prompt/capabilities/max_agents）/ CollaborationRules（Sequential|Parallel|Hierarchical、InterventionPolicy、ConflictResolution、max_rounds）
- `protocol.rs`：AgentMessage 8 种（task_assignment/task_result/question/answer/conflict_escalation/intervention/status_update + Unknown 前向兼容）+ TaskStatus/AgentStatus
- `session.rs`：CollaborationSession 状态机（NotStarted/InProgress{round}/Paused/Completed/Failed）；AgentHandle 的 agent_id 是**合成 id**（`{role}-{uuid}`），不连真实 AgentScope
- `manager.rs`：纯内存 `RwLock<HashMap<Uuid, Session>>`——只有台账与校验，**不驱动任何真实 agent 执行**
- 接入：mirri-core 依赖之，`tools/collaboration.rs` 注册 7 个 builtin 工具（CollaborationStart/SendMessage/GetMessages/GetState/Intervene/Escalate/Terminate，`COLLABORATION_TOOL_NAMES` 见 `tools/collaboration.rs:58`），经 session 作用域共享 `CollaborationManager`。**无持久化、permission 层对协作工具无策略**（见 §4-7）
- **模式门控（2026-09-12，SDR-003）**：7 个协作工具**注册但默认对 LLM 隐藏**——`tools/mod.rs:350-352` 注册后立即 `set_builtin_tool_enabled(name, false)` 整组关闭；宿主经 `collaboration.enter` / `collaboration.exit` RPC（mirri-proto 注册表 `rpc/registry.rs:465-466`，both faces，payload 镜像 swarm 的 `{session_id}`）进入/退出模式，handler（`mirri-embedded/src/embedded_server/session_handlers.rs:3304/3336`）同时翻转 `SessionScope.collaboration_mode()`（`permission/mod.rs` 的 `CollaborationModeState`，会话级 AtomicBool、不持久化，`session_scope.rs:442`）与工具组开关。`session.status` 面板回报 `collaborationMode`（`session_handlers.rs:787`，与 planMode/swarmMode 并列）
- **工具契约可发现性（2026-09-12）**：`CollaborationSendMessage` 参数 schema 以 `anyOf` 载荷给出 8 种消息形态的完整结构（兜底分支放行结构非法的消息、由引擎前置校验给出精确错误）；未知 `type` 报错列全部合法类型（`collaboration.rs:369-376`）、发件人不在名单时报合法成员；`CollaborationStart` 返回 `{session_id, agents}`（角色名清单，`collaboration.rs:175-180`），后续消息 from/to 必须落在该名单内
- **工具表顺序稳定（2026-09-12）**：`ToolManager` 启用集以排序序（字典序）暴露并在运行期再启用时维持有序（`tool_registry/mod.rs:166,248-252`）——工具变化公告跨重启稳定，不击穿 provider 提示词缓存
- **web 接线（2026-09-12，`08bbd989`）**：Mode 选择区（plan / goal / swarm / collaboration 四并列，`Composer.vue:899-929`）显式开关 Collaboration，全链路镜像 swarm（RPC 调用、`session.status` 回读、本地持久化偏好 `useMirriWebClient.ts:114`、i18n、移动端设置页）

**mirri-core swarm/subagent**：`swarm.rs` `SwarmModeService`（per-session）；embedded 端 `swarm.enter/exit/get` handler。`subagent/host.rs` `SessionSubagentHost`：spawn=profile→AgentScopeConfig→create_agent→新 TurnFlow；**子 agent `config.subagent_host = None`（无嵌套子 agent）**。

**registry agent.* 行**：`agent.getContext/getUsage/getModel/setModel/setThinking/setPermission`（both）、`swarm.enter/exit/get`（both）、`collaboration.enter/exit`（both，2026-09-12 起——协作模式门控，payload 镜像 swarm）、`agent.get/list`（rust-first）、`agent.activateSkill/activity/getEffectiveThinkingLevel/listCommands/runCommand`。无任何 `subagent.*` 行。

## 3. v1 遗产（packages/agent-core）

> **勘误（2026-09-12，B0）**：原标题「已退役」精确化为——v1 **server 包**已删除，但 CLI 旧引擎旗标路径（`MIRRICODE_LEGACY_FLAG`）仍可进程内实例化 v1 引擎（v1 Agent/AgentSwarm/SwarmMode 仍可被激活，无默认消费路径），详证见 §7-C/§8。

`agent/swarm/index.ts` 与 v2 同构（v2 是其直系移植）；`tools/builtin/collaboration/` 目录名下的 agent.ts/agent-swarm.ts 即 v2 工具组源头；`session/subagent-host.ts` 发同样的事件。**没有 swarm 之外的团队/任务共享概念**；可回收概念（trigger 语义、resume、后台任务）均已移植。

## 4. 团队模型视角的差距速览

1. **角色/职责**：profile 只有工具白名单；rust 协作角色的 RoleDefinition 不绑任何调度逻辑（agent_id 是合成占位）
2. **分工**：只有"父拆任务→子执行"（Agent/AgentSwarm，128 上限并发 5）；无引擎自动组建团队/按角色分派
3. **流转**：wire 上任务只有 agent 作用域的 task.list/stop/readOutput；无跨 agent 任务转移、无 task_assignment/task_result 真实投递
4. **交接**：子 agent 结果只能经父转述；无 peer 直接交接、无跨会话状态继承
5. **看板/共享状态**：无共享任务视图；CollaborationSession 的 message_log/state 在内存中，无 wire、无持久化、无 UI 事件
6. **通信**：协议层有 8 种消息类型定义与内置工具，但与真实 agent 执行完全脱钩；冲突仲裁（human/main_agent/majority_vote）仅有枚举定义
7. **约束**：swarm 模式仅做提醒注入与唯一性 veto；permission 层对协作工具无任何策略（2026-09-12 的模式门控是**工具面可见性**控制——`set_builtin_tool_enabled` 整组开关 + `collaboration.enter/exit` 宿主 RPC，不是权限裁决；`CollaborationModeState` 自述"无 permission 策略消费它"，见 `permission/mod.rs`）

## 5. v2 引擎蜂群机制细节补录（2026-09-12 快照）

> 本节为 §1 的增量细化（§1 原文保留），登记 2026-09-12 recon 逐行核验的代码事实。

**Swarm wire Op 与持久化恢复**
- `packages/agent-core-v2/src/agent/swarm/swarmOps.ts:18-25`：`SwarmModel` 为 `SwarmModeTrigger | null`（trigger 原样保留不折叠为布尔）；`swarm_mode.enter` / `swarm_mode.exit` 注册进 `PersistedOpMap`——**蜂群状态跨重启恢复靠 journal 回放**，非 `restoreEnter`。
- `swarmOps.ts:27-37`：enter op schema `{trigger}`、exit op schema `{}`；两者 `toEvent` 均发 `agent.status.updated{swarmMode:true/false}`；该事件为 volatile（`packages/protocol/src/events.ts:1547-1555`，不落 journal）。
- `swarm.ts:5-13`：`IAgentSwarmService`（isActive/enter/restoreEnter/exit）——**无 `get()` 方法**。
- `swarmService.ts:86-89`：`restoreEnter` 仅派发 op、无 reminder、无事件；**v2 全代码无调用方**（仅 v1 `packages/agent-core/src/agent/records/index.ts:87` 调用，v2 靠 wire 回放恢复）。

**进入/退出/自动退出/批次 veto**
- `swarmService.ts:75-84`：enter 幂等（model 非 null 即返回）；仅 `trigger !== 'tool'` 时注入进入提醒（`appendSystemReminder`，variant `swarm_mode`）。
- `swarmService.ts:47-53,119-122`：订阅 `turn.ended`；trigger 为 `task`/`tool` 时自动 `exit()`，`manual` 不退出。
- `swarmService.ts:54-72,133-148`：AgentSwarm 独占批 veto——`toolExecutor.onBeforeExecuteTool` 统计批内 `AgentSwarm` 调用数：`0` 或（`1 且 batch.length===1`）放行，否则整批 `denyToolExecution`（工具审批拒绝形态）；"多个 AgentSwarm 同批"与"AgentSwarm+其它工具混批"两类场景各有固定文案模板（多 AgentSwarm 场景按是否混批追加提示）。
- `swarmService.ts:91-113`：exit 时检查历史末条是否为进入提醒（willPop）——在则发 live-only `context.spliced`（start=history.length-1, deleteCount=1）弹出，否则追加退出提醒（variant `swarm_mode_exit`）；live/replay 双侧一致（:8-14 文档注释）。
- 提醒内容：`agent/swarm/` 下 `enter-reminder.md`（swarm 工作法：探索→分派→AgentSwarm `prompt_template`+`items`，最多 128 子 agent、尽量细分、避免冲突）与 `exit-reminder.md`。

**三条触发路径**
- tool：`agent/tools/agent-swarm/agentSwarmTool.ts:145-146`（执行时 `swarmMode.enter('tool')`）。
- manual：REST prompt 的 `swarm_mode` 标志（`packages/protocol/src/rest/prompt.ts:51`、`packages/kap-server/src/protocol/rest-prompt.ts:43`）→ `app/sessionLegacy/sessionLegacyService.ts:128-134`（enter('manual')/exit）。
- task：`packages/node-sdk/src/sdk-rpc-client-v2.ts:1571-1575`（`setSwarmMode({trigger:'task'})` 后立即 prompt）；`setSwarmMode`（:1561-1569）进程内直调 `IAgentSwarmService`。

**AgentSwarm 工具约束（引擎侧）**
- 契约 `agent/tools/agent-swarm/agent-swarm.ts:17-63`：`MAX_AGENT_SWARM_SUBAGENTS=128`；`prompt_template` 须含 `{{item}}`；`resume_agent_ids: Record<agentId, prompt>`；`model` 只作用于新 spawn（resume 的子 agent 保持自身 model）。
- `agentSwarmTool.ts:238-338`：items 最少 2（有 `resume_agent_ids` 时例外）；总数 >128 报错；展开后 prompt 重复即拒绝；结果渲染 `<agent_swarm_result><summary>completed/failed/aborted</summary>` + 逐 `<subagent agent_id item state outcome>` + 有未完成时 `<resume_hint>`。
- swarm 子 agent **不注册为后台任务**（`agentSwarmTool.ts:208` `runInBackground:false`，直接 await 整批；后台任务通知路径不覆盖蜂群）。
- 并发 ramp 与限流：`session/swarm/agentRunBatch.ts:46-54`——首批立即启动 5 个、其后每 700ms 放行 1 个；env `MIRRICODE_AGENT_SWARM_MAX_CONCURRENCY` 设硬上限（:54,639-651）；限流退避 3s×2、容量收缩/恢复间隔 2000ms/3min；**唯一未完成任务被限流时直接判 failed 而非无限重试**（:372-379）；用户取消时未完成任务标 aborted（started/not_started 区分，:531-559）。
- **Rust 落地（2026-11，B3）**：上述引擎侧约束已在 mirri-core 对齐——`subagent/swarm_batch.rs` 调度器实现同款起步斜坡（首 5 + 700ms/个，launch 计数独立于并发槽）、每尝试超时、取消三分（aborted + started/not_started、TS 双文案逐字）、429（不含 billing quota）同身份 requeue（`subagent.suspended` 事件 + `TurnFlow::retry_one_turn` 无新 prompt + 3s×2 退避 + 容量收缩/恢复 2s/3min、批内不回退、唯一未完成终局 failed）；env 主名 `MIRRI_AGENT_SWARM_MAX_CONCURRENCY`（兼容 TS 拼写、非法值 fail-soft——SDR-B3-01）；swarm 成员**同样不注册 TaskManager**（与 TS `runInBackground:false` 同口径，结果经工具结果一次性交付）。实现事实见 [subagent-context.md §6.11](subagent-context.md)。

**嵌套与权限（§1/§4 未覆盖的增量事实）**
- **TS v2 子代理可嵌套**：coder 与主 `agent` profile 的工具表均含 Agent/AgentSwarm（`session/agentLifecycle/profile/profiles.ts:38-39,52-53`）；`explore` profile 不含（:76-84）；主 agent 无 subagents allowlist（:104-107，可派任意 catalog profile）。——Rust `subagent/host.rs` 的子 agent **无嵌套**，属 Rust 侧差距（ROADMAP O2）。
- 权限默认放行：`agent/permissionPolicy/policies/default-tool-approve.ts:19-20`——Agent/AgentSwarm/TaskList/TaskOutput 均在 `DEFAULT_APPROVE_TOOLS`（manual 模式免确认，可被用户规则覆盖）；TaskStop 不在集合内。
- 子代理权限继承：spawn 时继承 caller 权限模式与 user tools（`agentTool.ts:308-311`、`sessionSwarmService.ts:172-177`）；主 agent `setPermission` 广播到全部 agent（`agent/rpc/rpcService.ts:145-147`）。

**事件/契约/路由增量事实**
- 事件：`packages/protocol/src/events.ts:638-675`——subagent.* 五种（spawned 含 model 解析链/parentToolCallId/swarmIndex/runInBackground；suspended 的 reason 为 Provider rate limit 重排队）；`:1513-1517` 总注册表——**无 swarm.*/collaboration.* 事件类型**。
- klient：`packages/klient/src/contract/agent/rpc.ts:253-278` agent 域 12 方法（无 swarm/collaboration）；`agentTaskContract` 在 `contract/agent/services.ts:47-61`（list/stopByUser/stop/readOutput，`agentTaskInfoSchema` :230-239 为 v1 兼容 snake_case）；**`swarm.enter/exit/get` 不存在于 klient**——方法名仅出现在 Rust 引擎映射 `packages/node-sdk/src/sdk-rpc-client-rust.ts:320-322` 与 `agent-core-v2/src/agent/rpc/core-api.ts:158-160`（`EnterSwarmPayload` 在 v2 无使用方）。（§1 对应表述的勘误见 §7-C。）
- kap-server：`src/routes/tasks.ts`（GET 任务列表 / GET 任务详情 / POST `:cancel`；kind `agent→subagent`、status `timed_out|lost→failed`、`killed→cancelled` 映射 :307-336；`agent_id` 透传供 REST 行与 WS `subagent.*` 行 join :375-381）；**无 swarm 路由**（蜂群控制唯一入口是 REST prompt 的 `swarm_mode` 标志）；WS 侧 `transport/ws/v1/subagentRosterTracker.ts` 重连时靠缓存的 `subagent.spawned` 重建蜂群卡片；transcript 折叠 `swarm_mode.enter/exit` Op 为 `swarm.enter/exit` 标记（`packages/transcript/src/history/foldFacts.ts:420-427`）。
- 后台终态通知（对照项，蜂群不走此路径）：`agent/task/taskService.ts`——detached 任务终态以 `<notification>` XML 合成 user 消息入列（admission `activeOrNewTurn`；agent 任务失败通知附 Agent(resume) 恢复指引）。

## 6. 消费端现状（2026-09-12 快照）

**web（apps/mirri-web）——双后端与接线**
- 引擎选择：`src/api/index.ts:4-14` 两个 transport——`DaemonMirriWebApi`（v1/v2 REST+WS）与 `WsRpcApi`（v3=Rust，单通道 JSON-RPC）；按 `GET /meta` 的 `backend` 字段自动选择（`"v3"`→WsRpcApi），`VITE_MIRRICODE_BACKEND` 可强制。**web 并非"仅 v2"。**
- Mode 选择区：`Composer.vue:899-955`（plan/goal/swarm/collaboration 四并列开关 + 模式 pill 显示激活标签；:925-926 注释明确 collaboration 行为 "functional client toggle (opt-in exposure of the Collaboration* tools)"）。逻辑 :668-706。
- 开关链：`useWorkspaceState.ts:2226-2235`（setSwarmMode：per-session map + localStorage + persistSessionProfile）；`:2238-2250`（toggleSwarmMode：manual 权限先弹确认 `workspace.swarmEnableConfirm`）；`:2255-2273`（setCollaborationMode/toggleCollaborationMode——**无 manual 确认框**，与 swarm 的差异点）；无会话时暂存 draft、首会话转移（:1309-1310）；每 prompt/steer 携带模式字段（:1706-1720、:1853-1863）；side chat 继承（`useSideChat.ts:235-236`）。
- 本地持久化：`storage.ts:14-15`（`mirri-web.swarm-mode` / `mirri-web.collaboration-mode`，per-session map）；会话删除清理（`useMirriWebClient.ts:641-642`）。
- 状态回读/投影：`useMirriWebClient.ts:679-696`（GET `/sessions/{id}/status`）+ `agentEventProjector.ts:948-975`（`agent.status.updated` → per-session map，reducer `useMirriWebClient.ts:888-898`）。
- **v2 引擎不消费 `collaboration_mode`**：`packages/kap-server/src/protocol/rest-prompt.ts:32-50` 与 `packages/agent-core-v2/src/app/sessionLegacy/sessionProtocol.ts:39-51` 的 schema 只有 `swarm_mode`（zod 剥离未知键）；statusResponse（:80-90）无该字段——**连 v2 时 collaboration 开关是纯客户端 UI 状态**（仅 Mode pill 标签与 /status 面板显示，无任何引擎效果）。v3 下经 `ws-rpc-api.ts:1208-1217` 调 `collaboration.enter/exit` RPC（且 v3 的 `message.send` 目前丢弃 per-prompt 模式字段，:646-649——模式切换只能走独立 RPC）。
- 斜杠命令：`slashCommands.ts:29` + `App.vue:616-624`——`/swarm`（裸=toggle；`on|off`=显式；`<task>`=开+发）。**无 `/collaboration` 命令**（SLASH_COMMANDS 全表 :24-39）。web 斜杠路径的三个差异事实：① `<task>` 变体经 `setSwarmMode` 直通、无 `trigger` 概念——引擎侧进的是 manual 蜂群，`/swarm <task>` **回合结束不自动退出、无"已结束"标记**（task 触发源仅 TUI 经 SDK `setSwarmMode({trigger:'task'})` 生效）；② `on`/`<task>` 变体直调 `setSwarmMode`、manual 权限下**无确认框**（裸 `/swarm` 与 Mode 菜单同走 `toggleSwarmMode`、有确认）；③ 模式设置为 fire-and-forget（`void` 不阻塞、开关乐观翻转不回退）；失败**非静默**——经通用操作失败告警呈现（`persistSessionProfile` catch → `pushOperationFailure`，`useMirriWebClient.ts:766-774,1305-1320`），无专用原因文案。
- 呈现：SwarmTool 卡片（`toolRegistry.ts:24` `agentswarm`；运行中默认展开；done/total 计数 + 五色 phase 分段（completed/working/suspended/failed/queued）+ 逐 subagent 手风琴；结束后从 `<agent_swarm_result>` 文本解析（`parseSwarmResult.ts`）；REST 刷新与 live 合并防闪烁（`swarmCardRows.ts`/`swarmGroups.ts`/`taskMerge.ts`））；dock 面板 "Sub Agent (N)"（`ChatDock.vue:230-238`）+ TasksPane + AgentDetailPanel；/status 面板 swarm/collaboration 各一行 on/off（`StatusPanel.vue:94-101`）；移动端开关（`MobileSettingsSheet.vue:291-307`）。@-mention 仅文件提及（`useMentionMenu.ts`）。i18n：`en/status.ts:18-22`、`zh/status.ts:18-22`（zh 中 Swarm/Collaboration 均保留英文）。

**TUI（apps/mirri-code）**
- `/swarm` 命令：`src/tui/commands/registry.ts:171-179`（`idle-only`，参数补全 on/off）+ `commands/swarm.ts:15-46`（无 session 报错 `NO_ACTIVE_SESSION_MESSAGE`；on/off 幂等提示 "already on/off"；裸命令切换；`<task>` 先权限确认再发）；manual 权限三选项确认 `swarm-start-permission-prompt.ts:13-49`（Switch to Auto and start / Switch to YOLO and start / Start in Manual，附行为差异说明；取消还原命令文本 `swarm.ts:54-57`）；设置失败报 "Failed to enable/disable swarm mode: \<err\>"（:134-136）；底层 `session.setSwarmMode(enabled, trigger∈{manual,task})`（:126-142）。
- 可见性：footer 状态栏蜂群徽标（`footer.ts:259`）；转录标记（`swarm-markers.ts:8-32`：'Swarm activated'/'Swarm deactivated'/'Swarm ended'——task 触发自动退出时渲染，`session-event-handler.ts:598-618`）；会话恢复回读 `session.getStatus().swarmMode`（`mirri-tui.ts:1499-1515`）。
- AgentSwarm 呈现：`agent-swarm-progress.ts`（live 网格：逐 subagent cell、braille 进度、phase 文案、80ms 动画；swarm 有活动 AgentSwarm 工具调用时活动 spinner 入网格（`mirri-tui.ts:2598-2608`））；结果摘要 'Agent swarm: <segments>'（`tool-call.ts:2140-2242`）；中断时标记 cancelled（`session-event-handler.ts:334,409-428`）。后台 subagent 走 `/tasks` 浏览器（`tasks-browser.ts`）。
- **TUI 无 collaboration 入口**（`src/` 全目录 "collaboration|collab" 零匹配；@-mention 为文件提及 `file-mention-provider.ts`）。

**desktop（apps/mirri-desktop）**
- Electron 薄壳，自身零协作功能（主进程仅 server 托管/tray/deep-link/updater 等）；`src/main/ensure-server.ts:266-301` spawn 捆绑 CLI SEA 执行 `mirri web`（v2 kap-server，:331-332 注释明示不用 v1 server）；`MIRRICODE_RUST_ENGINE=1` 时 `mirri web` 改 spawn Rust `mirri serve`（`apps/mirri-code/src/cli/sub/web/run.ts:248-283`）。用户可见的协作 UX = 捆绑 web UI（随 web 构建版本走）。

**多人类协作存在性（全仓核验）**：apps/ + packages/ TS 侧**无** real-time collaboration / shared session / comments / @人 mentions / presence / 邀请链接 / 团队管理；无 yjs/CRDT/OT 依赖痕迹。"share" 仅出现于 "shared daemon" 语境；"mention" 全部为文件提及。TS 侧 "collaboration" 的真实语义 = v1 工具分类目录名（`packages/agent-core/src/tools/builtin/collaboration/`：agent/agent-swarm/ask-user/skill-tool）。

### 6.1 web 消费端 B10 增量（2026-09-13 快照，D-24/D-25/D-26 修复后）

> 本小节按落地后事实增量记录（上文 2026-09-12 快照描述的三个差异事实已被本批修复，原快照保留作历史）。批次：B10（`plan/batches/B10-collab-web-consistency.md`）。

- **四路径确认统一（D-25①）**：`/swarm` 变体解析抽为纯函数 `lib/swarmCommand.ts:12`（toggle/on/off/task 四态）；`App.vue:620-625` 改走之。所有开启路径收敛到 `enableSwarmMode`（`useWorkspaceState.ts:2266`，manual 权限弹 `workspace.swarmEnableConfirm` 确认、返回 boolean 供 task 变体判定取消）：Mode 菜单/移动端开关（`toggleSwarmMode:2280`→enable）、裸 `/swarm`（同）、`/swarm on`（`App.vue:622`）、`/swarm <task>`（`runSwarmTask:2296` 先 `await enableSwarmMode()`，取消即整体中止）。
- **开关失败回退（D-25②）**：`setSwarmMode:2234`/`setCollaborationMode:2331` 乐观翻转 + `await persistSessionProfile(...)`（fire-and-forget 改为可等待），false 时回退 per-session map + localStorage；失败 toast 走专用标题（`persistSessionProfile` 第三参 `opts.title` 透传 `pushOperationFailure`，facade 内单次推送）：`warnings.swarmModePersistFailed` / `warnings.collaborationModePersistFailed`（en/zh 双写）。同态调用幂等跳过 RPC。collaboration 仍无 manual 确认（与 swarm 的既有差异保留）。
- **`/swarm <task>` 语义 + 「已结束」divider（D-25③）**：`runSwarmTask:2296` = 确认 → 开**手动**蜂群 → 记录 transient `swarmTaskBySession[sid]` → 发首条指令（忙时入队，镜像 createGoal；直发失败清 entry）。**不**自动翻转开关。「已结束」由 web 推导（v3 引擎无 task 触发源、无 turn 结束自动退出）：`maybeAppendSwarmEndedMarker`（`useMirriWebClient.ts:969`）挂主回合 `turnActiveChanged(false)` 与 `taskCreated/taskCompleted` 两类事件（inFlight+turnActive 守卫防早触发；seq 派生 marker id `swarm_ended_<sid>_<seq>` 防重放重复；仅已加载消息的会话追加）；判定纯函数 `lib/swarmEndedMarker.ts:17`（主回合结束 && 无 running 子代理 && task 进入）；渲染走 compaction divider 同构通道（`messagesToTurns.ts` `role:'swarmEnded'` divider turn → `ChatPane.vue` `.compact-divider` 复用 + `conversation.swarmEnded` en/zh）。**生命周期定位：实时转录增强**——marker 不进服务端 journal，会话快照重载后自然消失（compaction divider 有服务端 summary 支撑可跨快照，本 marker 无；持久化需引擎侧 trigger wire，留作 R-6 末段扩展锚点）。
- **collaboration 按 backend 能力呈现（D-24）**：纯函数 `lib/backendCapabilities.ts:8`（仅 v3 支持）；`collaborationSupported` computed（`useMirriWebClient.ts:2145`）经 App.vue 透传——Composer 菜单行 + pill、MobileSettingsSheet（移动端镜像）、StatusPanel dt/dd 行全部 `v-if` 隐藏（v1/v2）；`setCollaborationMode`/`toggleCollaborationMode` 在不支持后端 no-op，`sessionUsageUpdated` 与 `refreshSessionStatus` 回读同样 gate（防 v2 旧状态残留复述）。swarm 开关不受影响。v2 引擎侧 schema 剥离 `collaboration_mode` 为既定现状（DEC-5），本批未改 TS 引擎。
- **v3 per-prompt 模式字段显式丢弃（D-26）**：`ws-rpc-api.ts:656-679` submitPrompt 把 `thinking/permissionMode/planMode/swarmMode/collaborationMode/goalObjective/goalControl` 集中进 `satisfies Record<keyof Pick<PromptSubmission,...>>` 钉住列表后显式 `void`——有意丢弃可见可查（注释指向 updateSession 的独立 RPC 路由），字段从 `PromptSubmission` 消失即编译失败。v1/v2 REST 通道不受影响（仍消费 per-prompt `swarm_mode`）。

## 7. 文档 ↔ TS 现状校对矩阵（2026-09-12）

**A. 设计增强（文档优于 TS 现状——原样保留，不削弱、不向下对齐）**

| 既有文档条目 | TS 现状 | 处置 |
|---|---|---|
| requirements E1–E7 全部（团队组建/共享任务板/角色通信/仲裁/团队观测/团队持久化/模型统一） | 无任何实现（无 peer 通信、无共享任务板、无角色协作语义） | 保留为设计增强；实施待 Q1–Q5 裁决 |
| requirements §8 N1–N7（团队 NFR） | 无实现 | 保留为设计增强 |
| requirements SDR-003（协作工具默认隐藏 + 显式模式暴露） | TS 侧无对应实现（TS 引擎无协作工具组、无 collaboration 模式）；Rust 侧已落地（§2） | 保留；TS 现状缺口登记为 ROADMAP O5 |

**B. 回归/缺口（TS 支持但既有文档未保证——本次以 requirements E8 补齐基线）**

| TS 已支持能力 | 既有文档覆盖 | 本次处置 |
|---|---|---|
| 蜂群模式 wire Op 持久化、重启无声恢复 | 未记录 | E8 US8.5 + 本文件 §5 |
| 手动权限下蜂群启动确认（web 确认框 / TUI 三选项） | 未记录 | E8 US8.3 + 本文件 §6 |
| 模式可见性（Mode pill / footer 徽标 / 状态面板 / 移动端开关） | 未记录 | E8 US8.4 + 本文件 §6 |
| `/swarm` 单任务命令 + 自动退出 + "Swarm ended" 标记 | 未记录 | E8 US8.2 |
| 批次独占 veto（两类混批整批否决） | context §1 一句话 | E8 US8.6 + 本文件 §5 细化 |
| 进入指引收回（willPop 弹出）语义 | 未记录 | E8 US8.1 + 本文件 §5 |
| 模式偏好按会话独立 + 删除清理 + 首会话转移（web） | 未记录 | E8 US8.7 + 本文件 §6 |
| 子代理嵌套（TS 子代理可再派子代理） | 未记录；Rust 侧无嵌套 | 本文件 §5 + ROADMAP O2（Rust 差距） |
| Agent/AgentSwarm 默认权限放行 | 未记录 | 本文件 §5 + ROADMAP O3（跨域核对项） |

**C. 勘误（既有表述与代码不符——原文按增量原则保留未改，在此登记，修订待发起方裁决）**

| 位置 | 既有表述 | 代码事实（2026-09-12 核验） |
|---|---|---|
| context §1 | "wire 上有 swarm.enter/exit/get" | klient 契约无此 RPC；TS 侧蜂群控制经 REST prompt 的 `swarm_mode` 标志（kap-server）+ node-sdk 进程内服务调用；`swarm.enter/exit/get` 方法名仅存在于 Rust 引擎映射（`sdk-rpc-client-rust.ts:320-322`） |
| context §3 | "v1 遗产（packages/agent-core，已退役）" | 精确化：v1 server 包已删除，但 CLI `MIRRICODE_LEGACY_FLAG` 路径仍进程内实例化 v1 引擎（`experimental-v2.ts:69-76` → `sdk-rpc-client.ts:138` `createMirriHarness`，v1 进程内客户端），v1 Agent/AgentSwarm/SwarmMode 仍可被激活；无默认消费路径 |
| requirements 头部 | "collaboration-architecture.md（待需求确认后撰写）" | 本批已新建该文档（按 append-only 原则原文未改） |

> **处置落地（2026-09-12，B0 勘误批次）**：以上三处已按「就地精确修正 + 保留勘误标注」处理——§1 的 swarm RPC 表述与 §3 的 v1 退役表述已在原位标注勘误（删除线保留原文、括注给出正确事实）；requirements 头部配套文档注记已修正为现行链接（见该文件头部勘误注）。本表登记保留作勘误台账；ROADMAP O7 关闭。

## 8. v1-source 与消费引擎小结（2026-09-12）

- **无"v1-only 且被默认消费"的协作能力**：v1 Agent/AgentSwarm/SwarmMode 在 v2 有语义同构的独立实现（v2 为 DI service + wire Op 持久化，v1 为类内状态 + record；v2 额外有批次 veto）；web/desktop/CLI 的默认引擎均为 v2（或 Rust）。
- v1 残余消费均非协作用途：acp-adapter（仅类型与模型别名）、apps/vis（record 类型只读镜像）、e2e（logger 工具）。v1 `services/task` 是无消费方的孤儿代码。v1 独有语义 `restoreEnter` 的调用方也仅在 v1 恢复路径（v2 靠 wire 回放）。
- 消费引擎选择：web=服务端决定（v2 daemon 或 v3 Rust，`/meta` 自动检测）；desktop=捆绑 `mirri web`（v2，可经 env 切 Rust）；CLI=优先 Rust（`MIRRICODE_RUST_ENGINE`）> v2（默认）> v1（`MIRRICODE_LEGACY_FLAG`，非默认）。
