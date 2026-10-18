# 工具调用 — 现状盘点（Context）

> 快照日期：2026-09-10 · §7 增补：2026-09-12（启用集有序 + 协作工具门控索引）·
> 本文只盘点**当前状态与痛点**，不含目标、不含决策（那些见 [tool-calling-requirements.md](tool-calling-requirements.md) / [tool-calling-architecture.md](tool-calling-architecture.md)）。
> 代码事实基线：v2 = `packages/agent-core-v2/src/`；v1 = `packages/agent-core/src/`；协议 = `packages/protocol/src/`；消费端 = `apps/mirri-code` / `apps/mirri-web` / `apps/mirri-desktop`。
> v1 标记约定：`[v1-source][legacy]` = 仅在 legacy 引擎路径（`MIRRICODE_LEGACY_FLAG` / acp-legacy）存活；已退役面在 §4 明列。

## 1. v2 引擎现状（主基线）

### 1.1 契约模型（`src/tool/toolContract.ts`）

- `ExecutableTool` / `resolveExecution` / `ToolExecution` / `ExecutableToolContext` / `ExecutableToolResult`（Success|Error 二态，`stopTurn/truncated/note/delivery`）、`ToolUpdate`（stdout|stderr|progress|status|custom）、`ToolAccesses`（file/all + conflict 判定）、`ToolSource`（builtin|user|mcp）、`ToolDisclosure`（inline|deferred）、`ToolDefinition/ToolInfo`、`isMcpToolName`（`mcp__` 前缀）——`src/tool/toolContract.ts:1-239`。
- `ToolAccesses.conflict` 语义：任一方 `kind:'all'` 冲突；文件访问**任一方为写即冲突**（写-读、写-写均冲突，仅读-读/读-搜索不冲突）；`recursive` 树前缀 + 路径大小写折叠（`resourceAccessesConflict` :189、`fileOperationsConflict` :195、`fileAccessesOverlap` :213、`normalizePath` :226）。
- `stopTurn / stopBatchAfterThis` 为内部循环控制提示，持久化前剥离（文件头注释 :13-15）。

### 1.2 纯函数层（`src/tool/`）

- `args-validator.ts`：Ajv 三方言（draft-07/2019-09/2020-12，:24-35 关键字探测）、`strict:false, allErrors:true`；错误文案 `must have required property 'x'` / `must NOT have additional property 'x'`、以 `'; '` 连接；兜底 `'Tool parameter validation failed'`（:69-97）。校验器按 `ExecutableTool` 弱引用缓存（`toolExecutorService.ts:73`）。
- `input-schema.ts`：zod v4 `toJSONSchema({target:'draft-7', io:'input'})`（.default 字段仍可选）；`closeObjectNodes` 补 `additionalProperties:false`（:22-53）。
- `path-access.ts`：`WorkspaceConfig {workspaceDir, additionalDirs}`（:25）；`isSensitiveFile`（.env 家族 + SSH 私钥/credentials/.aws/.gcp，公钥与 .env.example 族豁免，:30-66）；`resolvePathAccess` 规范化→`~`展开→词法 canonicalize（不 follow symlink）→`isWithinWorkspace`（:279）；默认 `absolute-outside-allowed` 拒相对路径（:110, :266-277）；字符串 `PATH_SENSITIVE` 文案（:293-301）；敏感检查（`checkSensitive` :293）与越界守卫（`guardMode` :303-318）为两个独立开关——`guardMode:'disabled'` 仅解除工作区外守卫（越界的相对/绝对路径均放行），不影响敏感文件拒绝；`isWithinDirectory` 伪前缀防护（:204）；`extendWorkspaceWithSkillRoots`（:230）；`assertPathAllowed`（:336）。
- `result-builder.ts`：`ToolResultBuilder` 默认 50,000 字符 / 单行 2,000（:11-12），`[...truncated]`（:13），超限追加 `'Output is truncated to fit in the message.'`（:14）；`ok/error` 返回 `{output, isError, truncated, brief?}`（:104/:131）。
- `rule-match.ts`：`globMatch`（picomatch，:30）、`pathGlobMatch`（:43）、`literalRulePattern`（:140）、`matchesGlobRuleSubject/matchesPathRuleSubject`（`!` 否定，:148-171）。
- `tool-args-parse.ts`：`parseToolCallArguments` 失败归一 `{}` + `parseFailed`（:10-29）。
- `toolInputDisplay.ts`：13 种判别联合（:6-83），与 `packages/protocol/src/display.ts:3` `ToolInputDisplaySchema` 同构。

### 1.3 注册与激活（`src/agent/toolRegistry/` `toolActivation/`）

- `IAgentToolRegistryService`（:28）：register/list/listReferences/resolve；`AgentToolRegistryService` Map 实现、同名 last-wins（:31-64）。
- `toolContribution.ts`：`registerAgentToolService` 双重注册（Agent 作用域 DI OnDemand + 贡献数组，:65-77）；options 含 name/description/source/disclosure/when/domain/capabilities（:45）。
- 贡献方：builtin（`agent/tools/**` 自注册 + `session/cron/sessionCronServiceImpl.ts:181`）、mcp（`agent/mcp/mcpService.ts:279,325-332`，`qualifyMcpToolName` → `mcp__<server>__<tool>` :304，冲突跳过 :302-322）、user（`userToolService.ts:107`）、media（`agent/media/registerMediaTools.ts:43-53`，能力门控不走贡献表）。
- `AgentToolActivationService.activate`（`toolActivationService.ts:58-80`）：Profile 策略 + workspace 否决 + when 谓词 → 才构造注册；capabilities 登记（:78-80）；restore 后运行 + `agent.status.updated` 重跑（:51-55）；从不注销（头注释 :15-16）。
- 可见表：`llmRequesterService.ts:722-731` `defaultTools()` / :767-769 `providerVisibleTools`（滤 deferred）。

### 1.4 执行管线（`src/agent/toolExecutor/`）

- `execute`（`toolExecutorService.ts:180`）；preflight（:711-740，未知工具：`Tool "${toolName}" not found` :736）；prepare（:329-447：resolveExecution → isError 合成 :395 → fireBeforeExecute :400-422 → fireAsync(await MCP 就绪) :428 → dispatch `tool.call.started` :438/568-576 → `{accesses, execute}` :440-447）；`ToolScheduler`（`toolScheduler.ts`：非冲突并行 :27-43、冲突排队 :45-52、完成放行 :88）；完成序流回（`executeBatch` :459-499）；finalize（:611-657：onDidExecuteTool、stopTurn :650-653、truncateForModel :657）；telemetry `tool_call`（:306, :878-886）。
- 无 accesses 声明兜底 `ToolAccesses.all()`（:442）；合成结果任务 `none()`（:800）。
- 取消：`raceWithAbortGrace` 2 秒宽限（:69/:912）；用户取消文案（:905-909，见 requirements US2.5）；非用户取消 `Tool "${toolName}" was aborted`（:534-539）。
- 超时转后台：Bash `bashAutoBackgroundOnTimeout`（`bashTool.ts:26-28,154-156`，config 可关）；`Command killed by timeout (…)`（:352）。
- stopBatch：`prepareSkippedToolCall`（:450-454，`'Tool skipped because a previous tool call stopped the turn.'`）。
- 两级截断：工具内 ToolResultBuilder → `toolResultTruncation/toolResultTruncationService.ts:42`（>50,000 落盘 `<homeDir>/…/tool-results/` :59-71，预览 2,000 字符 :84-103，文案 `Tool output exceeded 50000 characters; showing a preview only.`）。
- 归流：loop `loopService.ts:907-963`（recordUnexecutedToolCalls :975-1001、tool.call/tool.result loop 事件、stopTurn → finishReason completed :960-962）；`contextMemory/loopEventFold.ts:175-185` 折叠为 tool 角色消息；中断恢复合成结果（:48-49）。
- 事件面：`toolExecutorEvents.ts:9-41`（tool.call.started/progress/result + beforeToolExecute 两段式 :133-165）。

### 1.5 渐进披露（`src/agent/toolSelect/`）

- `selectToolsTool.ts:33-68`（Loaded/Already available/Unknown tool 文案）；`toolSelectService.ts`：enabled 门控=能力 + `tool-select` 实验旗标（:95-102，`flag.ts:14-25`，默认关）；可动态加载判定 `source==='mcp' || disclosure==='deferred'`（:248-250）；shapeTools（:104-113）/shapeHistory（:125-131）；load 注入 system 消息（:130-157，origin DYNAMIC_TOOL_SCHEMA_VARIANT）；UnavailableToolDescriber 双态文案（:319-331）；公告 `<tools_added>/<tools_removed>`（:162，注入点 `toolSelectAnnouncementsService.ts:49-73`）。

### 1.6 工具策略（`src/agent/toolPolicy/`）

- `ToolCallGuard` preflight 拒绝（`toolPolicyService.ts:39-49`，文案 `Tool "${name}" is disabled by the active tool policy`）。
- 四层组合（`evaluate.ts:68-86`）：workspace 否决 → Profile → 全局 `[tools]`（`configSection.ts:13-20`）→ Session denylist；匹配 builtin/user 精确名、MCP glob（:35-54）；误配检测（:110-130）；`setSessionDisabledTools` 需 profile（:95-103，`PROFILE_NOT_BOUND`）。

### 1.7 用户工具（`src/agent/userTool/`）

- `UserToolRegistration`（`userTool.ts:4-9`）；服务 API register/unregister/list/inheritUserTools（:11-18）；wire Op `tools.register_user_tool` / `tools.unregister_user_tool`（`userToolOps.ts:47-66`）；restore 重建（`userToolService.ts:56-90`）；applyRegister = 注册 + profile.addActiveTool（:92-112）；执行=宿主转发（:122-151，`User tool "${name}" was aborted.` :144-147）；继承（:67-71）。

### 1.8 内建工具清单（`src/agent/tools/`，均经贡献表自注册）

| 工具 | 源文件 | 关键参数/输出要点 |
|---|---|---|
| `Bash` | `os/bash/bashTool.ts:127` | command/cwd/timeout/run_in_background/description/disable_timeout；前台默认 60s/上限 300s、后台默认 600s/上限 24h（`bash.ts:21-24`）；非零退出 `Command failed with exit code: N.`（:369）；后台/转后台返回 task_id 元数据 + next_step（:396-451） |
| `Read` | `os/read/readTool.ts:203` | path/line_offset/n_lines（负偏移读尾）；上限 1000 行/2000 字符/100KB（`read.ts:26-28`）；二进制/图片拒读引向 ReadMediaFile |
| `Write` | `os/write/writeTool.ts:46` | path/content/mode(overwrite\|append)；自动建父目录（`write.ts:21-38`） |
| `Edit` | `edit/editTool.ts:45` | path/old_string/new_string/replace_all；LF 匹配/CRLF 回写（`edit.ts:20-41`） |
| `Glob` | `os/glob/globTool.ts:122` | pattern/path/include_ignored；仅文件、mtime 倒序、上限 100（`glob.ts:44`） |
| `Grep` | `os/grep/grepTool.ts:100` | pattern/path/glob/type/output_mode/-i/-n/-A/-B/-C/head_limit/offset/multiline/include_ignored（`grep.ts:18-103`） |
| `FetchURL` | `fetch-url/fetchUrlTool.ts:29` | url；capabilities `['web.fetch']`（:91） |
| `WebSearch` | `web-search/webSearchTool.ts:38` | query；provider 宿主注入（`web-search.ts:26-34`） |
| `Agent` | `agent/agentTool.ts`（注册 :485） | prompt/description/subagent_type(默认 coder)/resume/run_in_background/model |
| `AgentSwarm` | `agent-swarm/agentSwarmTool.ts:91` | prompt_template({{item}})/items(≤128)/resume_agent_ids |
| `AskUserQuestion` | `ask-user-question/askUserQuestionTool.ts:64` | 1-4 问 × 2-4 选项；background 变体（`ask-user-question.ts:78-95`） |
| `Skill` | `skill/skillTool.ts:46` | skill/args；嵌套深度上限 3（`skill.ts:18`） |
| `CronCreate/CronDelete/CronList` | `cron/*/…` | cron/prompt(≤8KiB)/recurring；每会话 ≤50（`cron-create.ts:17-19`） |
| `CreateGoal/UpdateGoal/GetGoal/SetGoalBudget` | `goal/*/…` | objective/completionCriterion/replace；status(active\|complete\|blocked)；value+unit |
| `EnterPlanMode/ExitPlanMode` | `plan/*/…` | options ≤3（`exit-plan-mode.ts:26-58`） |
| `ReadMediaFile` | `read-media-file/readMediaFileTool.ts:233` | path/region/full_resolution；≤100MB（`read-media-file.ts:16`）；media registrar 能力门控注册 |
| `TaskList/TaskOutput/TaskStop` | `task/*/…` | active_only/limit；task_id；task_id/reason |
| `TodoList` | `todo-list/todoListTool.ts:38` | todos 全量替换/清空/查询 |
| `select_tools` | `select-tools/selectToolsTool.ts:35` | names[]（见 1.5） |

- MCP 工具面：`agent/mcp/tools/mcp.ts:47` `createMcpTool` 三级失败恢复（明确报错→重抛；模糊→ping 探活重试；传输死→重连再试，至少一次，头注释 :5-23）；`agent/mcp/tools/auth.ts:74` 合成 `mcp__<server>__authenticate`（OAuth：status 流式 URL → 阻塞等回调 15 分钟 :45 → 重连换回真实工具）。
- 去重：`agent/toolDedupe/toolDedupeService.ts`（同一步否决重复调用 + 跨步递进提醒，:34-58）。

### 1.9 钩子面（`src/agent/externalHooks/`）

- `externalHooksService.ts:146-161`：PreToolUse 挂 `onBeforeExecuteTool`（veto :151 / rewriteArgs :276-281 / additionalContext）；PostToolUse(-Failure) 挂 `hooks.onDidExecuteTool`（:156-160，输出截 2,000 字符 :296）。

### 1.10 权限门集成点（approval 域边界）

- `agent/permissionGate/permissionGateService.ts:42` 注册 `onBeforeExecuteTool` 监听者；检查点 = resolveExecution 后、execute 前（`toolExecutorService.ts:400`）；`adjudicate`（:52-77）ask→waitUntil / approve→pass / deny→veto。
- 同事件消费方：goal（`goalService.ts:350/:378`）、plan 审批（`planService.ts:112`）、swarm（`swarmService.ts:55`）、btw 顺带问（`btwService.ts:45`）、toolDedupe（`toolDedupeService.ts:171`）、externalHooks（`externalHooksService.ts:148`），以及 MCP 初始加载（另挂 `onWillExecuteTool`，`mcpService.ts:125`）。profile 管线不订阅该事件（角色/策略变化经激活与工具表计算生效）。

### 1.11 后台任务通知（自动再开机制）

- 后台（脱离前台）任务到达终态 → 构建完成通知（任务 kind / 终态 / 标题 / severity=完成 info·失败 warning / 正文 / 输出尾部快照，XML 渲染）→ 以用户角色消息入步骤请求队列（`agent/task/taskService.ts:1121-1149`，构建 :1178-1235）——**有活动回合时并入后续步骤（mergeable 语义可与其它请求合并进同一模型请求，`loop/stepRequestQueue.ts:4-13`），空闲时自动开新回合**（`loop/loopService.ts:179-218` admit 分流 + :221 建回合 + `pumpTurns` :439）。
- 幂等：`task:${taskId}:${status}` 三级 key（building/scheduled/delivered，:1189-1212）+ 已投递集合的 checkpoint 模型（`TaskNotificationDeliveryModel` :124-135，恢复重放判定）；恢复补发（`restoreAgentTaskNotifications` :1151-1176，只补未投递的终态任务）；抑制（`terminalNotificationSuppressed` :1182/:1208）。
- 仅 detached（后台）任务在通知面（:1181，`info.detached === false` 直接返回）；前台超时转后台 = `foreground_detached` 场景（`bashTool.ts:396-402`）——转后台后同样进入通知面。
- 对模型的指引文案（`bashTool.ts:436-450`）：`next_step: The task now runs in the background. You will be automatically notified when it completes — do NOT wait, poll, or call TaskOutput on it; continue with your current work.`（TaskOutput/TaskStop 不可用时简式 :444；后台可用分支另附 later-turn 措辞 + 仅必要时 TaskStop :446-449）。
- 持久化与事件：`taskStarted` / `taskTerminated` wire op（:1104/:1112）；`task.notified` 事件（:1237-1247，外部钩子消费点 `externalHooksService.ts:416`）。
- **去重键**：工具名 + 参数的规范化序列化（递归键排序，字段书写顺序无关；坏文本以原文参与比较避免误折叠）——`makeKey`/`canonicalTelemetryArgs`（`toolDedupeService.ts:78-79`、`_base/utils/canonical-args.ts:5-17`）；同批第二次命中产出占位结果并挂起共享首调结果（`stepDeferreds`），首调丢失时占位兜底文案（:268）。
- 通知快照：先试全量输出快照，不可得时回退尾部 3,000 字节（`NOTIFICATION_FALLBACK_PREVIEW_BYTES` :196，:1198-1201）。
- 同族再开：cron fire 定时注入（`session/cron`，注入文本到达即驱动模型）、后台问询答案到达、steer 合并（stepRequestQueue 头注释 :8-9）。
- 后台任务数量上限：`maxRunningTasks`（任务配置段，env `MIRRI_MAX_RUNNING_TASKS` 可覆盖）**可选、无默认值即不限**；达限时注册后台任务抛 `Too many background tasks are already running.`（`taskService.ts:934-939`，仅约束 detached 任务，前台不限）。
- goal 完成：模型声明即完成（引擎无强制验证，完成产出摘要并停回合——`updateGoalTool.ts:70-75`）。

## 2. v1 遗留层现状 `[v1-source][legacy]`

> 存活面：仅 legacy 引擎路径（`MIRRICODE_LEGACY_FLAG` / `apps/mirri-code/src/cli/sub/acp-legacy.ts:15,38` 硬编码 v1）。

- **注册中心**：`agent/tool/index.ts` `ToolManager`（:49）三 Map（builtin/user/mcp :50-52）；`registerUserTool`（:227，rpc.toolCall 反查宿主）；`registerMcpServer`（:277，经 `qualifyMcpToolName` 产出 `mcp__<server>__<tool>`，与 v2 同构）；`setActiveTools`（:537，MCP glob）；`get loopTools` 可见表（:1035-1094，enabled ∪ disclosure ∥ select_tools ∪ enabled MCP，MCP deferred）；`toolInfos/data`（:748-781）；`runShellCommand`（:140-221，TUI `!` 命令）；`updateStore` 写 `tools.update_store`（:125，resume 可重放）。
- **工具侧 KV**：`tools/store.ts:5-8` `ToolStore`（TodoList 等跨轮状态，落 agent.records）。
- **内建装配**：`initializeBuiltinTools`（agent/tool/index.ts:787-911）——文件/命令/状态/计划/工具选择/协作/目标/网络 + cron/ + background/ 六件套；条件注册：ReadMediaFile 按模型能力（:820）、AskUserQuestion 按 `rpc.requestQuestion`（:836）、Skill 按可调用技能（:844）、Agent/AgentSwarm（:846-900）、WebSearch/FetchURL 按 `ToolServices` 注入（:901-902）、Bash 超时转后台（:814-819）、cron 仅 main agent（:231 关联 CronManager）、Task* 三件套按 BackgroundManager（:226-229，同时是 Bash allowBackground 开关 :802-805）。
- **参数矫正**（仅 legacy 路径）：`tools/arg-converter/index.ts:30-46` `convertToolArgs` 按序应用；`ParameterAliasConverter`（`converters/parameter-alias.ts:72`，参数名别名矫正）与 `NumericStringConverter`（`converters/numeric-string.ts:~150`，字符串数字按 schema 类型矫正）；调用点 loop/tool-call.ts:263/:331（解析后、Ajv 校验前）。**实际别名配置仅一条**（`parameter-alias-config.ts:32-40`）：`Read` 的 `offset→line_offset`、`limit→n_lines`，无 `*` 通配条目；安全约束=仅当别名键非合法参数且映射目标确为合法参数时重映射（`parameter-alias.ts:13-16`），canonical 名已在参数中时别名被丢弃（:119-129）。该机制正是为既定反直觉命名（行偏移/行数参数）打补丁——命名规约见 architecture §1.1，v2 与 Rust 均不引入此机制。
- **校验/策略/展示**：`tools/args-validator.ts`（:10-16 三方言）；`tools/policies/sensitive.ts`（:1-42 敏感文件）、`path-access.ts`（:1-45，PATH_OUTSIDE_WORKSPACE/PATH_SENSITIVE/PATH_INVALID）；`tools/display/schemas.ts`（纯 re-export protocol）；`tools/support/*`（run-rg 子进程管道 :rg + 两阶段 kill、list-directory、git-worktree、image 压缩族、result-builder、workspace.ts、services.ts `ToolServices` 接口）。
- **执行流**：loop/tool-call.ts（参数解析 → convertToolArgs → validateToolArgs → PathSecurityError 拦截 → resolveExecution(approvalRule+display) → PermissionManager 审批门（agent/permission/index.ts:135-165,194） → ToolScheduler（tool-scheduler.ts） → tool.call/tool.result → 折叠回流；中断措辞 abortedToolOutput、未执行调用 `UNEXECUTED_TOOL_CALL_OUTPUT`（tool-call.ts:56-60））。
- **providers**：`tools/providers/moonshot-web-search.ts:39`（Mirri 搜索服务）、`moonshot-fetch-url.ts:32`（coding-fetch 优先，失败回退 localFallback）、`local-fetch-url.ts:196`（SSRF 校验 + Readability）；装配点 `rpc/core-impl.ts:1683-1711` `createRuntimeConfig`。

## 3. 消费端现状

- **TUI**（`apps/mirri-code`，v2 默认）：`tui/components/messages/tool-call.ts:533` `ToolCallComponent`；头部状态动词 Using/Used/Truncated（:1412-1505）+ 关键参数提取（:410-456，60 字符截断，路径保留尾部）+ 数字摘要 chip（`tool-renderers/chip.ts:117-128`）+ 结果渲染器注册表（`tool-renderers/registry.ts:39-69`，Read/Grep/Glob/FetchURL/WebSearch/Bash/Edit/Write/Goal 专用，其余 `renderTruncated` 前 3 行 + `... (N more lines, ctrl+o to expand)`，`truncated.ts:27-101`）；错误结果不走单行 glance 摘要、回落截断渲染器（`summary.ts:29`；折叠态仍为前 3 行+展开提示）；`tool.call.delta` 流式参数预览（:2035-2091）；`tool.progress` status 行 ≤24 行（:757-768，URL OSC 8 超链接 :1550-1568）/ stdout·stderr 尾窗保 3 行、50,000 字符上限（:770-781/:1570-1586）；交互：Ctrl+O 展开（editor-keyboard.ts:234-237）、Esc/Ctrl+C 中断（:123-212 → `cancelCurrentStream` → `session.cancel()`）、Ctrl+B detach（:316-325，前台 Bash >10s 提示 `DETACH_HINT_DELAY_MS` tool-call.ts:52-54）；`!` 命令 ShellRunComponent（mirri-tui.ts:990-1074）；子代理卡（`tool-call.ts:1745-1934`，spinner/统计/2 行窗/高度稳定）+ AgentGroup（agent-group.ts:174-176）+ ReadGroup（read-group.ts:1-21）；max_tokens 截断 notice（session-event-handler.ts:379-393，`'Model hit max_tokens — tool call was truncated before it could run.'`）；事件分发表（session-event-handler.ts:242-282,505-595）；`tool.list.updated` 显式忽略（:280）；`ToolInputDisplay` 仅进审批适配器（tui/types.ts:70、reverse-rpc/approval/adapter.ts:20-28）。
- **Web**（`apps/mirri-web`，v2 only）：`components/chat/ToolCall.vue:24-40` 渲染器解析（Media/Edit/Agent/Swarm/AskUser/Generic，`tool-calls/toolRegistry.ts:15-27`）；`ToolRow.vue:44-80`（图标+名+参数摘要+状态指示 running/ok/error/suspended+chip+耗时+展开；错误整卡红边）；`GenericTool.vue`（展开体=全参数摘要+逐行输出，无输出 `Waiting for output…`）；`EditTool.vue`（chip +A −B、右侧 diff 面板/文件预览）；`AgentTool.vue`（desc/type/prompt/最终输出 + `Open detail` 实时面板）；`SwarmTool.vue`（运行中默认展开、成员手风琴）；`AskUserTool.vue`（选项列表渲染/已取消）；`MediaTool.vue`（内联媒体，标题 文件名·mime·大小·尺寸）；`ToolGroup.vue`（连续工具堆叠组，聚合状态）；数据源：`api/daemon/agentEventProjector.ts:837-918`（tool_use_started/delta/completed 投影；**`tool.call.delta` web 侧显式忽略** :872-875；`duration_ms` 客户端未渲染 :910）、`composables/messagesToTurns.ts:668-712`（历史 → ToolCall、normalizeToolOutput/Media；悬空 running 沉降 ok :612-633）；快照重连 `WireInFlightToolCall`（:694，刷新后输出线不中断）；Esc 中断（ConversationPane.vue:1166-1190）。
- **desktop / kap-server**：v2 only（`apps/mirri-desktop/src/main/ensure-server.ts:262-331` 拉 `mirri web`；kap-server deps 仅 agent-core-v2，`routes/tools.ts:1-50` 说明 v1 全局单例被 v2 Agent-scoped 替代）。
- **node-sdk**：v1 类型/事件 re-export（`events.ts:1-114`，含 `CronFiredEvent`:108）；`rust-engine/translate.ts:1116-1192` 把 Rust 引擎 ToolInputDisplay 线格式归一为 SDK 类型（类型借 v1、值来自 Rust）；`v2/session-wiring.ts:33,83` 桥回 base 注册表。
- **apps/vis**：类型级 + 一个协议版本常量 re-export（`server/src/lib/agent-record-types.ts:5-27` 类型、:27 常量、手工镜像 cron 类型 :36-40）。

## 4. 已退役 / 无消费面（不入基线承诺）

| 面 | 事实 |
|---|---|
| `services/tool/`（IToolService/ToolService/BUILTIN_TOOL_DESCRIPTORS，services/tool/tool.ts:65-107） | 全仓零外部消费者（仅包自身注册与文档提及）；kap-server /tools 已基于 agent-core-v2 重写 |
| `services/approval/`（IApprovalService broker，services/approval/approval.ts:66,91） | 仅被同为孤儿层的 sessionService.ts:118 与 coreProcess{Service,Client}（coreProcessService.ts:18/:73/:246、coreProcessClient.ts:9/:20/:27）引用；全仓无外部消费者，v1 server 遗物 |
| v1 `services/` 进程内服务层整体 | 无 services 层外消费者 |
| v1 运行时审批 `PermissionManager`（agent/permission/index.ts） | 仅在 legacy 引擎路径活跃（node-sdk rpc.ts:824 + TUI approval/adapter 消费仍存在，但引擎面是 v1） |

## 5. 覆盖差异矩阵（需求文档 vs 代码现状）

| 需求条目 | 代码现状 | 差异 |
|---|---|---|
| E1 可用面（US1.1–1.3） | v2 契约/注册/激活全在 | 无差异 |
| E2 执行（US2.1–2.9） | executor 全链在；MCP 断连恢复（US2.8）为 `mcp__` 工具内建（createMcpTool 三级恢复） | 无差异 |
| E2 执行（US2.11 后台自动通知与再开） | 机制完整在（§1.11：用户侧消息注入 / 空闲自动开新轮 / 幂等 / 恢复补发 / 抑制 / 勿轮询指引） | **原基线文档未披露**（US4.6 曾以"读取指引"措辞误导为拉模式）——2026-09-10 复核补录并修正 |
| E2 执行（US2.10 数字形态参数值宽容） | 当前主引擎无此机制；legacy 路径有 `NumericStringConverter`（§2，字符串→数字，作用于解析后/校验前） | **用户决策新增为基线需求**（超出当前主引擎现状）：Rust 需实现；TS 侧由 legacy 机制承载、主引擎未覆盖 |
| E3 结果（US3.1–3.6） | 两级截断、chip、合成结果、去重全在 | 无差异 |
| E3 结果（US3.7 错误反馈自修正闭环） | 错误文案大多含定位+原因+指引（builtin-tools 各错误面表全录）；**少数兜底文案无下一步指引**（如 `Glob failed`、`Failed to stop task: {id}`、TodoList 无错误分支） | **部分超出 TS 现状**：US3.7 为原则化收编 + 对无指引兜底文案的增强要求，Rust 需补强 |
| E4 内建工具集 | 27 工具 + MCP/media 面全在（§1.8） | 「Think」渲染器为上游继承的转录兼容残留（2026-09-10 调查关闭，见 ROADMAP F-3） |
| E5 用户工具 | userToolService 全在 | 无差异 |
| E6 渐进披露 | toolSelect 全在，实验旗标默认关 | `MIRRICODE_EXPERIMENTAL_TOOL_SELECT` 默认关闭（文档已按"默认不生效"表述） |
| E7 策略 | toolPolicy 四层在 | 无差异 |
| E8 非功能 | 上限/事件/截断在；web `duration_ms` 未渲染、web `tool.call.delta` 显式忽略 | 见 §6 痛点清单 |

## 6. 已知痛点 / 差距（消费端与跨引擎）

- **web 工具级耗时未渲染**：wire `duration_ms` 存在但客户端未赋值（projector :910 注明 unused at client level；唯一赋值点在后台任务面板 useMirriWebClient.ts:1852-1864）；TUI 普通工具也无耗时显示（仅子代理有 elapsed）。
- **web 忽略 `tool.call.delta`**（:872-875）：参数在 started 事件已全量，无损但失去流式参数形态。
- **TUI `Think` 渲染器无对应 v2 工具**：`tool-renderers/registry.ts` 登记了 Think 渲染器（render-truncated 分支）——已查证为 fork 上游继承的转录兼容残留（本仓引擎历史无此工具，保留/删除无风险），见 ROADMAP F-3。
- **历史命名遗迹（已澄清）**：已退役的 services/tool 层曾用 `mcp:` 单冒号命名（services/tool/tool.ts:58-59）；live 路径 v1 与 v2 一致为 `mcp__<server>__<tool>`，无跨引擎映射负担（2026-09-10 评审核实）。
- **v1 arg-converter / 当前引擎无对应**：当前引擎无参数名别名矫正与数字字符串矫正（依赖模型正确性 + Ajv 校验拒绝）；该行为已升格为已裁决的参数名别名机制（requirements §9 P-2 / architecture ADR-1，独立设计决策，Rust 侧执行）；本清单与该机制的差异判定以 context §2 记录为准。另：Rust/TS 会话存储隔离但支持单项迁移（2026-09-10 用户确认），别名承担单项迁移兼容。
- **web 悬空 running 沉降为成功**：网页端对结果缺失的工具（如回合被中断）强制沉降为"成功"，与终端"已中断"呈现不一致——R4 失真信号，跟踪见 [tool-calling-ROADMAP.md](tool-calling-ROADMAP.md) F-10。
- **Rust 侧已知 TS 缺口**（rust-core-design ROADMAP 记录）：TS v2 无原生 `tool.call.delta` 块级事件面差距已被 Rust 补上（批 8b）；无 `tool.progress` 引擎级通道 Rust 已补（批 13）——TS 侧 `tool.progress` 事件存在但对齐面有限。
- **v2 快照重放语义**：web 经 `WireInFlightToolCall` 快照恢复运行中工具；TUI 经会话日志回放（loopEventFold 折叠）——两套恢复面并存（跨端一致性风险点）。
- **`tool.list.updated` 消费**：TUI 显式忽略、web 未见消费（事件面存在但工具集合变化 UI 无通知——与 US6.2 公告（模型侧）不同，用户侧无"工具集合变化"提示）。

## 7. Rust 侧现状（一句话索引，不重复）

Rust 内核（17 crates）工具域已有实现与对齐跟踪：RPC 面 `tool.list/call`、`agent.getTools` 与用户工具管理 `tool.listUserTools/setUserToolEnabled`（`register_user_tool` wire op 已闭环 2026-09-07）；事件面 `tool.call.delta`/`tool.progress` 已转正（对应 TS 缺口）；`ToolManager.enabled_tools_explicit` 白名单、`WorkspacePaths` 沙箱 allowlist（Read/Write/Edit/ReadMediaFile）、`tool_result_truncation`、`ToolInputDisplay::Diff` 映射等——详情以 [rust-core-design/ROADMAP.md](rust-core-design/ROADMAP.md) 与 [plan/rust-v2-method-parity.md](../../plan/rust-v2-method-parity.md) 为准（ROADMAP 已一句话引用，本文不复制）。

2026-09-12 增补（`03c3ecb1`/`929983d9`）：**启用工具集稳定有序**——初始启用与运行期再启用均按字典序（`tool_registry/mod.rs:166,248-252`），工具增删公告（US6.2 形态）跨重启字节稳定，不击穿 provider 提示词缓存；**协作工具模式门控**——7 个 `Collaboration*` builtin 注册但默认对 LLM 隐藏，`collaboration.enter/exit` 宿主 RPC 整组开关（产品裁决与现状详见协作域 [collaboration-requirements.md](collaboration-requirements.md) SDR-003 / [collaboration-context.md](collaboration-context.md) §2）。