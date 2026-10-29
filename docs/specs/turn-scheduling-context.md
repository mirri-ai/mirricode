# 回合调度域现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点盘点，随侦察/现状变化更新。
> 快照：2026-10-26（§3.7 新增硬中断取消投影失真证据；§3.3 为 2026-10-16；§3.2 为 2026-10-15；其余 2026-10-09）·
> 配套设计文档：[turn-scheduling-requirements.md](turn-scheduling-requirements.md) · [turn-scheduling-architecture.md](turn-scheduling-architecture.md) ·
> 相邻域：[subagent-context.md](subagent-context.md) §6.13（等待语义）· [session-management](session-management-requirements.md)（回合术语 S7）·
> 本文不描述目标、不含裁决。

## 1. TS v2 引擎现状（参照基线）

### 1.1 回合服务（`packages/agent-core-v2/src/agent/loop/loopService.ts`，1305 行）

- **结构**：`AgentLoopService`（Agent scope，`registerScopedService(LifecycleScope.Agent)`，loopService.ts:102,1263-1269）；头注释（:1-30）即总纲："Owns a FIFO of Turn jobs, each with its own `StepRequestQueue`"。
- **入队**：`enqueue()`（:179-194）为每个请求建 `pendingAssignments`（controlled promise）；静默期（quiescence）进 `heldAdmissions`（:185-189），释放时重新准入（:258-273）。
- **回合创建**：`createAndQueueTurn()`（:221-231）——`newTurn` 请求须带 `turnSeed` 否则 BugIndicatingError；`createPendingTurn()`（:334-354）构造 `TurnJob`（:1196-1205：request/seed/controller/ready/result/queue/steps/turn，每回合独立队列 + AbortController）；`reserveTurnId()`（:356-361，对齐 wire `TurnModel.nextTurnId` 单调 id）；`moveStandaloneStepsTo()`（:363-367）把闲置期独立队列的请求绑到新回合。
- **Turn 契约**（`loop.ts:107-114`）：`{ id, state?: 'queued'|'running'|'completed'|'failed'|'cancelled', signal, ready, result, cancel() }`——**state 不含 'blocked'**，但 `TurnEndReason`（turnEvents.ts:21）与 wire op `turn.ended` 的 enum 含 blocked（turnOps.ts:84）：**v2 loop 从不发布 reason 'blocked'**（`LoopRunResult` 联合 loop.ts:72-87 只有 completed|failed|cancelled；goalService 的 blocked 分支 goalService.ts:851,894 是保留位——hook 阻断走 prompt 层 promptService.ts:202-204，turn 从未启动）。
- **启动**：`pumpTurns()`（:439-447）守卫 `disposing || quiescenceDepth > 0 || activeTurnJob !== undefined` 后 `pendingTurns.shift()`（纯 FIFO）；`startTurn()`（:449-461）dispatch `promptTurn` wire op、置 running、`activeTurnJob = job`、发布 `turn.started`（`prompt` 字段仅对可展示来源填充——user / user-slash 的 skill_activation / plugin_command，turnEvents.ts:46-52）。（FIFO 术语：先来先跑）
- **结束时序**（runTurn 的 finally，:493-543）：`settleTurnReady`（:494）→ `releaseActiveTurn`（:565-575：取消残余 step、清活动位）→ dispatch `endTurn`（:505）→ 发布 `turn.ended`（:506-513，含 origin/prompt/reason/error/durationMs/interruptReason）→ 遥测（:529-539）→ `pumpTurns()`（:542）——**下一回合在 turn.ended 发布之后才启动**。`interruptReasonFor()`（:1241-1252）：cancelled→`user_cancelled|aborted`、`isMaxStepsExceededError`→`max_steps`、PROVIDER_FILTERED→`filtered`、其余→`error`。
- **step 循环**：`run()`（:608-634）`while(true)`：`beginLoopStep`（:649-683）→ `executeLoopStep`（:800-844，一次 LLM 请求 + 工具执行）→ `completeLoopStep`（:685-704）→ 错误走 `handleLoopStepError`（:706-714）。**步数无固有上限，队列耗尽即 completed**（:652-660）；**step cap**：`maxStepsPerTurn`（`[loopControl]` 段，仅 >0 时生效，:661-664）超限抛 `createMaxStepsExceededError`（loop.ts:19-30，code `LOOP_MAX_STEPS_EXCEEDED`）。
- **取消**：`cancel()`（:243-249）→ `cancelActiveTurn`（:275-284：dispatch `turn.cancel` + controller.abort）/ `cancelQueuedTurn`（:286-299：出队、cancel 全部 step、state=cancelled、resolve `{type:'cancelled', steps:0}`）；step 级 `cancelStep`（:429-437）；`handleLoopCancellation`（:716-734）区分 step 取消（继续 turn）与 turn 取消（返回 cancelled）；`dispose()`（:161-177）取消一切。逐请求撤销：`EnqueueReceipt.abort`（:190-193）→ `abortRequest`（:383-403：撤 held / 撤整个排队 turn / 撤单步）。
- **错误恢复注册表**：`registerLoopErrorHandler`（:577-599，before/after 有序插入）；已注册：`full-compaction`（fullCompactionService.ts:204-209）与 `stepRetry`（stepRetry/stepRetryService.ts:83）；first-match-wins，返回 true 表示已自行续跑。
- **持久化**：`agentState` 键 `loop.nextReservedTurnId` / `loop.lastRequestTraceId` / `loop.disposing`（:92-100）；`pendingTurns`/`activeTurnJob` 持有资源保持 plain fields（:24-29 注释）。

### 1.2 StepRequest 体系（`agent/loop/stepRequest.ts` + `agent/prompt/promptStepRequests.ts`）

- 基类 `StepRequest`（stepRequest.ts:40-85）：默认 `mergeable=false`、`turnScoped=true`、`admission='activeOrNextTurn'`。`StepRequestAdmission`（:23-27）四值：`'newTurn' | 'activeOrNewTurn' | 'activeOrNextTurn' | 'activeTurnOnly'`。`TurnSeed = { input, origin }`（:29-32）。
- 具体类型全集：

| 类型 | file:line | kind | admission | mergeable | turnScoped |
|---|---|---|---|---|---|
| `MessageStepRequest` | stepRequest.ts:91-109 | `'message'`（可覆写） | 继承选项 | false | true |
| `ContinuationStepRequest` | stepRequest.ts:111-122 | `'continuation'` | 默认 `activeOrNextTurn` | false | true（无 turnSeed，不能开 turn；`resolveContextMessages()` 返回 `[]`） |
| `PromptStepRequest` | prompt/promptStepRequests.ts:63-77 | `'prompt'` | `newTurn` | false | true |
| `SteerStepRequest` | promptStepRequests.ts:79-105 | `'steer'` | 默认 `activeTurnOnly`（可选 activeOrNewTurn） | **true** | **false**（跨 turn 存活） |
| `RetryStepRequest` | promptStepRequests.ts:107-120 | `'retry'` | `newTurn` | false | true（turnSeed 空输入） |
| `TaskNotificationStepRequest` | task/taskService.ts:223-239（extends MessageStepRequest） | `'task_notification'` | `activeOrNewTurn` | **true** | **false** |
| stop-hook 续步 | externalHooks/externalHooksService.ts:216-222 | `'stop_hook'` | `activeOrNextTurn` | true | true |

- 构造方：promptService（steer/inject/retry/prompt，promptService.ts:151,175,181,206）、taskService.ts:1125、goalService.ts:828,937、loopContinuationService.ts:35、externalHooksService.ts:217。

### 1.3 准入分派（`admit()`，loopService.ts:196-219）

**四分支**（非两分支）：

```ts
case 'newTurn':          createAndQueueTurn(request);
case 'activeOrNewTurn':  active ? assignStep(active, request) : createAndQueueTurn(request);
case 'activeOrNextTurn': active ? assignStep(active, request) : standaloneStepQueue.enqueue(request, at ?? 'tail');
case 'activeTurnOnly':   active ? assignStep(...) : BugIndicatingError + rejectAssignment(:210-216);
```

- 忙折入：`assignStep()`（:369-375）→ `enqueueStep`（:405-427，`at:'head'|'tail'` 默认 tail）。
- 闲置暂存：`standaloneStepQueue`（:110），回合创建时 `moveStandaloneStepsTo` 整体收编（:363-367）。
- 队列批次折叠：`StepRequestQueue.takeNextBatch`（stepRequestQueue.ts:38-55）——首个非 mergeable 请求为 driver，全部 mergeable 请求 fold 进该 driver 的 step；其余非 mergeable 留队列驱动后续 step；`at:'head'` 供失败重试插入。文件头注释（stepRequestQueue.ts:7-13）："turn-scoped requests are aborted while agent-scoped requests (steers) carry into the next turn"。

### 1.4 唤醒源全景（驱动源 → 入口 → 准入 → origin）

`PromptOrigin` 联合（contextMemory/types.ts:91-104）：`user | skill_activation | plugin_command | injection | shell_command | compaction_summary | system_trigger | background_task | cron_job | cron_missed | hook_result | hook_additional_context | retry`（各形状 types.ts:7-89，如 `CronJobOrigin{jobId,cron,recurring,coalescedCount,stale}` :61-68）。

| 驱动源 | 入口 file:line | StepRequest/准入 | origin |
|---|---|---|---|
| 用户消息 | rpcService.ts:103-111、gatewayService.ts:52-67 → promptService.enqueue | Prompt（newTurn） | user |
| 用户插话 | rpcService.ts:114-124 / gatewayService.ts:68-83 → promptService.steer（:133-160） | Steer（activeTurnOnly, mergeable） | user |
| 系统注入（cron/shell 后台/通用） | promptService.inject（:173-179） | Steer（activeOrNewTurn） | 调用方给定 |
| cron fire | session/cron/sessionCronServiceImpl.ts:455-517 | inject → Steer(activeOrNewTurn) | cron_job |
| cron missed（恢复补发） | sessionCronServiceImpl.ts:404-428 | 同上 | cron_missed |
| 任务通知 | taskService.ts:1121-1149 | TaskNotification（mergeable, activeOrNewTurn） | background_task |
| 提问答案送达 | tools/ask-user-question/question-background-task.ts（头注 :5-9） | 即 background_task 通知路径 | background_task |
| goal 续航 | goalService.ts:923-956（origin 常量 :117-120） | Message（newTurn） | system_trigger/goal_continuation |
| goal 终态续步 | goalService.ts:817-829 | Continuation（默认 activeOrNextTurn，无消息） | 无 |
| 工具步续步 | loop/loopContinuationService.ts:32-36 | Continuation（activeOrNextTurn） | 无 |
| stop hook 续步 | externalHooksService.ts:198-224 | Continuation（mergeable） | system_trigger/stop_hook |
| retry | promptService.ts:181 | Retry（newTurn） | retry |
| 压缩后重注入 | fullCompactionService.ts:443-471：**不入队**——`LoopErrorContext.retry`（loopService.ts:749-760）把失败 driver `{at:'head'}` 重插；摘要本体 origin compaction_summary（compactionHandoff.ts:111） | 原 driver 复用 | compaction_summary |
| undo 对账 | undo/undoService.ts:100-122：`tryAcquireQuiescence` 排他 + `reconcileParticipants()`（:182-194）——**不驱动新 turn**；任务状态改惰性注入（taskService.ts:329-332 injector `background_task_status`） | — | injection |
| print steer | task/printModeService.ts:40-168：只做 finish/continue 决策，新回合本体仍由任务通知触发 | — | — |
| btw 侧问 | session/btw/btwService.ts:31-49（fork 'main' 子 agent） | 子 agent 走正常 prompt(newTurn) | system_trigger/btw |
| 子代理运行 | session/subagent/runAgentTurn.ts:31-32,54,140 | Prompt(newTurn) | system_trigger |
| shell 命令 | shellCommandService.ts:208-238：输入输出直接 context.append（不驱动 turn）；后台化时 inject，origin `injection/shell_command_backgrounded` | Steer(activeOrNewTurn) | shell_command / injection |

### 1.5 turn.ended 的全部消费方（全库 7 处订阅）

| 消费方 | file:line | 行为 |
|---|---|---|
| goalService | goalService.ts:400-407 | 续航/异常结算（**唯一能再 enqueue 新 turn 的消费方**） |
| agentLifecycleService | session/agentLifecycle/agentLifecycleService.ts:110-116 | `cancelPendingForTurn(turnId)` |
| activityViewService | activityViewService.ts:125-127 | 活动视图投影 |
| fullCompactionService | fullCompactionService.ts:186-190 | 清活动回合追踪 |
| externalHooksService | externalHooksService.ts:190-194 | Stop/TurnEnd hook 通知 |
| swarmService | swarmService.ts:47-53 | `shouldAutoExit` 时退出 |
| interruptionReminderService | interruptionReminderService.ts:49-54 | 仅用户取消时记中断提醒 |

订阅在同一事件总线按订阅顺序同步执行；`pumpTurns()` 在 turn.ended 发布后（:542），故全部订阅者先于下一 turn 的 turn.started。TUI 不直接订阅（走 activityView/delta 事件）；printModeService 不订阅（由外层驱动调用）。

### 1.6 插话与静默期

- **steer 折入**：`SteerStepRequest`（mergeable:true）被 takeNextBatch fold 进当前 driver 的同一 step——stepRequestQueue.ts:7-10 头注原文："this is how steers land in the same LLM request as pending tool results or a fresh prompt instead of each costing its own step"。materialize 时写 `turn.steer` wire op（promptService.ts:151-153）。
- **⚠️ 跨回合存活与实现矛盾**：`turnScoped:false` 使其在 run 结束清理时不被 abort（`abortTurnScoped` 只 abort turnScoped 项，:61-66），队列头注（:10-13）声称 "agent-scoped requests (steers) carry into the next turn"——但 **`releaseActiveTurn`（loopService.ts:565-575）在回合结束时无条件 cancel `job.steps` 中全部排队/运行中步骤**（含非 turnScoped 的插话）。回合正常完成时队列已耗尽、无残留（承诺空转成立）；**异常终止路径（钩子停回合/步数上限/provider 失败/用户取消）的残留插话实际被丢弃、从不带入下一回合**（与头注意图矛盾，v2 无测试锁定该生存性；裁决记录见 ROADMAP O6，requirements US3.2 已按现状如实化）。
- **quiescence**：`tryAcquireQuiescence()`（loopService.ts:251-256）排他窗口（undo 使用）；期间新请求进 `heldAdmissions`，释放时（:258-273）重新 admit + pumpTurns。

### 1.7 配置

`[loopControl]` 段（agent/loop/configSection.ts:24）：`maxStepsPerTurn`（int ≥0，**optional 无代码默认 → 不设即不限**）、`maxRetriesPerStep`、`maxRalphIterations`、`reservedContextSize`、`compactionTriggerRatio`；env `MIRRI_LOOP_MAX_STEPS_PER_TURN`（:26,46-49）；legacy `max_steps_per_run` → `maxStepsPerTurn` 改名（:56-60）。

## 2. v1 对照（`packages/agent-core`）

v1 对应物 `src/agent/turn/index.ts` 的 `TurnFlow`（:109 起）：`steer()`（:146-163）在活动 turn 或压缩中把输入 push 进 `steerBuffer: BufferedSteer[]`（:110）并返回 null；`launch()`（:169-189）对 user/skill_activation/plugin_command/retry 与活动 turn 竞争时缓冲为 steer，**其余 origin 直接抛 `turn.agent_busy`**。无 turn 的纯循环 `src/loop/run-turn.ts` 的 `runTurn()`（:88-203，maxSteps :131-133，stop-hook 续航 :175-185）。
**一句话差异**：v1 用运行中 turn 内的 steer 缓冲数组（beforeStep flush、系统 origin 竞争时发布 `turn.agent_busy` 错误事件并返回 null——不抛出异常，调用方经错误事件感知），v2 统一为带 admission 类型的 StepRequest 队列准入（mergeable 折入 + 跨 turn 存活），无 busy 硬错误路径。

## 3. Rust 对照

`rust/crates/mirri-core`：`src/loop_engine/`（无状态 `run_turn`/`execute_loop_step`）+ `src/turn_flow/`（有状态宿主 `TurnFlow`，`steering.rs` 的 `SteerBuffer::push/flush/take_items`——头注 "Steer buffering — mid-turn user interjections."，缓冲在下一个 `before_step` 钩子 flush）。
**一句话现状**：Rust 复刻 v1 形态（steer 缓冲 + before_step flush + `turn.steer_flushed`），**尚无 v2 式 admission 分类 StepRequest 队列**。

### 3.1 ⚠️ 现状痛点：空响应步被静默记为"回合完成"

真实会话证据（2026-10-09，v3 rust core 会话 `session-b1a4d19b-*`，wire.jsonl 完整复盘）：

- 回合最后一步的模型回应**计费了 109 个输出 token，但无正文、无思考、无工具调用**（step.begin → usage.record → step.end，中间零内容事件），回合随即 `turn.ended reason: "completed"`。
- 收尾链路（`loop_engine/turn_step.rs` 的 `finalize_step`）：正文与思考为空时不追加任何内容事件；停止原因归一化后无工具调用 → EndTurn → 回合驱动循环（`run_turn`）按自然终局收尾。**没有任何分支识别"自然结束但零产出"这一退化形态**——空回应被当作正常完成处理。
- 后果：用户视角下工作停在半途（最后可见输出是一句"我需要再看一下……"的中间态陈述），无错误、无提示、无法区分模型自行停止还是系统吞了输出；只能靠手工复盘 wire 日志定位。
- 对照：压缩链路（`compaction/round.rs`）已把空响应当作触发条件处理，主 agent 循环无对应逻辑；取消路径（中断锚点）有可见标记机制可复用。
- 缺陷定性：违反本域 US1.2"回合结局如实"；修复需求与设计见 SDR-002 / ADR-002。
- **框定**：本节为发现时点（2026-10-09）的证据快照；SDR-002/ADR-002 的修复与证据同批落盘，快照描述的是修复前行为。

### 3.2 ⚠️ 现状痛点：僵死看门狗误杀健康等待、终止全程不可归因（两起）

真实会话证据（2026-10-15，v3 rust core + desktop，两个会话 wire.jsonl + 服务端日志完整复盘；快照日期 2026-10-15，描述修复前行为）：

- **共同模式**：代理启动长验证任务（cargo test 339s / 修复验证任务）后调用 `TaskOutput(block:true, timeout:600)` 阻塞等待——等待期间模型无输出、引擎无内容事件，`run_turn_with_stall_deadline`（`rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs:414`）以**纯静默时长**为判据的僵死看门狗在 300 秒（`ENGINE_TURN_STALL_TIMEOUT`）后 fire 取消令牌；取消对阻塞中的 TaskOutput 不生效，回合继续僵尸存活（5 分钟 / 96 秒）直到任务自然结束、TaskOutput 返回、回合才以 `turn.ended reason:"cancelled"` 落地——与用户取消同词。**看门狗触发分支零日志**，两起事故的归因只能靠 wire 时间差反推。
- **会话一**（`session-aad4bb23-*`，turn 52，持续 690s）：TaskOutput 等待 cargo test 结果 377s；看门狗约 300s 时取消；任务 exit 0 完成、结果完整返回（4071 测试通过）但回合已死。
- **会话二**（`session-6d39689c-*`，turn 5，持续 812s）：TaskOutput 等待修复验证任务 596s；看门狗约 300s 时取消；任务自身 600s shell 超时被杀（exit -1，服务端另有 stale heartbeat 告警）；三层超时（看门狗 300s / 任务 600s / TaskOutput 600s）纠缠，根因同为看门狗误判。
- **四层缺陷定性**：判据（工具等 IO 被当引擎僵死——工具执行期间活动信号不重置计时）、取消（阻塞等待不参与协作式取消，取消后僵尸期 96s–5min）、可见性（触发零日志、cancelled 与用户取消同词）、恢复（任务结果返回时回合已死，用户需手动恢复且无人提示未消费结果）。
- **对照**：SDR-002（空响应静默完成）同属"系统性终止/收场对用户不可解释"缺陷家族；该次修复（空响应守卫）证明"结构化判定 + 可见收场"路径在本域可行。
- 修复需求与设计见 SDR-003 / ADR-004；跨域恢复提案见 proposals P-028。

### 3.3 ⚠️ 现状痛点：等待用户决策被看门狗误杀、ACP 权限请求固定超时（2026-10-16 快照，描述修复前行为）

与 §3.2 同判据缺陷但等待对象是**用户本人**的两处暴露（代码勘察结论，2026-10-16）：

- **审批等待被误杀**：`approval.ask`（`rust/crates/mirri-core/src/turn_flow/hooks.rs` 的 Ask 臂）等待用户批准期间引擎无内容事件——hooks 层无工具进度通道，等待不产生任何活动信号；300 秒静默后看门狗取消回合，收尾映射为"cancelled by user"（错误归因——用户并未取消）。计划确认（ExitPlanMode 的 plan-review Ask）、目标启动确认、敏感文件/git 路径确认全部经同一 Ask 臂汇入，同暴露。
- **ACP 固定超时**：`PERMISSION_REQUEST_TIMEOUT = 60s`（`rust/crates/mirri-acp/src/main.rs:46`，env `MIRRI_ACP_PERMISSION_TIMEOUT_MS` 可覆写）应用于**所有**反向 RPC 请求——用户决策类（权限请求、用户征询）也在其列：IDE 用户 60 秒未回应即按超时拒绝执行。
- **既有保护的事实**：提问工具阻塞路径已有工具层保活（`drive_ask_with_keepalive`，2026-10-16 落地）；审批门/提问门是未决请求的唯一事实源（pending 计数可查）。
- 修复需求与设计见 SDR-004 / ADR-005。

### 3.4 实现勘察锚点（ADR-004 落地批次用，2026-10-15）

> 为 O8 实现批次提供的 file:line 锚点与机制事实；随实现推进失效即删。

- **看门狗本体**：`run_turn_with_stall_deadline`（`rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs:414`）——progress watch channel 由 `LiveEvent` 回调驱动（:289-298 包装 live_callback，每个 LiveEvent `send(())`）；静默超时先 `cancel.cancel()`（系统语义）再等一个窗口，仍不收场则硬丢弃并在丢弃路径记 `TurnEnded reason:"failed"`（:348-356）。**两级收尾都无日志**（硬丢弃有 wire 结算但无 tracing 日志）。
- **活动信号管道（已存在）**：工具 `ctx.progress`（`ToolProgressHandle`）→ `LiveEvent::ToolProgress`（`rust/crates/mirri-core/src/tool_registry/execution.rs:384-393` 接线）→ live_callback → progress watch channel。
- **任务心跳**：`TaskManager::heartbeat`（`rust/crates/mirri-core/src/task.rs:1589`，由输出追加触发）维护 `last_heartbeat`；`is_stale`（:1620）判定心跳过期——Running 状态本身不证明存活。心跳事件当前不产生任何 LiveEvent（到不了看门狗）。
- **TaskOutput 阻塞等待（取消不响应的根因）**：`TaskManager::wait_for_completion`（`rust/crates/mirri-core/src/task.rs:1706`）50ms 轮询任务状态，**不 select 回合取消令牌**——取消后等待继续到自然超时/完成；非测试调用方仅 TaskOutputTool 一处。
- **取消来源**：看门狗与用户取消共用同一 `CancelToken`（`cancel.cancel()` 系统语义 vs `cancel_user()` 用户语义——`rust/crates/mirri-core/src/cancel.rs:115` 可区分，但收尾路径 `coerce_aborted_if_cancelled`（`rust/crates/mirri-embedded/src/embedded_server/mod.rs:928`）只认 token 是否 fired，不区分来源）。工具执行收到的 `ctx.cancel` 即回合级令牌（`tool_registry/execution.rs:394-399`）。
- **子代理回合**：`subagent/host.rs` 直接调 `turn_flow.run_one_turn`，**无看门狗包装**（`run_turn_with_stall_deadline` 仅 mirri-embedded 主回合/任务/定时路径）——后台/swarm 子代理靠任务心跳 stale + 成员超时兜底（另一套机制，不产生 stalled）。
- **结局映射单点**：`turn_stop_reason_wire`（`rust/crates/mirri-core/src/session/event_projection.rs:1002`）`Aborted → "cancelled"`；`TurnStopReason` 枚举（`rust/crates/mirri-core/src/loop_engine/mod.rs:80`）无 Stalled 变体。wire op `TurnEnded.reason` 是自由字符串（`rust/crates/mirri-proto/src/wire.rs:226-229`）；reducer 有 `"cancelled"` 匹配（`rust/crates/mirri-proto/src/reducer.rs:343`）。
- **消费端（web）**：`turnActiveChanged` 事件带 `reason`（`apps/mirri-web/src/api/types.ts:724`；`agentEventProjector.ts:1291` 透传）；`useMirriWebClient.ts:1102` 按 `cancelled/failed/filtered` 三值归并 `aborted`——**未知值（含将来的 stalled）落 'idle' 分支，终止信号会被吞**；i18n `aborted: 'Stopped'/'已中断'`（`apps/mirri-web/src/i18n/locales/{en,zh}/workspace.ts`）；会话状态 `lastTurnReason` 三值联合（`apps/mirri-web/src/types.ts:37`）。
- **TUI/desktop**：turn 终态渲染未见独立 reason 匹配（桌面端走 web 通道）。

### 3.5 ⚠️ 现状痛点：ADR-004 落地后 48 小时内三起新形态误杀（2026-10-17 快照，描述 P-029 提案前行为）

ADR-004 重设计落地后，推模型活动信号判据在三起事故中暴露新的等待点缺口（三起事故的会话记录均在本工作区 `~/.mirri-code/v3_sessions/wd_mirricode_963bbfd6e198/`）：

- **等待模型首响应**（2026-10-17 23:13，session-fdf98268）：前台子代理的模型请求在途（等 provider 首 token 约 6 分钟），流式响应首事件到达前引擎无内容事件也无任何活动信号——父回合静默越窗被取消（当时词表记 cancelled，turn 47，时长 68 分钟）。子代理自身的任务驱动回合同样被杀（turn 61，501 秒）。
- **等待转后台进程**（2026-10-18 05:48，session-f1e24e04 turn 21）：子代理前台命令超时自动转后台续跑，续跑循环不打任务心跳；子代理 `TaskOutput(block=true)` 阻塞等待探测不到心跳推进 → 活体信号链路无源 → 父回合静默 600 秒越窗，turn 21 落盘 stalled（时长 5205 秒），子代理被前台孤儿守卫取消。
- **fork 链路喂狗错位**（2026-10-17 18:23，session-7049e890 的子代理 agent-86d01f09 turn 8）：fork 出的子代理会话共享源会话的任务管理器，子代理活动喂到了源会话的看门狗时钟上——喂狗线接错对象，正确时钟断粮，355 秒误杀。
- **对照事实**：三起事故期间看门狗无一次正确捕捉真僵死；模型挂死场景已被 HTTP 读超时（240 秒量级）独立覆盖。
- 推模型结构性缺陷分析与拉模型三代提案见 P-029 / [turn-scheduling-watchdog-pull-model-proposal.md](turn-scheduling-watchdog-pull-model-proposal.md)。

### 3.6 喂线机制现状清单（P-029 提案的 As Is 基线，2026-10-18 快照）

> 推模型下"健康但安静"等待点的喂线接线现状——P-029 的取代对象。随实现推进失效即删。

- **已接线（逐起事故后补）**：模型首响应等待的 `LlmPending` 心跳（30 秒一拍，仅首事件前）；spawn 路径后台任务的进程心跳（心跳窗口 1/4 周期）；前台子代理的活动转发（子代理 live 事件 → 父回合活动钟）；TaskOutput 阻塞等待的心跳推进探测（经共享任务管理器的 liveness listener 喂父回合）；提问工具的 keepalive（75 秒 tick）。
- **缺失（开放集合）**：前台转后台续跑的进程心跳（§3.5 第二起）；前台 AgentSwarm 的活动转发（成员是 Agent 任务类型，被进程心跳服务显式排除）；hooks 层审批等待（ADR-005 以门登记豁免覆盖）。
- **静态窗口**：`ENGINE_TURN_STALL_TIMEOUT` 默认 600 秒（300 秒时代的两次误杀见 §3.2）；`turnStallTimeoutMs` 配置可覆写。

### 3.7 ⚠️ 现状痛点：硬中断的用户取消被投影成"已失败"（2026-10-26 快照，ADR-007 修复前）

- **两条取消收场路径，两种结局词汇**：协作收场——引擎在等待点观察到取消令牌后自了结，经 `TurnCompleted(stop_reason: Aborted)` → wire reason `cancelled`（`turn_stop_reason_wire`，`rust/crates/mirri-core/src/session/event_projection.rs`）；硬中断收场——`message.abort` / `goal.pause` / `goal.cancel`（`cancel_in_flight_turns`，`rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs`）对 fire-mode 回合直接发布 `TurnFailed(error_code: "abort.user_cancelled")`，由中断侧关闭回合括号。
- **失真点**：`SessionEvent::TurnFailed` → `event.turn.ended` 投影对**所有**失败事件一律输出 `reason: "failed"` + 结构化 error 对象（含 `abort.user_cancelled`）——用户取消被压平成"已失败"。
- **前端放大**：web 投影器见 `reason === 'failed'` 产出 agent-error 通知；错误码→标题映射表（`AGENT_ERROR_TITLE_KEYS`）无 `abort.*` 条目，落兜底标题"Model request failed"——用户主动终止被渲染成模型请求失败的错误 toast（含 Diagnostics 面板）。
- **引擎自身口径已正确**：活动视图（`activity_shadow.rs` `outcome_of_failure`）把 `error_code` 以 `abort.` 为前缀的失败归为 Cancelled——引擎内语义正确，唯独 wire 投影层失真。
- **`abort.user_cancelled` 的产生源全量核实（2026-10-26）**：(1) `cancel_in_flight_turns` 硬编码路径，调用方仅 `message.abort` / `goal.pause` / `goal.cancel`（全部用户语义 RPC）——**当前唯一会在回合上产生该码的路径**；(2) `EngineTurnError::Engine(CoreEngineError::UserCancelled).domain_code()` 语法上存在，但 `UserCancelled` 仅由 `CancelToken::cancel_user()` 产生（`rust/crates/mirri-core/src/cancel.rs`），其生产调用方仅 `shell.cancel` 与 shell 前台转后台——均打在 shell 命令令牌上而非回合令牌，**当前生产代码中路径 (2) 不可达**（若未来出现用户语义的回合令牌取消，也应归此码，精确匹配双向安全）。结论：该错误码可安全等价于"用户发起"。
- **错误码族辨析**：`abort.*` 族含 `abort.user_cancelled`（用户）与 `abort.cancelled`（系统，如作用域关闭）——投影判定必须精确匹配 `abort.user_cancelled`，前缀匹配会把系统取消误归用户取消。

## 4. 已知陷阱（写入基线时已核对）

- `TurnEndReason`/wire enum 含 `'blocked'` 但 v2 loop 从不产生它（goal 的 blocked 实际由 UpdateGoal 工具路径触发，非 turn reason）。
- `admit()` 是 4 分支而非 2 分支——`activeOrNextTurn`（Continuation 默认）与 `activeTurnOnly`（Steer 默认）同样是核心准入类别。
- 单活动回合不变量的实现载体是 `activeTurnJob` 唯一字段（loopService.ts:115），`status()` 的 `'idle'|'running'`（:233-241）同源。

## 5. 与相邻域的关系

- **goal 域**：续航是本域最大消费方（newTurn 同步入队、先于 pumpTurns）；等待挂起的机制侧（续航发放暂停）按 turn-scheduling-architecture ADR-001 落位本域。
- **background-tasks 域**：任务通知/定时注入是唤醒源的两个既有实例（准入/合并走本域，业务语义在彼域）；其 ADR-008（等待挂起）的机制侧被本域收编。
- **会话管理域**：撤销排他窗口（undoService）消费本域静默期；S7「回合」术语权威在该域。


