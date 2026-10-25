# 权限与审批（approval）— 架构与实现机制

> 本文档描述 TS 引擎（以 v2 为主）实现权限与审批的机制、数据流与接口形状。现状代码证据（file:line）见 [approval-context.md](approval-context.md)。动机仅以抽象问题陈述保留，不携带代码事实。

## 1. 总体结构

审批闭环由四层组成，自上而下：

1. **裁决层（Agent 作用域）**：`permissionGate` 监听每次工具执行前的否决事件，运行静态有序的 `permissionPolicy` 策略链，得出 `approve / deny / ask`。
2. **往返层（Agent 作用域）**：`toolApproval` 拥有审批往返——构建请求、发布事件、等待会话级审批中介、记录会话级规则、上报遥测、把拒绝格式化为模型可读的工具结果。
3. **中介层（Session 作用域）**：`interaction` 是与领域无关的「阻塞等待人类输入」内核，持有待决集合；`approval` / `question` 是其上的类型化门面。审批也可以完全不存在中介（无界面宿主）。
4. **呈现层（宿主侧）**：TUI 面板、web 卡片（经 REST/WS）、ACP 权限请求、SDK 处理器、无头自动批准——每类宿主一个适配器。

数据流向：

```
模型发起工具调用
  → toolExecutor 执行前钩子（两遍裁决事件）
    → permissionGate（策略链裁决）
      ├─ approve → 放行（可携带执行元数据）
      ├─ deny/result → 直接否决（拒绝消息回传模型）
      └─ ask → 延迟到「无其他监听者否决」后启动审批往返（toolApproval）
            → session approval 门面 → interaction 内核（park + journal）
            → 宿主呈现（TUI/web/ACP/SDK/headless）
            → 用户决策 respond → 决议折叠回裁决（放行 / 否决+消息 / 延续裁决）
  → 会话级批准 → permissionRules 记录 pattern → 后续同刻面调用被 session-approval-history 策略放行
```

### 设计动机（抽象）

- **两遍裁决**：审批往返是唯一有副作用的裁决（弹 UI、等人），必须保证「另一个监听者本会否决这个调用」时根本不发起审批——否则用户会在一个注定被丢弃的请求上浪费时间。第一遍同步裁决（立即否决/放行），第二遍才逐个调用延迟工厂。
- **内核与门面分离**：审批与问询（以及用户工具交互）共享同一套「挂起-应答-最近已解决」语义；把领域无关的等待语义做成内核，审批/问询只做类型化包装，避免三处各写一份排队与幂等。
- **静态有序策略链**：裁决优先序是产品语义（旁路全禁 > 用户 deny > 模式放行 > 会话记忆 > 用户 ask/allow > 敏感路径 > 兜底问询），以固定顺序的一组小策略表达，每个策略只回答「我是否对此调用表态」。

## 2. 裁决层

### 2.1 permissionGate

- 接口：`data()` 返回当前 `{ mode, rules }`；`authorize(context)` 供非事件路径主动裁决。
- 机制：向 `toolExecutor.onBeforeExecuteTool` 注册监听；对每个调用运行策略链：
  - `ask` → `event.waitUntil(() => requestToolApproval(...))`（注册冷工厂，不立即弹）；
  - `approve` → `event.pass(executionMetadata)`；
  - `deny` / 内联 `result` → 经 `toolApproval.resolvePermissionResolution` 转成否决。
- 每次裁决上报 `permission_policy_decision` 遥测（回合、调用、策略名、工具、模式、决定、原因明细）。

### 2.2 执行前事件的两遍语义（toolExecutor 侧）

- 事件对象提供四种陈述：`veto(result)`（首个否决立即生效）、`allow()`（立即终局放行）、`pass(metadata)`（放行并附元数据）、`waitUntil(factory)`（注册延迟裁决工厂）。
- 发射器两遍执行：第一遍逐监听者同步裁决（veto/allow 先到先得）；仅当第一遍无结论时，第二遍逐个调用 waitUntil 工厂——第一个返回 veto 的决定调用，返回元数据的并入放行痕迹。
- 四种陈述在窗口关闭后调用即抛错（防迟到的否决被静默忽略）。`rewriteArgs` 允许监听者改写参数（与本域无直接关系，属 tool-calling）。

### 2.3 permissionPolicy 策略链（静态有序）

链序即裁决优先序（详表与代码位置见 context）：

| # | 策略 | 表态条件 |
|---|---|---|
| 1 | `deny-all` | opt-in（btw 子代理启用后全禁，支持自定义消息） |
| 2 | `auto-mode-ask-user-question-deny` | auto 模式下的 `AskUserQuestion` → deny + 自行决策指引 |
| 3 | `user-configured-deny` | 用户 deny 规则命中（含 reason 的拒绝消息） |
| 4 | `auto-mode-approve` | auto 模式 → 全放行 |
| 5 | `session-approval-history` | 会话级批准记忆命中 → approve（遥测带匹配策略与是否带参数） |
| 6 | `user-configured-ask` | 用户 ask 规则命中 → ask |
| 7 | `user-configured-allow` | 用户 allow 规则命中 → approve |
| 8 | `sensitive-file-access-ask` | 调用的文件访问涉及敏感文件 → ask |
| 9 | `git-control-path-access-ask` | 路径含 `.git` 组件、或位于 git 工作树控制目录 → ask |
| 10 | `yolo-mode-approve` | yolo 模式 → 放行（注意位置：在用户 ask 与敏感/git 之后，故这些面在 yolo 下仍会问） |
| 11 | `default-tool-approve` | 只读/编排类内建工具名集合 → approve |
| 12 | `git-cwd-write-approve` | posix 且 Write/Edit 全部写路径都在工作区内且 cwd 是 git 工作树 → approve |
| 13 | `fallback-ask` | 兜底 ask |

模式语义要点（由链序导出，详见 requirements US1.1）：

- **auto**：位置 4 全放行，故用户 ask 规则与敏感/git 面在 auto 下不生效；只有 deny 面（1/2/3）先于它。
- **yolo**：位置 10 才放行，故用户 ask、敏感文件、git 控制路径在 yolo 下仍问询。
- **manual**：无模式放行，走默认放行（11/12）与兜底 ask（13）。

### 2.4 permissionMode

- **单一事实源（ADR-004）**：会话实时模式是权限裁决的唯一运行时决策源——裁决链只读取会话持有的实时模式；界面上显示的模式是该状态的镜像（乐观更新、服务端对账，落地失败显式暴露）。会话级真值在 wire 折叠态（`permission.set_mode` Op 还原的会话状态）；各代理侧（主/子代理）的 `PermissionManager` 持有的只是该实时模式的**投影**——靠 `setPermission` 广播、子代理派生继承与 resume 重播维持一致，运行时不存在按字面可寻址的「单点字段」。
- `PermissionMode = 'manual' | 'auto' | 'yolo'`，存为 wire 标量 Model（初始 `manual`），经 `permission.set_mode` Op 变更；`PermissionModeConfiguredModel` 标记「显式设置过」以区分默认值。
- `setMode` 幂等（同值且已显式配置则零动作），实际变化后发 `onDidChangeMode` 事件。
- RPC 面 `setPermission`：更新会话实时模式并向会话内全部存活代理广播（主代理、已存在子代理、蜂群成员）；子代理派生时以会话实时模式为初值（防御派生早于广播的时序窗口），派生后仍受后续广播覆盖。切换上报 `yolo_toggle` / `afk_toggle` 遥测。
- 会话建立路径保证「用户已选定的模式先于首回合首个工具裁决落地」：先选模式后建会话的时序下，建立路径把所选模式作为会话初始模式写入（与 CLI 启动参数同构，ADR-004）。
- `defaultPermissionMode` 配置节（config）：新会话的初始姿态种子与界面初始显示（ADR-004——不参与进行中会话的裁决，不做热覆盖）。
- `permissionModeInjection`：`permission_mode` 上下文注入变体——进入/离开 auto 时注入对应提醒；「上次模式」入 agent 状态；位置感知（被压缩折叠后下次注入重申）。

### 2.5 permissionRules

- 规则模型：`PermissionRule { decision: allow|deny|ask, scope: turn-override|session-runtime|project|user, pattern, reason? }`。
- wire Model `PermissionRulesModel` 持有 `rules` 与去重的 `sessionApprovalRulePatterns` 两个状态：
  - `permission.rules.add`（transient，不持久）：宿主在恢复/运行时注入规则（v1 不持久规则，宿主重进时重给）。
  - `permission.record_approval_result`（persisted）：审批结果入日志；仅「approved + scope=session + 带刻面」时把 pattern 加入去重集合。
- `matchesRule`：pattern 语法 `ToolName` / `ToolName(argPattern)` / `*`——工具名经 picomatch 通配匹配；带 argPattern 时交 `execution.matchesRule(argPattern)`（由工具的审批刻面实现，刻面生成属 tool-calling 域）判定；解析失败（空串/缺括号/空工具名）视为不匹配（配置层另有校验拒绝）。
- config 节 `[permission]`：TOML 的 `deny` / `allow` / `ask` 列表与 `tool`+`match` 简写归一为内部 `rules` 数组；pattern 合法性以解析器校验，非法规则使配置加载失败（不静默忽略）。
- 用户配置面策略只消费 `turn-override` / `project` / `user` 三个 scope；`session-runtime` 由会话记忆策略独占。

### 2.6 特殊裁决面（非策略链内）

- **计划审查**：`ExitPlanModeReview` 拦截携带非空 `plan_review` 展示的 ExitPlanMode 调用，直接走 `toolApproval.requestToolApproval`（origin `exit-plan-mode-review-ask`，**manual/yolo 下弹出；auto 模式下跳过审查**——计划提交在 auto 下直接放行，与目标启动确认的 auto 跳过一致），并把每种审批结局折叠为合成工具结果（批准→退出计划+计划全文+所选变体指令；Revise→保持+反馈；Reject and Exit→退出+终止回合；dismiss→保持）。注意拒绝侧的跨端差异：ACP 与 web 提供显式「Reject and Exit」，TUI 只提供「Reject / Revise」，TUI 侧拒绝走默认分支（保持计划模式+终止回合）。
- **目标启动**：goalService 监听 `CreateGoal` 调用，manual/yolo 且带 `goal_start` 展示时经 `waitUntil` 走审批（auto 跳过）；批准时可按 `selectedLabel` 切换权限模式。
- **蜂群排他**：swarmService 监听执行前事件，同批多个 `AgentSwarm` 或与其余工具混合时直接 veto（经 `formatDenyMessage` 格式化）。
- **旁路问询（btw）**：btw 子代理启用 `deny-all` 策略（自定义消息），一切工具调用拒绝。

## 3. 往返层（toolApproval）

- 接口：`resolvePermissionResolution(result, context, origin)`（把裁决折叠为放行/否决/再往返）；`requestToolApproval(context, askResult, origin)`（发起审批往返）；`formatDenyMessage` / `formatApprovalRejectionMessage`（拒绝文案）。
- `requestToolApproval` 流程：
  1. 组装 `ApprovalRequest { sessionId, agentId, turnId, toolCallId, toolName, action, display }`（display 缺省回退为 generic 形态，action 回退为 `Approve <tool>`）。
  2. 尝试解析会话审批服务（Session 作用域，无宿主界面时可能不存在）——**不存在则直接 auto-approve**（无界面嵌入的缺省）。
  3. 存在则发布 `permission.approval.requested` 事件，`abortable` 等待 `approvalService.request(...)`（回合中止信号联动）；取消类错误原样上抛，其余错误按 error 结局处理（发 resolved 事件、上报遥测、ask 结果的 `resolveError` 延续可自救）。
  4. 决议后发布 `permission.approval.resolved` 事件（含决策或 error）；`rulesService.recordApprovalResult`（approved+session+刻面 → 会话记忆）；上报 `permission_approval_result` 遥测。
  5. ask 结果的 `resolveApproval` 延续（策略/特殊流可把决策折叠为终局裁决，如计划审查的合成结果）；无延续时：approved → 放行（undefined），rejected/cancelled → 否决。
- 拒绝文案：`rejected` 与 `cancelled` 分别有明确措辞（可区分），可附用户反馈；**非主代理（worker）的拒绝/取消消息附加「换方法、别重试、别绕过」指引**。
- 事件契约：`permission.approval.requested / resolved`（resolved 的载荷是 request 上下文 + `ApprovalResponse` 或 `{ decision: 'error', error }`），进 DomainEventMap 供外部钩子与 UI 订阅。
- `ApprovalResponse { decision: approved|rejected|cancelled, scope?: 'session', feedback?, selectedLabel? }`——`selectedLabel` 承载「用户在多个放行变体中选了哪一个」（计划变体、目标启动档位）。

## 4. 中介层（Session 作用域）

### 4.1 interaction 内核

- `ISessionInteractionService`：`request`（阻塞等待应答）/ `enqueue`（不阻塞，经 `onDidResolve` 观察）/ `respond(id, response)` / `listPending(kind?)` / `isRecentlyResolved(id)` / `cancelPendingForTurn(turnId)` / `onDidChangePending` / `onDidResolve`。
- 待决集合存于 session 状态（`interaction.pending` / `interaction.recentlyResolved` / `interaction.nextId`），随会话生灭；id 缺省自动生成 `interaction-N`。
- `cancelPendingForTurn`：回合结束时把该回合挂起的交互以 `{ cancelled: true, reason: 'turn_ended' }` 收尾。
- 最近已解决表：60 秒 TTL、上限 256 条——供 REST 幂等判定（重复 resolve 短窗内可区分「已解决」与「从未存在」）。
- journal：每次请求/应答以 `interaction.request` / `interaction.resolved` Op 写到**发起代理**的 wire（`origin.agentId ?? 'main'`），冷启动转录折叠可重建交互实体；**挂起的 Promise 不从 journal 恢复**——无应答的请求在折叠语义中按取消处理（进程死亡恢复终态）。

### 4.2 approval / question 门面

- `SessionApprovalService`：`request` → interaction `kind: 'approval'`；`enqueue` / `decide(id, response)` / `listPending()`。请求 id 缺省取 `toolCallId`（稳定关联键），再退化为 `toolName:时间戳`。
- `SessionQuestionService`（问询，本域覆盖用户闭环）：`request`（可带中止信号与 agentId）/ `enqueue` / `answer` / `dismiss` / `listPending`；`QuestionResult = null（dismiss）| 扁平答案 Record | { answers, method }`。

## 5. 呈现层（各宿主接入）

### 5.1 嵌入式 SDK（node-sdk）

- 会话级 `setApprovalHandler(handler?)` / `setQuestionHandler(handler?)`：按 sessionId 注册进处理器表；清除即注销。
- 引擎审批 → `requestApproval(request)`：无处理器 → `{ decision: 'cancelled', feedback: 'No approval handler registered.' }`（不默认放行）；处理器抛异常 → cancelled + 错误事件（`SESSION_APPROVAL_HANDLER_ERROR`）+ 「Approval handler failed.」反馈。
- `setPermission(mode)` RPC 透传模式设置。

### 5.2 无头模式（CLI run-prompt）

- 无 TUI 时安装固定处理器：审批一律 `approved`；问询一律 `null`。

### 5.3 ACP 适配器

- 规范三选项（`approve_once` / `approve_always` / `reject`，optionId 不透明、name 回传为 `selectedLabel`）；计划审查特化（`plan_opt_<i>` / `plan_approve` / `plan_revise` / `plan_reject_and_exit` 命名空间；ACP 拒绝侧排序为 Revise → Reject and Exit，与 TUI 的 Reject → Revise 不同——两端拒绝选项集也不同，见 §2.6 跨端差异）。
- 问询桥接：ACP 无专用问询方法，问询复用权限请求面（`q{n}_opt_{i}` / `q{n}_skip` 选项命名空间）；多问题批次降级为只问第一题（遥测 `question_degraded` 记录丢弃数）、多选降级为单选；RPC 失败或未知选项一律返回无作答（null，dismiss 语义），不编造答案。
- 响应映射：`cancelled` outcome → cancelled；approve_once → approved；approve_always → approved+session；reject → rejected；**未知 optionId 一律 rejected（安全缺省）**；兼容旧客户端的 `approve` / `approve_for_session` 旧标识。
- 审批卡片：以 `${turnId}:${rawId}` 前缀 id 关联已渲染的工具卡，diff/file-io 预览与 plan_review 内容置顶，恒附 `Requesting approval to <action>` 摘要。
- 模式档位：ACP 四模式（default/plan/auto/yolo）映射为 `{ plan: bool, permission: manual|auto|yolo }` 两开关；模式列表与默认档在 session/new、session/load 响应中广播。

### 5.4 Web（经 kap-server）

- REST：`GET /sessions/{sid}/approvals?status=pending` 列待决（补 `createdAt` 元数据）；`POST /sessions/{sid}/approvals/{aid}` 提交决策。错误映射：会话不存在 / 审批不存在 / 已解决（60 秒幂等窗，`resolved:false` 信封）/ 校验失败。
- wire 保真缺口（有意为之）：引擎审批不过期，wire schema 要求 `expires_at` → 派生 `created_at + 24h`；`tool_call_id` / `session_id` 缺失时以 interaction id / 路径 id 兜底。
- WS 事件 `awaiting_approval`（会话状态含待决审批）；前端把 `approvalRequested` 事件落进每会话待决表，渲染审批卡片（含计划变体选择、Revise 反馈框、数字快捷键），会话状态机中「等待审批」优先级最高；断线重连经快照/REST 重建待决表。问询同理（questions 路由 + 问询卡片）。
- TUI 的 ACP 之外的本地路径与 web 的差异：TUI 面板内嵌于终端 UI（模态协调器串行 approval/question）；web 卡片为页面内组件、可多会话并存。

### 5.5 TUI（mirri-code）

- `ReverseRpcController`：`show(payload)` 返回 Promise；一次一个、按到达顺序排队；`respond` 解当前并推进；`cancelAll(reason)`（会话切换/重载）把当前+排队全部以 cancel 响应收尾；`autoResolveFor` 钩子实现「session 级批准继承给排队中的同 action 请求」（丢弃 feedback/selectedLabel）。
- `ReverseRpcModalCoordinator`：approval / question 两类模态的互斥与串行（当前接线下两类各自 FIFO 串行、异类互斥先后呈现）。
- 审批适配器：SDK `ApprovalRequest` → 面板数据（标题映射、generic 参数推导预览块、8 种高危命令模式标注、`ToolInputDisplay` 13 形态 → 展示块、选项特化：默认四项 / 计划审查 / goal_start 档位）；面板响应 → SDK `ApprovalResponse`（`approved_for_session` → approved+session）。
- 审批面板组件：块渲染（diff 聚类 3 行上下文、内容高亮 10 行截断、danger 警示）、键盘（↑/↓/数字/回车/Esc/Ctrl+C/Ctrl+D 拒绝/Ctrl+E 预览/Ctrl+O 工具输出）、行内反馈输入、最多 5 块 + brief 去重、提示行。
- 会话接线：切换/重载会话时注销旧 handler、`cancelAll` 并为新会话重装 handler。
- 模式命令：`/auto`、`/yolo`（on/off/toggle，含状态提示文案）、`/permission` 选择器（设置入口可达）；`--auto`/`--yolo` 启动参数；goal/swarm 命令的启动确认对话框（选择后经 `setPermission` 切档，goal 创建失败回滚原档）。

## 6. 外部钩子与遥测

- `PermissionRequest` / `PermissionResult` 钩子事件：fire-and-forget 触发（不阻断审批主流程），载荷为脱壳后的请求/结果上下文，匹配器按工具名过滤。
- 遥测面：`permission_policy_decision`（逐裁决）、`permission_approval_result`（逐往返，含 duration_ms、surface、session_cache_written、has_feedback）、`yolo_toggle` / `afk_toggle`（模式切换）、`plan_submitted` / `plan_resolved`（计划审查）。

## 7. v1 侧（legacy，仅存差异摘要）

- v1 `PermissionManager`：策略链可动态 unshift（`DenyAllPermissionPolicy`）；规则经父代理链共享（子代理继承父的 rules/mode——`parent` 链 + `modeOverride`）；规则不持久（恢复时宿主重给）。
- v1 协议审批中介（daemon 侧）：`IApprovalService` / `IQuestionService` 一次性 broker（request + resolve/dismiss + listPending），经 `event.approval.requested` WS 广播 + REST resolve；协议形态带 daemon 铸造的 ULID `approval_id` 与 **60 秒 `expires_at`**；问询协议面合成 `q_<i>` / `opt_<i>_<j>` 稳定 id、5 态答案联合（single/multi/other/multi_with_other/skipped），回传时把 id 翻译回问题/选项文本（未知 id 原样保留可诊断）、`click` 应答方式在进程内被丢弃。
- v2 的 wire 迁移 `1.1 → 1.2`：旧版会话批准记录无 `sessionApprovalRule` 时按动作描述推导 pattern（`run command → Bash` 等），不可恢复动作（计划模式内命令、后台命令）不推导——宁可恢复后多问。
- v1 与 v2 的裁决面基本同构；v2 差异：策略链静态化、规则/模式入 wire Model、interaction 内核统一审批与问询、审批无过期。

## 8. 架构决策记录

### ADR-001（2026-09-12）审批往返采用「两遍裁决 + 冷工厂」而非同步等待

- **背景与动机**：审批是唯一有副作用的裁决面；若在同步裁决阶段直接弹审批，其他监听者（如计划守卫、外部 PreToolUse 钩子）可能本会否决该调用，用户就在一个注定被丢弃的请求上被打断。
- **决策**：第一遍同步裁决（veto/allow 先到先得），第二遍才逐个调用 `waitUntil` 注册的冷工厂发起审批往返。
- **被否决的备选**：同步阶段直接弹审批（用户被无谓打断）；全部裁决都延迟（放行/拒绝也要两遍，无谓增加往返延迟）。
- **影响范围**：§2.1、§2.2。

### ADR-002（2026-09-12）待决交互「内存态、journal 只做重建」的恢复语义

- **背景与动机**：进程崩溃时可能存在挂起未决的审批/问询；恢复时若重建等待 Promise，会与「用户早已不在那个上下文」的现实冲突，且无人应答会永久悬挂。
- **决策**：挂起语义只存在于内存（随会话生灭）；journal 记录请求与应答仅用于冷折叠重建交互实体；折叠中「有请求无应答」一律按取消终态处理。
- **被否决的备选**：恢复时重建未决审批并重新弹出（用户面对过期的上下文，且新进程的 UI 可能未连接）。
- **影响范围**：§4.1、requirements US10。

### ADR-003（2026-09-12）模式语义的链序设计（yolo 仍保留高危问询，auto 才全放行）

- **背景与动机**：两种「自动」姿态的安全预期不同：yolo 面向「可信批量工作但别动我的敏感文件」；auto 面向「无人值守全放行，连问询都不要」。
- **决策**：`auto-mode-approve` 置于用户 ask 规则与敏感/git 面之前（auto 下这些面不再打断）；`yolo-mode-approve` 置于其后（yolo 下用户 ask、敏感文件、git 控制路径仍问询）；`AskUserQuestion` 仅在 auto 被拒并附「自行决策」指引。
- **被否决的备选**：两档同等全放行（丢失 yolo 的高危防线，与 UI 文案「AI decides which actions need your approval」不符）。
- **影响范围**：§2.3、requirements US1.1。

### ADR-004（2026-09-16）权限模式的单一事实源与会话内全量广播

- **背景与动机**：模式状态存在多个潜在写入面（配置默认值、CLI 启动参数、TUI/web/ACP/SDK 的切换入口、目标/蜂群启动确认的档位选择）。若无唯一的权威持有者与明确的传播规则，不同环节可对同一会话持有不同姿态，产生「界面显示 A、裁决按 B」的认知错位（requirements SDR-004）。
- **决策**：
  1. **唯一事实源**：会话实时模式由会话作用域持有（每会话一份），裁决链只读取它；界面是乐观镜像 + 服务端对账。
  2. **专用变更通道**：模式变更不随消息携带，只经专用设置通道；设置通道承担三件事——更新会话状态、持久化（wire journal）、向会话内全部存活代理广播。
  3. **派生初值 + 广播兜底**：子代理派生时从会话实时模式取初值（防御派生早于广播的时序窗口），派生后的代理仍受后续广播覆盖——两道机制叠加保证无窗口不一致。
  4. **先落地后裁决**：会话建立路径保证用户已选定的模式在首回合首个工具裁决前落地（先选模式后建会话的时序下由建立路径写入初始模式，与 CLI 启动参数同构）。
  5. **种子不参与裁决**：配置默认模式仅为新会话初始姿态与界面初始显示的种子，不参与进行中会话的裁决，不做热覆盖（区别于用户规则热推送——规则是声明式约束的增量集合，模式是会话姿态的单值，语义不同）。
  6. **失败显式暴露**：模式设置未能落地时必须向用户显式暴露，不得静默吞掉（乐观更新回滚并提示）。
- **被否决的备选**：①消息携带模式——与本域既有设计（模式状态只经专用通道）冲突，且引入「同回合内模式漂移」的双写竞态；②子代理派生时快照后不再跟随——存在已存活子代理停留在旧姿态的窗口，违背全量实时生效；③配置变更热推送覆盖会话实时模式——全局配置不应覆盖会话内用户的显式选择。
- **影响范围**：§2.4、§5（各宿主的模式入口）；requirements US1.2 / SDR-004；v3 现状缺口见 context §8。

### ADR-005（2026-09-16）审批展示 wire 契约采用内部标签 `kind` 协议方言

**决策**：Rust 引擎 `ToolInputDisplay` 跨线（`approval.ask` / `session.snapshot` 的 `tool_input_display`）一律序列化为内部标签方言——`{ "kind": "file_io", "path": ..., "operation": ... }`，变体名与字段名以 `rust-core-design/01-protocol-unification.md` 的 `ToolInputDisplay` schema 为准（语义蓝本即本域 13 形态）。serde 侧用 `tag = "kind"` + `rename_all = "snake_case"` + Option 字段 `skip_serializing_if` 实现。

**备选与否决理由**：(a) serde 默认外部标签（`{"FileIo": {...}}`）——任何按 kind 判别的客户端（web/TUI/ACP）都读不出形态，全部降级 generic，正是本 ADR 要修的缺陷；(b) 只发扁平 `summary` 字符串（01-protocol-unification 初稿）——无法渲染 diff/文件内容/目标文本等结构化预览，US3.1/US9.1 不满足。

**后果**：`goal_start` 需携带 objective/completion_criterion（CreateGoal 在 resolve 阶段即有）；`plan_review` 需携带 plan/path/options（ExitPlanMode 从 PlanManager 读取）；`diff` 携带 before/after。`todo_list`/`background_task` 保持无字段单元形态（面板类工具 manual 模式自动放行，不弹审批）。

### ADR-006（2026-10-07）Rust 方言：计划内容经 ExitPlanMode 参数内联提交，而非 v1 的宿主计划文件流

**决策**：rust v3 内核的计划模式中，模型通过 `ExitPlanMode` 的 `plan` 参数直接提交完整计划文本（落 `plan.revision` op 的内联 `planText`，`plan_mode.exit` 前置写入），替代 v1 的宿主托管计划文件流（宿主铸造路径 + Write/Edit 白名单例外 + ExitPlanMode 读盘）。v1 的 `plan_review` 审批展示（plan/path/options）与全部审批结局语义（§2.6）不变；`path` 字段在 rust 方言下保持缺省。

**备选与否决理由**：(a) 宿主计划文件流（TS parity）——需要计划文件路径铸造、注入、守卫路径例外与读盘四段新机制，且现有 rust 宿主（embedded server）从未实现该流（`set_plan_file_path` 生产零调用），为一个无消费者的机制扩权限面不值得；(b) 仅修守卫放行 ExitPlanMode——计划文本无来源，`ExitPlanMode` 必然死在「No plan text found」，闭环仍断。

**后果**：计划文本的唯一权威来源是 `plan.revision` op 的 `planText`（恢复路径已有）；`PlanManager::set_plan_text` 为工具写侧；计划模式指引文本与守卫允许列表（Read/Grep/Glob + AskUserQuestion + EnterPlanMode/ExitPlanMode）完全对齐。

### ADR-007（2026-10-13）问询关闭的回传形态与回合终止机制

**决策**：问询关闭（`QuestionResult = null`）的回传从「无指引的空答案」改为**带强制终止语义的结构化结果**，引擎侧保证回合结束，不依赖模型自觉。

- **回传形态**：关闭时问询工具结果携带三要素——结局（关闭，区别于逐题跳过的部分作答）、行为指令（不要基于猜测继续；结束本轮输出，等待用户主动输入）、可选的用户关闭原因（提供原因输入的接入面才携带）。后台变体（`background: true`）不终止回合：任务以取消定局，取消通知携带同样的行为指令送达发起代理。
- **终止机制**：前台问询关闭 → 工具结果回传 → 引擎在该工具调用所在的回合边界强制终止（复用回合终止既有机制，与「审批拒绝并终止」同构）；同回合其余未决问询以「回合已结束」取消收尾，未决审批不受连带。**递延注记（2026-10-13，落地批次）**：「同批并行问询的兄弟问询取消」尚未实现——同一步内并行发起多个问询且用户仅关闭其一的场景下，被关闭问询的 `stop_turn` 因批次需等待全部 future 而暂不生效；作为后续项挂账（实现方向：回合收尾时撤销该回合仍在挂起的问询 gate 条目），落地前该子句不构成对实现的约束。
- **重问护栏**：同一会话内对刚被关闭的问题原样重问，在工具入参校验层拒绝并回附行为指引（先例：`QUESTION_UNSUPPORTED_FAILURE_MESSAGE` 的「Do NOT call this tool again」模式）。

**备选与否决理由**：(a) 维持现状（空答案 + 一句说明，模型裁量）——SDR-005 已裁决否决（意图不明时不得以猜测继续）；(b) 只改回传文案不强制终止（软指引）——依赖模型自觉，行为不可验收，骚扰循环风险留存；(c) 关闭即中止整个会话——过度反应，关闭只是拒绝这次交互，不是终止协作。

**后果**：`QuestionResult` 的 `null`（dismiss）分支语义变更为携带行为指令的终态。**修订（2026-10-13，落地批次）**：终止语义的 Rust v1 范围收敛为「gate dismiss（用户关闭面板经 `question.dismiss` / gate 通道解析）」——引擎在该路径强制 `stop_turn`。无头缺省（问询一律 null）与 ACP RPC 失败降级路径产生的「无作答」**暂不终止回合**（维持各自降级语义，见 ROADMAP 开放项 10 剩余项），对齐与否作为后续裁决显式挂账，不作为本 ADR 的既成约束。重问护栏的记忆环为**会话内存态**（不随 resume 重建、容量 256 逐字匹配）——「同一会话内」按进程内会话生命周期理解，跨 resume 的原样重问不拦截（诚实边界，如需持久化再立新 ADR）。glossary AP5 同批登记。

### ADR-008（2026-10-24）问询投递的连接韧性：槽位重解析等待环（SLOT-RESOLVE-RETRY）

**背景与动机**：长回合中客户端连接可能中途断开并重连（网页刷新、心跳踢除、客户端进程重启）。现状是投递失败即终局：工具在解析期把回调快照烤进闭包，连接断开后快照永久失效，`question.ask` 立即失败回传模型，用户无感知（现状证据见 context §10；需求裁决见 SDR-006）。

**决策**：`question.ask` 的投递从「单次回调调用、失败即终局」改为**按投递尝试循环 + 通道错误时等待重连**：

- **投递尝试 = 回调槽位当前解析**：`AskUserQuestionTool` 不再在 `resolve_execution` 时一次性快照回调。阻塞与后台两条路径都改为在**每次投递尝试时**从会话共享的回调槽位（`ReverseRpcSlot`）解析当前回调：投递成功 → 进入既有的「gate 双通道收敛」等待；投递返回**通道类错误**（连接关闭/出队失败——回调方以稳定错误签名表达，而非字符串匹配）→ 视为一次「通道不可用」事件，进入等待环。（实现形态，2026-10-24 落地批次：稳定签名 = `reverse_rpc::CHANNEL_ERROR_PREFIX` 常量 + `is_channel_error` 谓词，实现方经该常量构造、消费方经该谓词判定，禁止散落字面匹配。）
- **「从未附着」与「附着后断开」结构性区分**：调用时槽位为空（无头模式、嵌入式未注册处理器）维持既有快速失败语义（SDR-006 范围边界）——等待的前提是用户曾经在场。连接 teardown **不清空槽位**——这是「从未附着 vs 附着后断开」区分的判别前提，属本决策的约束而非偶然现状（若 teardown 清空槽位，所有「附着后断开」将静默退化为「从未附着」快速失败，韧性机制整体失效）；teardown 后槽位仍持有旧回调、投递以其通道类错误判定断开。因此「槽位有回调但投递失败」即「附着后断开」，进入等待；「槽位为空」即「从未附着」，立即按既有无承接方语义收尾（循环中途槽位变空视同通道类错误继续等待，不退出等待环）。
- **gate 注册与 request_id 生命周期**：进入投递循环前一次性向问询门面注册、**全循环复用同一 `request_id`**：每次重试投递的 payload 携带同一 id（客户端去重与卡片锚点）；通道类错误**不撤销** gate 注册（等待期间待决列表可见、`question.answer` 可凭该 id 先行作答——双通道先到先得在等待环内继续成立）；仅在收到响应、用户 dismiss/answer、回合取消或等待上限到期时按各终态收尾/撤销。投递成功后等待作答期间连接再断同样不收尾——gate receiver 仍可经重连后的应答通道解决。
- **等待环 = 限时 + 可取消 + 槽位变更感知**：等待环以固定间隔（500ms）重新解析槽位并重试投递（「错则再等」式重试——不用 `Arc` 身份比较预判，直接以投递结果判定通道是否恢复，语义单一且无需比对语义约定）；退出条件为投递成功（转入 gate 双通道等待）、`ctx.cancel`（用户中止，保持既有取消语义，非错误收尾）、问询 gate 双通道先行解决（等待环让位）、或等待上限到期 → 以可区分的「通道不可用」错误回传模型并附降级指引（纯文本提问或稍后重试），**不触发问询关闭护栏**（护栏只由用户 dismiss 写入）。（上限计时语义，2026-10-24 落地批次：上限只对「通道已断开」的累计等待计时——自首次通道类错误起算、覆盖全部重试间隔；投递在途（通道未判死）与投递成功后的作答等待不受上限约束——作答等待维持既有无限期契约。）**后台路径差异**：后台 detached 问题的 future 无 `ctx.cancel` 可观察（既有结构），其等待环退出条件为投递成功、gate 通道解决或上限到期；后台问题被显式停止（TaskStop）时，其 gate 待决登记随之撤销（SDR-006 范围边界——不依赖上限兜底；实现上经等待环每次 500ms 重试轮询任务状态发现 Stopped——无信号注册使 stop 只翻转状态，轮询是 detached future 唯一可观察停止的面）。
- **重连后的送达**：等待重连的问询经投递循环的重试送达用户眼前（槽位重装后 ≤500ms 内新的 `question.ask`）。问询卡片在客户端侧的重现以此重投递为机制；approval 式的订阅期 `list_pending` 重推不适用于问询（挂账见 ROADMAP 开放项 11e）。
- **范围边界**：无 gate 内核的裸嵌入式路径（`None => callback.call(...)` 直调）保持现状立即失败（该路径无待决事实源，挂起无法被外部解决）；审批的 `approval.ask` 投递路径不在本 ADR 变更范围（见 SDR-006 范围边界）。阻塞路径的等待环位于既有 `drive_ask_with_keepalive` 包裹之内，存活心跳覆盖等待全程（stall watchdog 不误杀）；后台路径不阻塞回合，无 stall 暴露，不需心跳。

**备选与否决理由**：(a) 在传输层（ws_rpc 连接 teardown）把断连缓冲为重连重放——把连接语义埋进传输层，嵌入式/ACP 等其他宿主无法受益，且 teardown 的 fail-fast（防往死 outbox 排队）本身是正确防御，不应削弱；(b) 槽位改为 watch 通道实现精确唤醒——接口形态改动波及所有持有 `ReverseRpcSlot` 的装配点，收益只是把「秒级重试」变「即时」，对分钟级的重连场景不成比例；(c) 问询工具直接持有 SessionManager 引用跨层查询会话状态——工具层依赖会话管理器，层次倒挂。

**后果**：`AskUserQuestionTool` 的 `resolve_execution` 不再捕获回调快照；阻塞路径与后台路径各自内嵌投递循环。回调 trait（`ReverseRpcCallback::call`）的错误约定需补文档化语义（通道类错误 vs 业务类错误），ws_rpc 的 `WsReverseRpcCallback` 与 embedded 的 `ReverseRpcCallbackImpl` 按约定返回通道类错误签名。等待上限为服务级配置（缺省 5 分钟），热更新语义随服务配置既有规则。SDR-006 同批落档。

### ADR-009（2026-10-25）问询事件承载完整载荷：触达通道自足，作答通道不变

**背景与动机**：问询投递有两条线路，职责不同——反向 RPC 是唯一的**作答**通道（引擎等它的回包），`event.question.requested` 是**触达**通道（durable、可游标重放、可跨客户端）。当触达通道只携带第一题的摘要文本时，任何仅凭它建卡的客户端只能渲染出没有选项的降级卡；而客户端为两个来源合成的卡片共用同一个请求 id，去重策略一旦是先到先得，降级卡就会挤掉完整卡，用户直接失去可点选的选项。

**决策**：`event.question.requested` 的载荷同时携带完整问题项（`questions`）与第一题摘要文本（`question`）。

- **职责边界不变**：作答仍只走反向 RPC 与其 `question.answer` 回落；事件不承接任何作答语义。
- **摘要字段保留**：只读摘要的消费者（命令行打印模式、契约镜像）不需要逐项解析，`question` 继续存在，值等于第一题文本。
- **标识符在投影期铸造**：事件本身以工具入参原文承载问题项（无 id）；item 的 `q_<idx>`、option 的「label 即 id」由投影统一补齐，与快照读面共用同一实现——两处各自铸造必然漂移，而 option id 必须与引擎回传模型的可读答案一致。
- **客户端去重改为信息量优先**：同一请求 id 的重复公告中，条目数与选项数更多的卡片胜出，内容相同的重复为无操作。

**备选与否决理由**：(a) 让事件继续只带摘要，由客户端收到后回拉快照——每题多一次往返，且仍要处理与直播卡的合并时序，复杂度更高；(b) 删除触达通道、只留反向 RPC——触达通道的价值恰在反向 RPC 失效的场景（重连、第二客户端），删除等于放弃这些场景；(c) 由客户端按需自行补全选项——客户端无从得知完整选项，不可能。

**后果**：持久事件日志每道问询多存至多 4 题 × 4 选项的内容（低频、有界）。`event.question.requested` 的载荷契约变更登记于协议镜像测试；开放项 11e（重连后按快照重推卡片）仍保持独立裁决——本记录只让事件自身自足，不引入新的重推机制。
