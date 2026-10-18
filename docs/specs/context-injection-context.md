# 上下文注入域 · 现状清单（context-injection-context）

> **快照日期：2026-09-16**。本文为现状清单（代码事实 + 覆盖差异矩阵），不含决策；需求/设计见同域 requirements / architecture 文档。引擎基线：v2（`packages/agent-core-v2`）为主，v1（`packages/agent-core`）以 `[v1]` 标注且仅 `MIRRICODE_LEGACY_FLAG` 门控下被 CLI 消费（web/desktop 零 v1 依赖）。

## 1. v2 代码事实

### 1.1 核心通道

| 事实 | 位置 |
|---|---|
| `appendSystemReminder`：user 角色 + `<system-reminder>` 信封 + `context.append` | `packages/agent-core-v2/src/agent/systemReminder/systemReminderService.ts:17-31` |
| 信封教学（"authoritative system directives"） | `packages/agent-core-v2/src/app/agentProfileCatalog/system.md:33` |
| 注入不与相邻用户消息合并（origin 非 user） | `packages/agent-core-v2/src/agent/contextProjector/contextProjectorService.ts:641-643` |
| 暂存缓冲（未闭合工具交换期间延迟落地，无上限） | `packages/agent-core-v2/src/agent/contextMemory/loopEventFold.ts:108-133,240-243`、`contextTranscript.ts:62-95` |
| 系统提示模板变量（cwd/now/cwd_listing/agents_md 等，静态组装非注入） | `packages/agent-core-v2/src/app/agentProfileCatalog/profile-shared.ts:87-145` |
| explore 代理 `<git-context>` 前置块（静态） | `packages/agent-core-v2/src/session/agentLifecycle/profile/gitContext.ts:45-113` |

### 1.2 条件注入框架

| 事实 | 位置 |
|---|---|
| 框架本体：register/inject/isNewTurn/位置记账/splice 对账/restore resync | `packages/agent-core-v2/src/agent/contextInjector/contextInjectorService.ts:39-172` |
| `isNewTurn` 持久化于 agentState | 同上 `:34-37,52,60-63,77-83` |
| 挂在 `onWillBeginStep` 末段 | 同上 `:53-58`；LLM 请求在钩子链完成后发出 `agent-core-v2/src/agent/loop/loopService.ts:809-816` |
| 压缩后重注 `injectAfterCompaction` | `agent-core-v2/src/agent/fullCompaction/fullCompactionService.ts:562` |

### 1.3 框架层 provider 注册点

| 变体 | 注册点 | 内容/条件 |
|---|---|---|
| `plugin_session_start` | `agent/plugin/agentPluginService.ts:50-57` | 首注 + 目录变更刷新 + 中和声明（`:126-147`） |
| `goal` | `agent/goal/injection/goalInjection.ts:20` | 三档模板 + `<untrusted_objective>` 转义 + 预算引导 |
| `plan_mode` | `agent/plan/injection/planModeInjection.ts:45-66` | full/sparse/re-entry/exit；2/5 轮阶梯（`:29-30,72-92`） |
| `permission_mode` | `agent/permissionMode/injection/permissionModeInjection.ts:41-64` | auto 进/出转换沿；历史缺失补注 |
| `todo_list_reminder` | `session/todo/sessionTodoService.ts:100-104` + `session/todo/todoListReminder.ts:15-16,29-41` | 双 10 轮门槛 |
| `background_task_status` | `agent/task/taskService.ts:317-320,329-332,361-367` | 压缩后置 pending |
| `capability_gap` | `agent/capabilityGap/capabilityGapInjection.ts:92-95` | 新回合 + 模态缺口 |

### 1.4 事件型系统提醒（直接调用）

| 机制 | 位置 | 备注 |
|---|---|---|
| btw 侧问声明 | `session/btw/btwService.ts:35`（文案 `btw.ts:15-28`） | fork 时一次 |
| `/init` 完成（含 AGENTS.md 全文） | `session/sessionInit/sessionInitService.ts:116` | flush wire journal |
| goal 取消提醒 | `agent/goal/goalService.ts:671`（文案 `:107-111`） | |
| goal 预算停止提醒 | `agent/goal/goalService.ts:853`（文案 `:144-149`；宽限 `:846-857`） | 一次性宽限步 |
| goal 分叉清除提醒 | `agent/goal/goalService.ts:1143`（文案 `:113-117`；pending 标志 `:218-235`） | wire 待办模型，恢复补发 |
| 工具增删公告 | `agent/toolSelect/toolSelectAnnouncementsService.ts:69`（文案 `dynamicTools.ts:112-129`；差集自愈 `toolSelectService.ts:162-173`） | 首步 + 压缩边界 |
| 打断提醒 | `agent/interruptionReminder/interruptionReminderService.ts:69-76`（文案 `:24-28`；wire 模型 `interruptionReminderOps.ts:15-42`） | 末条持久消息去重；恢复补发 |
| 插件会话启动/中和 | `agent/plugin/agentPluginService.ts:82,87,126-147` | main agent 专属 |
| 蜂群进入/退出（退出可弹出） | `agent/swarm/swarmService.ts:79,94-109` | 弹出 reducer `agent/contextMemory/contextOps.ts:122-132` |
| 图片压缩 caption（ownerPromptId） | `agent/prompt/promptService.ts:255` + `promptStepRequests.ts:50` | 随 prompt 撤销连带删除 `contextOps.ts:305-330` |

### 1.5 嵌入式与消息层

| 机制 | 位置 |
|---|---|
| 工具去重递进提醒（嵌结果尾部，3/5/8/12 阈值） | `agent/toolDedupe/toolDedupeService.ts:34-63,90-108,386-399` |
| cron 触发/补发消息 | `session/cron/sessionCronServiceImpl.ts:404-428,455-488` |
| 后台任务通知（mergeable、去重键） | `agent/task/taskService.ts:223-238,1186-1225` |
| stop hook 续跑消息 | `agent/externalHooks/externalHooksService.ts:207-223` |
| hook 附加上下文 / 阻断结果 | `agent/externalHooks/user-prompt.ts:105-121`、`externalHooksService.ts:269-274,321-343` |
| `!` 直跑命令输入/输出 | `agent/shellCommand/shellCommandService.ts:208-238` |
| 技能激活 / 插件命令 | `agent/skill/skillService.ts:56-99`、`agent/rpc/rpcService.ts:188` |
| 子代理启动 prompt | `session/subagent/runAgentTurn.ts:31-59` |
| goal 续航消息 | `agent/goal/goalService.ts:1011-1077,1041-1077` |
| 动态工具 schema（role=system + tools 字段） | `agent/toolSelect/toolSelectService.ts:125-128,150-157`（发送时按 disclosure 剥离） |

### 1.6 生命周期/投影/持久化

| 事实 | 位置 |
|---|---|
| PromptOrigin 全量 13 值 | `agent/contextMemory/types.ts:7-104` |
| wire 持久化 `context.append_message` | `agent/contextMemory/contextOps.ts:147-150`、`contextMemoryService.ts:65-70` |
| 压缩保留策略（只保真实用户输入） | `agent/contextMemory/compactionHandoff.ts:146-173` |
| 压缩摘要前缀 / elision 信封 | `compaction-summary-prefix.md`、`compactionHandoff.ts:106-128` |
| undo 跳过/连带删除 injection | `contextOps.ts:305-330`、`contextTranscript.ts:172` |
| 消息投影 origin → `metadata.origin` | `app/messageLegacy/messageProjection.ts:113-133`（`session/…` 路径以仓库现状为准） |
| 读侧投影（合并/剥离/降级） | `agent/contextProjector/contextProjectorService.ts:69-97` |

## 2. `[v1]` 代码事实（legacy 路径，`MIRRICODE_LEGACY_FLAG` 门控）

- 核心三原语：`ContextMemory.appendSystemReminder`（`agent/context/index.ts:92-100`）、`DynamicInjector` 基类（`agent/injection/injector.ts:3-39`）、`InjectionManager`（`agent/injection/manager.ts:15-118`）。
- 注入器：TodoList（`injection/todo-list.ts`，文案与 v2 逐字相同）、PlanMode（`injection/plan-mode.ts`，多 re-entry 变体）、PermissionMode、PluginSessionStart（`injection/plugin-session-start.ts`，history 扫描对齐 injectedAt）、Goal（`injection/goal.ts`）、ToolsDiff（`injection/tools-diff.ts`，origin 用 `system_trigger` 而非 injection）。
- `[v1]` 独有细节：`/reload` 强注 plugin session start + 中和（`session/index.ts:766-801`）；本地命令输出 `<local-command-stdout>`（`context/index.ts:117-125`）；`injectAndNotify` steer 式立即注入（`context/index.ts:110-115`，唯一调用 shell 后台化 `agent/tool/index.ts:192-197`）；`<system>` 工具侧信道（`agent/context/tool-result-render.ts:35-38`）；图片 caption 从 `<system>` 内联改道信封注入（`context/index.ts:65-90,727-762` 拆分改道 + `tools/support/image-compress.ts:904-930` 内联模式提取）。
- `[v1]` 无 interruptionReminder 域（中断只触发 goal pause，不注入）。
- 消费状态：v1 全部注入面挂 `MirriCore` 内部，唯一生产入口是 CLI 的 legacy flag 路径（`apps/mirri-code/src/cli/experimental-v2.ts:69-76`、`packages/node-sdk/src/sdk-rpc-client.ts:77-89`）；web/desktop 零触及。**范围限定**：该结论只覆盖 v1 注入面；v1 包的非注入工具函数（模型别名解析、wire 迁移/投影工具等）仍被 acp-adapter 与 vis 无条件引用，legacy 淘汰时需一并迁移。

## 3. TS vs Rust 差异矩阵

Rust 侧根：`rust/crates/`（mirri-core / mirri-embedded / mirri-server / mirri-proto）。

> 执行批次快照（2026-09-17）：interruptionReminder 行、swarm 进出提醒行、goal 事件提醒（cancelled / budget-stop）已接线落地（见 ROADMAP 历史记录 2026-09-17 条）；goal_fork_cleared 维持 N/A。其余行状态为 2026-09-16 首次基线快照，未复核。

| TS v2 机制 | Rust 对应 | 状态与差异 |
|---|---|---|
| `appendSystemReminder` | `mirri-core/src/context_memory/mod.rs:411-448` `append_system_reminder` | ✅ 发给模型的消息逐字节等价（信封文本、user 角色、单 text part）；**origin 元数据有差**：TS 由调用方指定（injection+variant / system_trigger+name 等），Rust 固定为 Injection——影响撤销/压缩按来源判定的粒度。Rust 另有 deferred 上限 `max_deferred_messages`（TS 无上限，超限拒绝并告警） |
| 信封教学（system prompt） | `mirri-core` 系统提示组装 | ✅（镜像） |
| contextInjectorService（注册式 + 位置记账 + splice 对账 + isNewTurn 状态） | `mirri-core/src/turn_flow/injection/mod.rs:27-92`（`Injector` trait + `InjectionManager`） | ⚠️ 骨架等价；**无** positions 追踪/splice 对账/isNewTurn 状态——以"每步扫描当前内存上下文"替代（ADR-003 替代路线）。扫描域 = 压缩后的内存态（被压掉的消息已不在，扫描回不到压缩前；压缩保留头也在域内）。实际扫描量：goal/todo/plan 均为反向+提前退出的局部扫描（窗口≈自上一条用户消息或同类提醒以来，`goal.rs:243-258`、`todo_list.rs:57-81`、`plan_mode.rs:122-134`）；唯一正向全量 fold 是工具公告（`tools_diff.rs:70-79`，公告消息稀少） |
| capability_gap | `injection/capability_gap.rs:40-81` | ✅ |
| tools diff 公告 | `injection/tools_diff.rs:58-127` | ✅；压缩边界改为每步重 fold，无需 `needsBoundaryInjection` 状态 |
| plan_mode | `injection/plan_mode.rs:29-153` | ✅（2/5 轮阶梯同款） |
| permission_mode | `injection/permission_mode.rs:29-56` | ✅ |
| goal 每回合注入 | `injection/goal.rs:236-269` | ✅；TS 的 cancelled（`:671`）/ budget-stop（`:853`）事件提醒**已接线**（2026-09-17，commits f9106d1b/501e5c61）；fork-cleared（`:1143`）**不适用（N/A）**——TS fork 为跨会话分叉，Rust `fork_agent`（`session/agent_lifecycle.rs:97-124`）为同会话内新 agent 且 goal 是会话级单例，fork 保留 goal |
| todo_list_reminder | `injection/todo_list.rs:24-107` | ✅（双 10 轮同款） |
| background_task_status（压缩后重注） | `turn_flow/hooks.rs:536-565` + `turn_flow/mod.rs:1684-1715` | ✅（文案逐字镜像） |
| interruptionReminder | `mirri-core/src/interruption_reminder.rs`（完整移植 + 测试） | ✅ **已接线**（2026-09-17，commits 606760d6/2d12eb97/60987c07）：用户取消回合注入提醒并写入 wire op `interruptionReminder.recorded`（`mirri-proto/wire.rs:609-610`），wire restore 后按 pending 重放 |
| btw 侧问声明 | `mirri-server/rpc.rs:3644-3711`、`mirri-embedded/session_handlers.rs:4560-4580` | ✅ |
| sessionInit（/init 提醒） | — | ❌ **缺失**（Rust 无 `/init`） |
| swarm 进出提醒 | `mirri-core/src/swarm.rs`（`SwarmModeService`，完整移植） | ✅ **已接线**（2026-09-17，commits 7ac4dddd/908c539c）：生产路径改用 `SwarmModeService`（替换 `permission/mod.rs:113` 的裸 `SwarmModeState`），进出提醒 + wire op + 退出弹出收回 |
| skill 激活注入 | `mirri-embedded/session_handlers.rs:3937-3948` + SkillTool `tools/skill.rs:196-208` | ✅，但**投递通道 P1 分歧**已登记（`plan/rust-tool-calling.md:3098`：TS 规格为用户侧消息，Rust 用 system-reminder 旁注） |
| 工具去重提醒 | `turn_flow/dedup.rs:34-58,206-267,300-325` | ✅（3/5/8/12 同款） |
| 压缩 handoff / summary prefix / elision | `context_memory/mod.rs:739-795`、`compaction/mod.rs:89`、`mirri-proto/reducer.rs:975-991` | ✅；压缩后另有 `refresh_agent_system_prompt`（TS `profile.refreshSystemPrompt` 对齐） |
| undo 跳过 injection | `undo_service.rs:71-81` | ✅ |
| hook 结果/附加上下文 | `turn_flow/mod.rs:893-1021` | ✅ |
| steer 冲刷（BackgroundTask 持久化） | `turn_flow/hooks.rs:570-597,1466-1493` | ✅（SDR-B2-01） |
| 静态环境上下文（`cwd/now/cwd_listing/agents_md` 模板变量 + Ultimate Reminders） | `scope/session_scope.rs:1085-1118,1303` | ⚠️ **形态不同**：AGENTS.md + 目录树注入 system prompt 尾部（agent 创建 + 压缩后刷新），非每步尾部注入；`${os}/${shell}/${now}/${cwd}` Ultimate Reminders 环境变量为已登记缺口（`plan/rust-builtin-profiles-parity.md:72`） |
| 动态工具 schema（role=system + tools 字段） | tools_diff / tool select 协议 | ✅（progressive disclosure 同协议） |
| —（TS 无对应） | `injection/title.rs:16-49` TitleInjector（ADR-017/018 命名后熄灭） | 🔵 **Rust 独有** |
| —（TS 无对应） | 注入 wire 持久化内建不变式（`record_system_reminders`，ADR-009） | 🔵 **Rust 强化**（引擎路径内建重放等价测试） |

## 4. 文档覆盖差异矩阵（本基线建立前）

| 机制 | 本基线前的覆盖 | 现状 |
|---|---|---|
| `<system-reminder>` 信封契约 | 零规格（仅散点提及：skills-architecture.md:93、goal-context.md:75、collaboration-context.md:64） | 本域 US1 承载 |
| contextInjector 框架契约 | 零规格（approval-architecture.md:86 一处局部语义） | 本域 US2/ADR-003 承载 |
| todo_list_reminder 注入器 | **零覆盖**（与 TodoList 工具输出 write-reminder 易混淆——后者 tool-calling-builtin-tools.md:1033 已覆盖） | 本域 §1.3 承载 |
| interruptionReminder | 仅一行现状（turn-scheduling-context.md:91）+ wire op 归属表 | 本域 US3 AC1 承载 |
| sessionInit（/init） | 一句提及（agent-profile-settings-page-context.md:81） | 本域 US3 AC3 承载 |
| stop hook / hook 附加上下文产品语义 | 无归属域（turn-scheduling 表行 + 实现记录） | 本域 US5 通道侧承载；事件语义仍开放（见 ROADMAP） |
| 注入物压缩退场总规则 | 隐含于 compaction-architecture 措辞与 plan 裁决 | 本域 US6 AC1 承载 |
| 各业务变体内容语义 | 分散于 goal/approval/skills/multimodal/subagent/tool-calling/collaboration 域，质量良好 | 维持各域归属（本域边界声明） |

## 5. 遗留现状风险（非决策，供查阅）

- v2 暂存缓冲无上限（长工具交换 + 高频注入理论上可堆积）。
- 打断提醒的"末条持久消息去重"依赖消息序，极端时序（多进程/协作）下未验证。
- Rust 侧两个"已移植未接线"模块（interruption_reminder.rs、swarm.rs SwarmModeService）构成功能漂移风险——代码存在但生产路径不可达。
