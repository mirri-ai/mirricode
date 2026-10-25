# 权限与审批（approval）— 现状盘点（context）

> 快照日期：**2026-09-12**。本文档为现状清单：代码事实（file:line）、消费端盘点、覆盖差异矩阵与痛点。不含目标与裁决（见 [approval-requirements.md](approval-requirements.md) / [approval-architecture.md](approval-architecture.md)）。行号以快照日代码为准，会随演进而漂移。

## 1. 代码地图（v2 引擎，`packages/agent-core-v2/src`）

### 1.1 裁决层

| 能力 | 代码事实 |
|---|---|
| 权限策略链（13 策略，静态有序） | `agent/permissionPolicy/permissionPolicyService.ts:48-62`（链序），`:65-73`（首个表态即返回） |
| 策略接口 / 裁决形态 | `agent/permissionPolicy/types.ts:40-65`（`PermissionPolicyResult`: approve/deny/ask，ask 可带 `resolveApproval`/`resolveError` 延续）；`:9-21`（`ApprovalRequest`/`ApprovalResponse`：`decision/scope/feedback/selectedLabel`）；`:23-26`（`PermissionData { mode, rules }`） |
| `auto-mode-approve` | `agent/permissionPolicy/policies/auto-mode-approve.ts:14-16`（auto → approve） |
| `auto-mode-ask-user-question-deny` | `policies/auto-mode-ask-user-question-deny.ts:15-23`（auto 下 AskUserQuestion 拒绝 + "Make a reasonable decision and continue without asking the user"） |
| `default-tool-approve` | `policies/default-tool-approve.ts:7-30`（默认放行清单：Read/Grep/Glob/ReadMediaFile/SetTodoList/TodoList/TaskList/TaskOutput/CronList/WebSearch/FetchURL/Agent/AgentSwarm/AskUserQuestion/Skill/EnterPlanMode/ExitPlanMode/CreateGoal/GetGoal/SetGoalBudget/UpdateGoal/select_tools） |
| `deny-all`（btw 用，opt-in） | `policies/deny-all.ts:21-48`（enable/disable + 自定义消息，reason `source: 'side_question'`） |
| `fallback-ask` | `policies/fallback-ask.ts:9-11` |
| `git-control-path-access-ask` | `policies/git-control-path-access-ask.ts:27-47`；路径判定 `policies/path-utils.ts:25-44`（`.git` 组件 / worktree 控制目录） |
| `git-cwd-write-approve` | `policies/git-cwd-write-approve.ts:24-51`（posix + Write/Edit + 全写路径在工作区内 + cwd 是 git worktree） |
| `sensitive-file-access-ask` | `policies/sensitive-file-access-ask.ts:12-15`；敏感清单在 `tool/path-access.ts:30-60`（`.env`/`id_rsa*`/`credentials`/`.aws|gcp credentials` 等 + `.example` 类豁免——清单本身属 tool-calling/execution 边界，本域只消费判定结果） |
| `session-approval-history` | `policies/session-approval-history.ts:16-39`（遍历 `sessionApprovalRulePatterns`，匹配 → approve，遥测带 `has_rule_args`/`match_strategy`） |
| `user-configured-{deny,ask,allow}` | `policies/user-configured-rule.ts:20-59`（scope 过滤 `turn-override/project/user`，deny 消息带 reason）；三个入口文件同目录 |
| `yolo-mode-approve` | `policies/yolo-mode-approve.ts:14-16`（链位在用户 ask/敏感/git 之后） |
| 权限门（事件接线） | `agent/permissionGate/permissionGateService.ts:42`（注册 `onBeforeExecuteTool`），`:52-77`（adjudicate：ask → `:66` `waitUntil` 冷工厂；approve → `pass`；deny/result → 折叠），`:79-98`（`authorize` 主动裁决）；`deny` 消息经 `toolApproval.formatDenyMessage` |
| 两遍裁决事件 | `agent/toolExecutor/beforeToolExecuteEvent.ts:1-23`（机制文档），`:42-44`（`denyToolExecution`），`:74-99`（veto/allow/pass/waitUntil/rewriteArgs + 窗口断言） |

### 1.2 模式（permissionMode）

| 能力 | 代码事实 |
|---|---|
| 模式类型与默认 | `agent/permissionMode/permissionModeOps.ts:15-20`（`PermissionModeModel` 初始 `manual`；`permissionMode.configured` 标记显式设置） |
| set_mode Op（持久） | `permissionModeOps.ts:28-31`（`permission.set_mode` 入 PersistedOpMap） |
| setMode 幂等 + 变更事件 | `agent/permissionMode/permissionModeService.ts:44-50` |
| auto 进入/退出提醒注入 | `agent/permissionMode/injection/permissionModeInjection.ts:41`（注册 `permission_mode` 注入变体）、`:53-70`（lastMode 对比 + 压缩后重申；提醒文本 `permission-mode-auto-enter/exit-reminder.md`） |
| 默认模式配置节 | `agent/permissionMode/configSection.ts:18-20`（`defaultPermissionMode`: manual/auto/yolo） |
| RPC setPermission + 广播 | `agent/rpc/rpcService.ts:139-156`（主代理 `broadcastPermissionMode`；`yolo_toggle`/`afk_toggle` 遥测） |

### 1.3 规则（permissionRules）

| 能力 | 代码事实 |
|---|---|
| 规则模型 | `agent/permissionRules/permissionRules.ts:13-22`（decision/scope/pattern/reason；scope: `turn-override|session-runtime|project|user`） |
| wire Model + 双 Op | `agent/permissionRules/permissionRulesOps.ts:27-73`（`permission.rules.add` transient 不持久——"v1 不持久规则，宿主恢复时重给"；`permission.record_approval_result` persisted，approved+session+pattern 去重入 `sessionApprovalRulePatterns`） |
| pattern 解析/匹配 | `agent/permissionRules/matchesRule.ts:31-55`（`Tool` / `Tool(arg)` / 括号错误抛错）、`:59-82`（工具名 picomatch + `execution.matchesRule(argPattern)` 判定；解析失败不匹配） |
| `[permission]` TOML 节 | `agent/permissionRules/configSection.ts:59-117`（deny/allow/ask 列表 + `tool`+`match` 简写归一；`permissionToToml` 写回时删旧列表键；`:38-42` pattern 校验入 schema——非法 pattern 使配置加载失败） |
| 服务门面 | `agent/permissionRules/permissionRulesService.ts:30-45` |

### 1.4 审批往返（toolApproval）

| 能力 | 代码事实 |
|---|---|
| 合同 | `agent/toolApproval/toolApproval.ts:21-42` |
| 往返主流程 | `agent/toolApproval/toolApprovalService.ts:101-216`：请求组装 `:106-127`；无中介 auto-approve `:131-133`；`permission.approval.requested` 事件 `:135`；abortable + 中止传播 `:137-141`；错误结局 `:142-168`（error 事件 + 遥测 + `resolveError` 延续）；会话规则提取 `:171-174`（approved+session 时取 `execution.approvalRule`）；resolved 事件 `:175-181`；`recordApprovalResult` `:182-189`；`permission_approval_result` 遥测 `:190-205`（含 `duration_ms`/`approval_surface`/`session_cache_written`/`has_feedback`） |
| 拒绝/取消文案 | `toolApprovalService.ts:218-241`（rejected 与 cancelled 分别措辞、feedback 附加）；worker 指引 `:253-255`（非 main 代理附加 "Try a different approach — don't retry..."） |
| 事件契约 | `toolApprovalService.ts:38-59`（`PermissionApprovalRequestContext`/`ResultContext` 进 `DomainEventMap`） |

### 1.5 特殊裁决面

| 能力 | 代码事实 |
|---|---|
| 计划审查 | `agent/plan/exitPlanModeReview.ts:39-59`（拦截非空 `plan_review`，origin `exit-plan-mode-review-ask`）；auto 跳过守卫在 `agent/plan/planService.ts:118-122`（`mode !== 'auto'` 才 `waitUntil` 审查）；结局折叠 `:61-141`（批准→`plan.exit()`+计划全文+变体指令；Revise→保持+反馈；Reject and Exit→退出+`stopTurn`；dismiss→保持） |
| 目标启动确认 | `agent/goal/goalService.ts:351-373`（manual/yolo + `goal_start` 展示才弹；批准后按 `selectedLabel` 切模式） |
| 蜂群排他 veto | `agent/swarm/swarmService.ts:56-77`（同批 >1 个 AgentSwarm 或混合调用 → deny 消息） |
| 外部钩子（PermissionRequest/Result） | `agent/externalHooks/externalHooksService.ts:158-176`（订阅审批事件 fire-and-forget，脱壳载荷）；事件类型表 `agent/externalHooks/types.ts:3-20`（16 类钩子事件含 `PreToolUse` 可 veto） |
| wire 迁移 1.1→1.2 | `wire/migration/v1.2.ts:20-57`（旧批准记录补 `sessionApprovalRule`：`run command→Bash`、`edit file→Write` 等映射；`run command in plan mode`/`run background command` 不可恢复不推导） |

### 1.6 中介层（session 作用域）

| 能力 | 代码事实 |
|---|---|
| interaction 内核合同 | `session/interaction/interaction.ts:17`（kind: `approval|question|user_tool`）、`:48-59`（request/enqueue/respond/listPending/isRecentlyResolved/cancelPendingForTurn + 双事件） |
| 内核实现 | `session/interaction/interactionService.ts:45-46`（最近已解决 TTL 60s / 上限 256）；`cancelPendingForTurn` `:93-108`（`{ cancelled: true, reason: 'turn_ended' }`）；request/enqueue `:110-118`；respond `:120-129`；park `:146-163`（id 缺省 `interaction-N`）；journal `:165-195`（请求/应答 Op 写到 `origin.agentId ?? 'main'` 的 wire；无容器时跳过） |
| journal Op（持久） | `session/interaction/interactionOps.ts:27-82`（`interaction.request`/`interaction.resolved`；头注释明确"不恢复挂起 Promise，无应答按取消折叠"） |
| approval 门面 | `session/approval/approval.ts:13-40`（`ISessionApprovalService`: request/enqueue/decide/listPending）；`session/approval/approvalService.ts:22-55`（id 缺省 `req.toolCallId` 再退 `toolName:Date.now()`） |
| question 门面 | `session/question/question.ts:20-56`（`QuestionResult = null | 答案 Record | { answers, method }`；dismiss=null） |

## 2. 消费端盘点

### 2.1 嵌入式 SDK（`packages/node-sdk`）

- `src/rpc.ts:832-849`（会话级处理器表 set/clear）；`:853-878`（`requestApproval`：无 handler → `cancelled` + "No approval handler registered."；handler 异常 → `cancelled` + "Approval handler failed." + `SESSION_APPROVAL_HANDLER_ERROR` 错误事件）；`requestQuestion` 同构（无 handler → `null`）。
- `src/session.ts:95-97`（`setApprovalHandler`）、`:209-217`（`setPermission`）。
- 无头模式：`apps/mirri-code/src/cli/run-prompt.ts:390-393`（`installHeadlessHandlers`：审批一律 approved、问询一律 null）。

### 2.2 ACP 适配器（`packages/acp-adapter/src`）

- `approval.ts:21-23`（规范 optionId：`approve_once`/`approve_always`/`reject`）；`:64-72`（canonical 三选项，排序理由注释）；`:94-118`（plan_review 特化：`plan_opt_<i>`/`plan_approve`/`plan_revise`/`plan_reject_and_exit`，拒绝侧排序 Revise → Reject and Exit——与 TUI 的 Reject → Revise 顺序及选项集均不同，见 §5 缺口 7）；`:147-179`（响应映射 + 兼容旧客户端 `approve`/`approve_for_session` + 未知 id → rejected 安全缺省）；`:200-225`（plan 选项 label 映射，`selectedLabel` 驱动下游）；`:256-281`（权限卡片：`${turnId}:${rawId}` 前缀关联工具卡、diff/plan 头条、恒附 "Requesting approval to <action>"）；`:303-324`（`attachSelectedLabel`）。
- 问询桥接：`question.ts:25-68`（`q{n}_opt_{i}`/`q{n}_skip` 选项命名空间、label 原样回传、尾部 Skip 即无作答、无「其他」自由文本入口）；`session.ts:1286-1366`（复用 requestPermission 面；多问题降级只问第一题 + 遥测 `question_degraded` 记录丢弃数；多选降级单选；RPC 失败 catch `:1357-1364` → null 无作答，不编造答案）。
- `modes.ts:28-52`（四模式 default/plan/auto/yolo + 默认档）；`:93-107`（`acpModeToToggles`：plan=计划模式+manual，auto/yolo 直映射）。

### 2.3 Web（`apps/mirri-web/src` + `packages/kap-server/src`）

- REST：`packages/kap-server/src/routes/approvals.ts`（文件头 1-30 行内注释完整记录：GET pending / POST resolve；错误 40401/40404/40902/40001；60s 幂等窗 → 40902 `resolved:false`；`expires_at` 派生 `created_at+24h`（APPROVAL_EXPIRY_MS）；`tool_call_id`/`session_id` 缺失兜底）；问询同构 `routes/questions.ts`；路由注册 `routes/registerApiV1Routes.ts:21`。
- 前端：`apps/mirri-web/src/composables/useMirriWebClient.ts:1030-1032`（`approvalRequested` 事件入每会话待决表）、`:2181-2187`（`pendingApprovals` computed）、`:2195-2202`（状态优先级 awaiting-approval 最高）、`:2549+`（侧栏待关注计数 = 审批+问询）、`:1679+`（`buildApprovalBlock`）；卡片 `components/chat/ApprovalCard.vue:119-139`（approve/approveSession/reject/feedback/plan 变体/Revise/Reject and Exit）、`:142-165`（数字快捷键）。
- WS：`packages/kap-server/src/transport/ws/v1/events.ts:82` + `protocol/events-zod.ts:500-503,583`（`awaiting_approval` 会话状态）。

### 2.4 TUI（`apps/mirri-code/src/tui`）

| 能力 | 代码事实 |
|---|---|
| SDK 接线/会话切换取消 | `mirri-tui.ts:1568-1571`（切换会话：注销旧 handler + `approvalController.cancelAll`）、`:1711-1714`（reload 同理）、`:1587`（新会话装 handler）；`:1524/1526`（auto/yolo 快捷 setPermission） |
| 启动模式 | `mirri-tui.ts:191-201`（CLI 选项 → 启动模式）、`:703`（startup permission） |
| 审批适配器 | `reverse-rpc/approval/adapter.ts:7-12`（默认四选项）、`:13-17`（计划拒绝项：Reject → Revise，**无 Reject and Exit**——与 ACP/web 的拒绝侧选项集不同，见 §5 缺口 7）、`:61-144`（generic 参数推导预览块）、`:214-230`（8 种高危命令模式：递归删除/sudo/管道进 shell/dd 写盘/mkfs/写裸设备/chmod 777/fork 炸弹）、`:344-388`（选项特化：plan/goal）、`:155-174`（面板响应 → SDK 响应，`approved_for_session`→session） |
| 面板组件 | `components/dialogs/approval-panel.ts:191-206`（工具特定标题）、`:261-318`（Esc/Ctrl+C/Ctrl+D→rejected、Ctrl+E 预览、Ctrl+O、feedback 模式）、`:340-343`（≤5 块 + brief 去重 `:176-189`）、`:397-407`（提示行）、`:40-42`（diff/内容 10 行上限） |
| 队列与自动继承 | `reverse-rpc/base-controller.ts:35-45`（一次一个按序排队）、`:48-57`（respond 推进）、`:59-67`（cancelAll）、`:83-94`（drainAutoResolved）；`reverse-rpc/approval/controller.ts:14-26`（session 批准继承：同 `action`、丢弃 feedback/selectedLabel） |
| 模态互斥 | `reverse-rpc/modal-coordinator.ts:68-89`（approval/question owner 互斥串行；active 同主替换分支 `:76-80` 承担同类接续——控制器 respond→advance 队列非空时不再 hide 直接 show 下一个；当前双控制器接线下不可达的是 queued 同主替换分支 `:82-86`——排在协调器队列期间其 owner 不会再发起 show） |
| 问询面板 | `components/dialogs/question-dialog.ts:138-141`（Ctrl+O 切换工具输出，与审批面板一致）；`:133-135`（Ctrl+C/Ctrl+D → 空答案） |
| 处理器 | `reverse-rpc/approval/handler.ts:6-23`（异常兜底 cancelled） |
| 模式命令 | `commands/config.ts:116-208`（`/yolo`、`/auto` on/off/toggle + 提示文案）、`:613-627`（`/permission` 选择器挂载）、`:748-763`（applyPermissionChoice 幂等提示）；命令注册 `commands/registry.ts:151-153` |
| 模式选择器文案 | `components/dialogs/permission-selector.ts:5-21`（Manual/Auto/YOLO 描述） |
| goal/swarm 启动确认 | `commands/goal.ts:342-400`（manual/yolo 均弹，含失败回滚原模式）；`commands/swarm.ts:37-44`（**仅 manual 弹**，yolo/auto 直接下发）；组件 `components/dialogs/goal-start-permission-prompt.ts:14-62`（manual 四项含「Start in Manual」，yolo 三项无此项）、`swarm-start-permission-prompt.ts:12-50`（三项 + Esc 取消） |
| 提示语 | `constant/tips.ts:43-44`（/auto、/yolo 提示） |

## 3. v1 侧（legacy）代码事实

- 协议审批/问询中介：`packages/agent-core/src/services/approval/approval.ts:66-91`（`IApprovalService`: request/resolve/listPending；id 关联键 = `toolCallId`）、`:97-135`（`toBrokerRequest`：daemon ULID `approval_id`、**`expires_at = createdAt + 60s`**）、`:142-151`（`toAgentCoreResponse` snake↔camel 适配；头注释声明这是唯一 protocol↔SDK 翻译点）。
- v1 问询协议面：`services/question/question.ts:64-97`（request/resolve/dismiss/listPending）、`:106-172`（wire 合成 `q_<i>`/`opt_<i>_<j>` id、恒 `allow_other=true`）、`:201-262`（5 态答案联合回翻译为扁平 Record；未知 id 原样保留；`click` method 进程内丢弃）。
- v1 引擎裁决器：`agent/permission/index.ts:28-80`（`PermissionManager`：parent 链规则共享 + `modeOverride`；`set_mode`/`approval_result` 写 records + replayBuilder）。v1 策略目录 `agent/permission/policies/`（与 v2 同构 + v1 特有：`plan-mode-guard-deny`、`plan-mode-tool-approve`、`agent-swarm-exclusive-deny`、`swarm-mode-agent-swarm-approve`、`pre-tool-call-hook`）。
- **甄别结论（按 DOMAINS 要求核对 authSummary）**：`services/authSummary/`（authSummary.ts / authSummaryService.ts）与 `services/auth/managedAuth.ts` 属登录状态摘要与托管认证，**与审批展示无关**，归 auth-oauth 域（v2 对应面 `agent-core-v2/src/app/auth/`）。本域不文档化其内容。

## 4. 与相邻域文档的引用关系（核对结果）

- tool-calling 域已覆盖：工具可用面四层组合与误配检测（`tool-calling-context.md:52`）；审批刻面生成（`tool-calling-architecture.md:110`：「工具在解析阶段生成审批刻面……（审批面见 approval 域）」）；边界声明（`tool-calling-requirements.md:8`：「审批与权限流 UX 归 approval 域」）——与本域边界一致，无重叠、无缺口。
- toolContract 上的审批相关字段：`tool/toolContract.ts:79-84`（`accesses`/`display`/`approvalRule`/`matchesRule`——刻面合同，归 tool-calling）；本域经 `execution.approvalRule`（`toolExecutorService.ts:556`）消费。
- conversation-rendering 域覆盖审批历史的转录呈现；session-management 域覆盖 btw 投递机制（本域只覆盖其 deny-all 审批面）。

## 5. 覆盖差异矩阵（recon 时无既有文档 → 全量首建）

| 能力 | 代码现状 | 文档覆盖 |
|---|---|---|
| 权限模式三档语义/切换/广播/提醒 | ✅（§1.2） | requirements US1 |
| 策略链 13 面与优先序 | ✅（§1.1） | US1.1/US2 |
| 审批往返（事件/遥测/worker 指引） | ✅（§1.4） | US4/US11 |
| 会话级批准记忆 + wire 迁移 | ✅（§1.3/§1.5） | US5 |
| 用户规则 [permission] 配置 | ✅（§1.3） | US6 |
| 计划审查 / 目标 / 蜂群特殊流 | ✅（§1.5） | US7 |
| 问询用户闭环 | ✅（§1.6/§2） | US8 |
| TUI 面板/队列/命令 | ✅（§2.4） | US3 |
| web 卡片/REST/WS | ✅（§2.3） | US9.1/US9.5 |
| ACP 选项/模式 | ✅（§2.2） | US9.2/US1.4 |
| SDK handler/无头缺省 | ✅（§2.1） | US9.3/US9.4 |
| 错误/取消/超时路径 | ✅（§1.6/§2.1） | US10 |
| 外部钩子 PermissionRequest/Result | ✅（§1.5） | US11 |

**发现的回归 / 缺口**（TS 支持但此前无文档保证——首建即披露）：

1. **yolo 仍保留高危问询**（用户 ask 规则、敏感文件、git 控制路径在 yolo 下仍弹审批）——与直觉的「yolo=全放行」不同，链序（`yolo-mode-approve` 位于第 10 位）是有意设计；auto 才是全放行。此前无任何文档记录此差异，Rust 若按字面理解 yolo 会丢掉这条安全防线。
2. **auto 模式下用户 ask 规则被忽略**（`auto-mode-approve` 先于 `user-configured-ask`）——同样是链序导出的非显然语义。
3. **v1 wire 的 60s 审批过期 vs v2 无过期**：v2 web 路由以派生 24h 值填充 wire 字段（保真缺口，kap-server 注释明示有意为之）。
4. **旧会话批准记录的刻面回填**（migration v1.2 的 action→pattern 映射与「不可恢复动作宁可多问」原则）。
5. **btw 子代理的 deny-all 是 opt-in 而非常开**（`deny-all.ts` 默认 disabled）。
6. **ACP 问询面的静默降级**：多问题批次降级为只问第一题（其余丢弃，仅遥测 `question_degraded` 留痕）、多选降级为单选——用户可感知但无任何用户面文档披露。
7. **计划审查拒绝出口的跨端不一致**：TUI 拒绝侧只有 Reject/Revise（普通 Reject 走引擎默认分支：保持计划模式+终止回合），无「Reject and Exit」；ACP 与 web 提供显式「Reject and Exit」（退出计划模式+终止回合）。同一动作在两端可达结局不同。
8. **启动确认的触发条件差异**：goal 启动确认在 manual 与 yolo 下均弹；swarm 启动确认仅 manual 弹（yolo/auto 直接下发）——同族交互行为不对称。

**残留未盘点项**：`apps/mirri-desktop` 的审批消费面（以 recon 核对为准，本次未发现其独立审批实现，消费经由 SDK/web 面）；`services/authSummary` 的 v1 协议细节留给 auth-oauth 域；敏感文件内建清单条目（`tool/path-access.ts:30-60`）归 tool-calling/execution 边界。

## 6. 现状痛点（recon 观察，供 Rust 参考）

- 会话级批准记忆按代理维度隔离（`permissionRules` 为 Agent 作用域）：子代理各自审批、各自记忆，同一会话内主/子代理可能对同刻面重复弹审批（v1 曾有 parent 链共享规则）。**后记（2026-09-13，B9 Rust 轨）：Rust 引擎已修复为会话内跨代理共享（见 §7.1）；本条描述的 TS v2 现状不变。**
- web wire 的 `expires_at` 是派生值而非真实语义（见 §5 缺口 3）。**后记（2026-09-13，B9 Rust 轨）：Rust 引擎侧核验无任何 `expires_at` 派生（见 §7.4）；web wire 层清除由 B9-Web 轨落地。**
- TUI `/yolo` 提示文案「asks for approval on risky ones」依赖链序实现，无独立规格约束（验收项见 approval-requirements.md US1.1）。
- 用户级规则只能手写 TOML，审批面板无「升级为永久允许」入口（已列为增强提案）。**后记（2026-09-13，B9 Rust 轨）：引擎侧已提供 `approval.listRules/addRule/removeRule` CRUD RPC 与规则接线（见 §7.2）；web 的「始终允许」升级入口与管理面板由 B9-Web 轨落地。**

## 7. B9 落地快照增量（2026-09-13，Rust 引擎轨）

> B9（审批规则管理与诚实性）Rust 轨落地后的引擎侧事实增量。§1–§3 的 TS v2 现状快照不变；本节是 Rust 引擎（`rust/crates/*`）的现行事实源。web 面（升级入口、规则面板、倒计时诚实化）由 B9-Web 轨另行落地后并入。

### 7.1 会话级批准跨 agent 共享（D-07，SDR-003）

- `SessionApprovals`（`rust/crates/mirri-core/src/permission/mod.rs`）：`Arc`-共享的会话级批准存储（`Mutex<HashSet<String>>`）。`SessionScope` 构造时创建一份，`create_agent` 注入每个 agent 的 `PermissionManager`（`with_shared_approvals`）——主代理、子代理、蜂群成员共享同一份「approve for session」记忆。
- resume 恢复路径无需特判：wire log 回放把 `permission.record_approval_result` 恢复进主 agent 的 manager，即写进共享存储——resume 之后创建的 agent 同样可见。

### 7.2 用户级规则接线与 CRUD（D-34 / P-013 引擎侧）

- **修复事实**：`ServerConfig.permission.rules` 在 B9 之前解析了但从未注入任何 live agent（`AgentScope::new` 构造 PermissionManager 时传空集）——手写 TOML 的用户级规则完全无效。
- **现行链路**：用户级规则经 `SessionScopeConfig.user_permission_rules`（`session_scope_config_from` 从共享 config 快照种子）在会话构造时就位——每个 agent 创建时被盖上当前规则集（含子代理——deny 规则不可经派生子代理绕过；**会话 fork 同样继承源会话的当前规则集**，deny 不会经 fork 静默消失）；CRUD 写与 config.toml 热重载经 `SessionManager::apply_user_permission_rules`（embedded 侧 `EmbeddedServer::apply_user_permission_rules` 跨全部已构建 workspace runtime 扇出）推送到所有 live agent，无需重启；`config.set` 补丁改动 `[permission]` 段时同样推送。
- **新 RPC 三方法**（approval 域，registry `both_kept`，RustFirst，serve 经 engine fall-through）：
  - `approval.listRules` —— 读共享 config 快照，`{rules: [{decision, scope, pattern, reason?}]}`（reason 缺省省略，zod `.optional()` 不受 null）。读面如实返回 `[permission]` 段的全部规则行（每行自带 scope 字段）——手写的 project-scope 行也可见。
  - `approval.addRule` —— `{rule: {decision, pattern, reason?, scope?}}`（scope 仅 `user`/缺省）；写管线 = `config.set` 同款（`load_without_env` → mutate → `save_to` 原子写 → env 重载换快照）；同 pattern + 同 decision 幂等 no-op，同 pattern + 异 decision → invalid_params（conflicting rule），不可解析 pattern → invalid_params。冲突判定与写入仅作用于 **user scope** 行（裁决 4.d：project/turn-override 行不受本面管理——与 project 行同 pattern 的 user 规则可共存，求值仍由链序的 deny-before-allow 支配）。
  - `approval.removeRule` —— 按 `{pattern, decision}` 定位（同样仅 user scope 行）；未命中 → not found；命中走同款写管线。写后推送与返回值取自 **env 展开**后的重载快照（与 session create/resume 的种子同形）。
- **裁决 4.b（wire 面，记录义务）**：用户级规则**不进会话 wire log**——用户级规则是跨会话的宿主配置而非会话状态（持久化事实源是 mirri home `config.toml`；会话内生效走内存 `set_rules` 推送）。`WireOp::PermissionRulesAdd` 因此保持 dead（不复活）。这与 TS `permission.rules.add` 的 transient 语义同构（宿主恢复时重供）。
- 附带修复：`session.get` 快照的 `permission.rules` 回填真实 effective rules（原硬编码 `[]`；空→非空是向后兼容的加宽）。

### 7.3 审批刻面下发（`approvalRule`）

- 引擎 `approval.ask` 反向 RPC 载荷新增 `approval_rule`（snake_case，`turn_flow/hooks.rs` 的 approval_request 构造处取自 `AuthorizeToolContext.approval_rule`）。
- `ApprovalSnapshot`（`session/gates.rs`）增 `approval_rule: Option<String>`；`approval.listPending` 行与 `interaction.listPending` payload 以 camelCase `approvalRule` 条件输出（无刻面则省略字段）。该字段是 ask 载荷与 listPending 行的新增字段，不是新的 `event.*` 事件——不触发事件三端投影同步义务（web 投影器由 B9-Web 轨消费）。
- 刻面粒度诚实：`Write` 等工具刻面今天仍是工具级（无路径细化）——「始终允许」确认框须如实展示 pattern（路径细化刻面在 ROADMAP §3 挂账）。

### 7.4 过期诚实化（D-13）

- Rust 引擎不派生任何 `expires_at`（B9 核验：Rust 全仓无 expires 派生，唯一命中是无关测试名）；审批过期渲染只在收到真实 `expires_at` 时成立——web wire 层三处造假点（§5 缺口 3）由 B9-Web 轨清除。


### 7.5 B9 Web 轨落地增量（2026-09-13，commit `9528fbb1`）

- **D-13 web 落地**：三处造假清除——`wire.ts:297`（`expires_at?` 可选）、`api/types.ts:352-363`（`expiresAt?`/`approvalRule?`）、`ws-rpc-event-bridge.ts:121-131`（透传、缺省省略）、`mappers.ts:369-370`；倒计时 chip 仅 `expiryLeft !== null` 时渲染（`ApprovalCard.vue:301`，`data-testid="approval-countdown"` 供 DOM 检查）；纯逻辑 `lib/approvalExpiry.ts`。
- **approvalRule 投影**：`messagesToTurns.ts` / `useMirriWebClient.ts` 两份 `buildApprovalBlock` 均带 `...expiry, ...facet`（`types.ts:914-922` `ApprovalBlockCommon`）；snapshot 路径双拼写水合（`ws-rpc-api.ts:3410-3431`）。
- **升级入口（P-012 web）**：`ApprovalCard.vue:461`「始终允许」按钮（仅真有 `approvalRule` 时渲染）+ `lib/alwaysAllowUpgrade.ts:20`（先决后写组合，非 approved 决不写规则）+ `useWorkspaceState.ts:2095-2149` 接线与专用失败标题；`alwaysAllowRule` 不外泄 wire（`approvalResponseToEngineWire` 白名单三重防线）。
- **规则面板（P-013 web）**：`components/settings/PermissionRulesPanel.vue`（CronPanel 自取数先例，会话无关的用户级面）+ SettingsView `'rules'` tab + `lib/ruleFacet.ts`（DSL 预览镜像 Rust `parse_pattern` 的 `rfind(')')` 语义，尾随垃圾接受并丢弃）+ `lib/permissionRules.ts`。
- **v3-only**：三 RPC 的 REST 适配器诚实降级（`client.ts:895-907`）。
- **⚠️ Rust 侧遗留（开放项）**：`rust/crates/mirri-server/src/rpc.rs:1574-1593` 的 `session.snapshot` `pending_approvals` 行**既无 `expires_at` 也无 `approval_rule`**——web 已按可选字段容错，引擎补发即生效（刷新水合的卡片恢复「始终允许」入口需此字段）。B12 前由 Rust 侧补齐或明确挂账。**后记（2026-09-13，B12）：已解决——mirri-server snapshot 行补发 klient camelCase 拼写的 `approvalRule`（仅当快照带刻面）与诚实的 `expiresAt`（引擎无过期派生时显式 `null`，绝不伪造），与 `approval.listPending`/interaction 内核行同形。**

## 8. v3 模式传递缺口快照（2026-09-16，Rust 引擎 + web 轨）

> 触发事件：v3 会话在用户已选定「完全自主（auto）」的情况下，主代理与子代理每步工具调用仍逐项弹审批。以会话 `session-7c9203d6-1844-4f8c-99e5-8a9dd6823f6a`（`~/.mirri-code/v3_sessions/...` wire log 全程无 `PermissionSetMode` Op，却有 7 次 `permission.record_approval_result`——Bash/Write/AgentSwarm 逐项审批）为证据复盘。三处缺口均相对 SDR-004/ADR-004 终态；修复项挂本域 ROADMAP §3。

### 8.1 web v3：会话建立前的模式选择未落地

- `setPermission`（`apps/mirri-web/src/composables/client/useWorkspaceState.ts:2493-2497`）依赖 `persistSessionProfile`，而后者在无活跃会话时直接 no-op（`useMirriWebClient.ts:780-781`）——会话建立前的选择只留在浏览器本地。
- 首条消息路径 `startSessionAndSendPrompt` → `createDraftSession`（`useWorkspaceState.ts:1260-1333`）把 draft 的 thinking/plan/swarm/collab/goal 携带进新会话，唯独不携带 permission（无 `draftModes.permission` 概念——permission 被设计为随每条 prompt 走）。
- 但 v3 通道的 `sendPrompt` 有意丢弃 per-prompt `permissionMode`（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:685-706`，D-26：模式状态只经专用 RPC）——首条消息携带的模式被丢弃，服务端全程不知道该选择。
- 对照：skill 启动路径 `startSessionAndActivateSkill` 显式持久化 `permissionMode: rawState.permission`（`useWorkspaceState.ts:1407-1416`，注释自述原因）——同一意图两条路径不对称，普通 prompt 路径缺失。

### 8.2 rust v3：模式变更不广播已存活子代理

- `agent.setPermission` handler（`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:2604-2645`）：`dispatch_set_permission` 只写目标 agent（web 只传 session_id → main agent）并记 journal，无会话内广播。
- 子代理仅在派生时继承父代理当时的实时模式（`rust/crates/mirri-core/src/subagent/host.rs:726-734`，D-47）；中途切换不更新已存活子代理——与 US1.2「广播覆盖全部存活代理」不符（v2 TS 的 `broadcastPermissionMode` 反而覆盖，见 §1.2）。
- `PermissionSetMode` reducer（`rust/crates/mirri-proto/src/reducer.rs:386-393`）只恢复单 agent 模式，resume 路径同样无广播。

### 8.3 rust v3：default_permission_mode 无消费点

- config 键的解析与 wire 序列化存在（`rust/crates/mirri-core/src/config/mod.rs:93`、`config/wire.rs:110`、`env_layer.rs:103`），但全 crates 生产代码无消费——新 agent 的模式来自 `PermissionMode::default()` = `Manual`（`permission/mod.rs:40-43`；`scope/config.rs:108`、`workspace_scope.rs:339`、`profile.rs:1293` 的 `unwrap_or_default()` 链）。v2 TS 侧有对应实现（§1.2 configSection + `agentLifecycleService.ts:201-204`）。
- 用户全局 `default_permission_mode = "yolo"` 在 v3 上因此不生效（对 §8.1 的会话同时是叠加因素：即使模式没丢，默认值也不会兜底）。

## 9. 问询关闭现状快照（2026-10-13，TS v2 + Rust v3）

> 触发：SDR-005/ADR-007 裁定关闭终态为「终止回合、归还输入权」。本节记录裁决时的实现现状（差距事实），供实现侧对照。

- TS v2 关闭路径：`packages/agent-core/src/tools/builtin/collaboration/ask-user.ts`——`requestQuestion` 返回空 answers 或异常即 `dismissedQuestionResult()`：固定文案 `User dismissed the question without answering.`，无行为指令、无原因携带、不终止回合（模型自行裁量）；遥测仅 `question_dismissed` 一个事件（无延迟/后续行为维度）。
- TUI 关闭交互：`apps/mirri-code/src/tui/components/dialogs/question-dialog.ts`——Esc / Ctrl+C / Ctrl+D / Submit 页 Cancel 四路全部走 `onAnswer({ answers: [] })` 同一出口，无原因输入；按 ADR-007 裁决后的差异（Ctrl+C 归中断路径）在 TUI 侧是否调整属实现细节，TUI 面将随 Rust 核心切换整体重估（对齐 ROADMAP §3 开放项 6 的裁决口径）。
- ACP 降级路径：`packages/acp-adapter`（问询桥接）RPC 失败或未知选项 → null 无作答——语义上属「无应答」而非「用户主动关闭」，ADR-007 已裁定两类均终止回合。
- 无头缺省：`installHeadlessHandlers` 问询一律 null——同上，无人应答即无共识。
- Rust v3：~~`rust/crates/mirri-core`（session/gates.rs 的 `SessionQuestionService` 同构面）dismiss=null 分支与 TS 同形，无终止语义~~（**后记 2026-10-13：Rust 引擎轨已落地 ADR-007**——`rust/crates/mirri-core/src/tools/ask_user.rs` 阻塞路径关闭返回 `dismissed_result()`（`stop_turn: true`、`is_error: false`、`{answers:{}, note, instruction}` 三要素 JSON，引擎侧经 `loop_engine/turn_step.rs` 既有 `stop_turn` 消费强制 `EndTurn`）；后台路径关闭 → 任务取消定局（`TaskStatus::Stopped`）+ 携带同一指令的取消通知（`task_notification.rs` 共享常量，不终止回合）；防重问护栏在 `resolve_execution` 入参校验层（`session/gates.rs` dismissed-text ring：会话级、容量 256、逐字匹配、`answer()` 不入环；无处理器宿主不记录、护栏不触发）。无头/SDK 空回调宿主的「无作答」立即返回不注册 gate，仍走各自的降级语义。）
- web/desktop：`packages/kap-server/src/routes/questions.ts`（REST resolve）dismiss 路径回 null，消费端呈现「无作答」——无原因输入、无回合终止触发。

## 10. 问询投递连接韧性现状快照（2026-10-24，Rust v3，session-028fcdba 故障复盘）

- `AskUserQuestionTool`（`rust/crates/mirri-core/src/tools/ask_user.rs`）在 `resolve_execution` 时一次性 `resolve_callback()` 把回调快照烤进执行闭包（阻塞路径 :271、后台路径 :322）。槽位机制本意是「晚订阅可达」，但**连接断开后回调被 teardown 吊销、槽位不重装**（客户端不重连重订阅就永远是旧的），快照失效即永久失效。
- WS 传输（`rust/crates/mirri-server/src/ws_rpc.rs`）连接 teardown 将共享 `ReverseRpcState.revoked = true` 并清空 pending（:2051-2064）——刻意的 fail-fast，后续 `question.ask` 经 `WsReverseRpcCallback::call` 立即返回 `Err("reverse RPC connection closed")`。
- 实测故障（wire.jsonl 时间戳）：同回合前 8 次 AskUserQuestion 成功（15:33 前）；连接断开后两次重试均在 **7ms 内**立即失败（20:42:09 / 20:42:14，`question.ask failed: reverse RPC connection closed`）——非超时，是 revoked 快速失败。模型自行降级纯文本提问，用户无感知。
- 审批路径（`rust/crates/mirri-core/src/turn_flow/hooks.rs:1393` 起）投递失败时**保持 gate 待决**（不收尾为错误），客户端重连后经 `approval.decide` / interaction 读面可解决——已具备「投递失败挂起」语义，但无主动重发与等待上限。
- 问询 gate 双通道收敛已存在（`question.answer` / gate receiver 先到先得）；WS 订阅路径对 `approval_gate.list_pending()` 有重推（ws_rpc.rs:1170），**question gate 无对称重推**（问询重连后只能靠 gate RPC 通道作答，无卡片重推）。
