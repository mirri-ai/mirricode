# 中断任务恢复（interrupted-task-recovery）现状盘点

> **快照日期：2026-10-28**（建域当日首录）。本文是中断任务恢复域的代码事实清单，随现状变化更新；不含目标与裁决（设计文档单向引用本文）。
> 文档基线 = [interrupted-task-recovery-requirements.md](interrupted-task-recovery-requirements.md) + [interrupted-task-recovery-architecture.md](interrupted-task-recovery-architecture.md)。
> 所有行号基于 2026-10-28 的工作区；引用前用工具复核。

## 1. 会话与回合的持久化现状（我们能知道什么）

### 1.1 存储布局

| 事实 | 位置 |
|---|---|
| 会话目录三件套：`state.json`（元数据）+ `wire.jsonl`（追加型事实日志）+ `snapshot.json`（可选回放检查点） | `rust/crates/mirri-core/src/session/persistence.rs:1-39`（模块文档） |
| 会话根 = `<mirri-home>/v3_sessions`；workspace 桶只在有 key 时存在（无 key 为平铺老形态） | `rust/crates/mirri-cli/src/main.rs:194`；`persistence.rs:23-28` |
| `state.json` 的元数据字段（`PersistedSessionMetadata`）：version/title/created_at/updated_at/tags/archived/custom_title/last_prompt/work_dir/forked_from/last_used_at/archived_at/compressed 等——**没有任何回合状态字段** | `rust/crates/mirri-core/src/session/persistence.rs:73-179` |
| 子代理日志是第四形态：`<session_dir>/agents/<agent_id>/wire.jsonl` | `rust/crates/mirri-core/src/session/persistence.rs:447-454` |
| SQLite（`<home>/session-index.db`）只是列表读模型，行结构与 state.json 同族，同样无回合状态列 | `rust/crates/mirri-session-index/src/lib.rs:1-7, 86-110` |

### 1.2 回合的落盘词表（wire op）

| 事实 | 位置 |
|---|---|
| 回合相关 op 恰为五个：`turn.prompt`、`turn.steer`、`turn.steer_flushed`、`turn.cancel`、`turn.ended`——**没有 `turn.started` 这个 op** | `rust/crates/mirri-proto/src/wire.rs:195-241` |
| `turn.ended` 载荷是自由字符串 `reason: String`（+ 可选 `error` / `durationMs`），**不是线级枚举**；读侧由 `outcome_of()` 把字符串归并为四值枚举（completed/cancelled/stalled→其余归 failed；`abort.*` 错误码归 cancelled） | `rust/crates/mirri-proto/src/wire.rs:229-241`；`rust/crates/mirri-core/src/session/activity_shadow.rs:181-201` |
| 注意：`event.turn.started` 作为投影**事件**存在（`turn.started` 引擎事件），但不是持久化 wire op——「没有 turn.started」的表述限指 wire op 层 | `rust/crates/mirri-core/tests/session_events_tests.rs:2081`；ACP 方言 `rust/crates/mirri-acp/src/acp_handler.rs:2150` |
| 每条 JSONL 记录带持久化层注入的 `time` 戳 | `rust/crates/mirri-core/src/session/persistence.rs` + `mirri_persistence::append_log` |

### 1.3 「未收尾回合」的磁盘可判定性

| 事实 | 位置 |
|---|---|
| `SessionManager::read_only_open_turn_started_at`：返回「最后一个 `turn.prompt` 后没有 `turn.ended`」的时间戳（只读，走迁移前 JSON 记录以保留 `time`） | `rust/crates/mirri-core/src/session/manager/restore.rs:598-612` |
| 该函数当前唯一生产调用方：`session.snapshot` 的 `current_turn_started_at` 字段（serve/web 路径），做前端「Working for X」计时锚点；嵌入式列表路径不调用它 | `rust/crates/mirri-server/src/rpc.rs:2403-2419` |
| 前端消费 `current_turn_started_at` 字段（非函数） | `apps/mirri-web/src/api/daemon/ws-rpc-api.ts:3578` |

## 2. 重启后的可见性缺口（我们为什么看不出来）

### 2.1 内存态与会话态

| 事实 | 位置 |
|---|---|
| 流式累积器（`in_flight_shadow`）明确是易失的：流式增量从不落盘，重启即空 | `rust/crates/mirri-core/src/session/in_flight_shadow.rs:1-19` |
| 活动视图（`activity_shadow`）只从**当次进程**的事件总线折叠；进程重启后 bus 全新，`SessionActivityState` 从 `default()` 起——重启后所有会话一律 `busy=false`、`last_turn_reason=None` | `rust/crates/mirri-core/src/session/activity_shadow.rs:42-51, 66-74` |
| 回合结局枚举四值：`Completed / Cancelled / Failed / Stalled`——**没有 `Interrupted`**（无「服务死亡导致的中断」语义） | `rust/crates/mirri-core/src/session_activity.rs:36-48` |
| 会话恢复（`session.resume`）只重放历史：悬空的 `turn.prompt`（无 `turn.ended` 收尾）在恢复时是**静默空操作**——不重开回合、不置活动态、不产生任何信号 | `rust/crates/mirri-core/src/session/manager/restore.rs:1-6, 92-209`；`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1103+` |
| 用户主动取消的中断痕迹是持久化的：`turn.cancel` + `interruptionReminder.recorded` 在取消当场写入 wire 日志，恢复时重放注入上下文（给模型看，非 UI 状态） | `rust/crates/mirri-core/src/turn_flow/mod.rs:1613-1661`；`rust/crates/mirri-core/src/interruption_reminder.rs:1-13`；测试 `rust/crates/mirri-core/tests/restore_interruption_tests.rs:171-255` |
| **等待用户决策的挂起队列是进程内存态**：审批/提问 gates 的待决登记在内存 Map，会话恢复（resume）重建时是全新空 gate，上次的挂起等待不重建（等待中的提问/审批在服务退出后失效，仅提问内容留在对话历史里可读） | `rust/crates/mirri-core/src/session/gates.rs:233, 371-402`（pending 内存 Map）；`rust/crates/mirri-core/src/session/manager/restore.rs:280-293`（恢复重建 fresh gates） |

### 2.2 读面（列表与界面）

| 事实 | 位置 |
|---|---|
| `SessionView`（`session.list` 行结构）含 `busy / main_turn_active / pending_interaction / last_turn_reason`——字段齐全，但重启后来源归零（§2.1） | `rust/crates/mirri-embedded/src/embedded_server/mod.rs:612-642` |
| 列表读面构建：`dispatch_list_sessions`（每行 live 视图或持久化条目视图） | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:689, 833-844` |
| Web 会话行徽标机制已有两枚：`cancelled/failed` → 「已中断/Stopped」徽标，`stalled` → 「已停滞」徽标；guard 含 `!busy && 无待决问答/审批` | `apps/mirri-web/src/components/SessionRow.vue:194-213`（`lastTurnReason` 计算 :53） |
| 徽标 i18n：en `aborted='Stopped'` / `stalled='Stalled'`；zh `aborted='已中断'` / `stalled='已停滞'`（注意：zh 侧「已中断」词汇已被用户取消/失败占用，本域新增语义需另取名，见需求文档术语表） | `apps/mirri-web/src/i18n/locales/en/workspace.ts:69-76`；`apps/mirri-web/src/i18n/locales/zh/workspace.ts:67-74` |
| 冷启动进入空白新建任务草稿页，历史仅在后台填侧边栏；无「上次会话」记忆（仅 URL 深链例外） | `apps/mirri-web/src/composables/client/useWorkspaceState.ts:1130-1153` |
| 桌面壳每次启动都加载根 URL，只持久化窗口尺寸；`desktop-state.json` 只记 `{pid, port, startedAt}` 供陈旧守护进程清理，非 UI 状态 | `apps/mirri-desktop/src/main/window/create.ts:57-82`；`apps/mirri-desktop/src/main/server/state.ts:12, 43, 61` |
| desktop-lite：独立端口 55828 + 独立状态目录，但**复用同一 rust 引擎二进制与同一 `~/.mirri-code/v3_sessions`**（无自有会话存储） | `apps/mirri-desktop-lite/src/bun/daemon.ts:10-13, 286-290` |

## 3. 服务生命周期现状（退出时发生什么）

| 事实 | 位置 |
|---|---|
| 优雅关闭通道只有两条：Ctrl+C（`tokio::signal::ctrl_c`）与 HTTP `POST /api/v1/shutdown`；**无 SIGTERM 处理**（grep 仅命中进程杀灭与测试） | `rust/crates/mirri-cli/src/main.rs:422-431`；`rust/crates/mirri-server/src/http.rs:115, 334-345` |
| 关闭状态机：`request_shutdown`（watch channel）→ `drain_engine_calls`（有界等待在途引擎调用，超时 aborting）→ 服务器返回 | `rust/crates/mirri-server/src/state.rs:432-461`；接线 `rust/crates/mirri-server/src/lib.rs:86`、`http.rs:154` |
| **关闭链路不写任何文件**——无退出时间戳、无在跑清单、无生命周期哨兵；全仓无 heartbeat/sentinel 生命周期文件（仅任务级内存心跳与进程指纹） | `rust/crates/mirri-server/src/state.rs:432-461`（链路核查）；最近的磁盘物是 `desktop-state.json`（pid/port/startedAt，Electron 侧） |
| 唯一的「关闭时落盘」在 `session.close`（快照写出），与进程退出无关 | `rust/crates/mirri-core/src/session/manager/mod.rs:765-799`；`session_handlers.rs:1673-1680` |

## 4. 后台任务与崩溃善后现状

| 事实 | 位置 |
|---|---|
| 启动孤儿清扫：首个 RPC 前一次性扫全部工作区×会话的任务归档，裁决 `Running` 的进程类任务死活；报告写 `<sessions_root>/orphan-sweep/<ts>.json`；另有常驻 60s 巡检 | `rust/crates/mirri-embedded/src/embedded_server/orphan_governance.rs:1-45`；`rust/crates/mirri-core/src/orphan_sweep.rs:1-40`（写报 :590-610） |
| 默认策略 `ReportOnly` | `rust/crates/mirri-core/src/config/mod.rs:405, 1021-1025` |
| **孤儿报告无任何消费方**——无 RPC 暴露、无 UI 读取（仅测试缝） | 全仓 grep `orphan-sweep`（`apps/mirri-web` 零命中） |
| 任务重载对账：无指纹的 `Running` 任务改写为 `Stopped`（"lost"，日志 `reconciling lost running task as stopped after restart`）；有指纹的交给孤儿清扫 | `rust/crates/mirri-core/src/task.rs:656-712`（谓词 :686-687，写回 :692-699） |
| 定时任务跨重启恢复（wire 重放） | `rust/crates/mirri-core/src/cron.rs:593-660` |
| goal 异常结束置 `Paused`（事后收敛，非重启续跑） | `rust/crates/mirri-core/src/goal.rs:585` |

## 5. 「继续」能力现状

| 事实 | 位置 |
|---|---|
| **不存在续跑回合机制**：注册表仅 `session.resume`（会话重建）/ `session.reload` / `goal.resume`（goal 域）；无 `turn.resume` / `turn.continue` / `turn.retry` 的 wire op 或 RPC | `rust/crates/mirri-proto/src/rpc/registry.rs:394-395, 482`；`rust/crates/mirri-proto/src/wire.rs`（词表核查） |
| CLI 子命令：`web/serve/acp/print/migrate/session-list/config-show/doctor`，无恢复类命令 | `rust/crates/mirri-cli/src/main.rs:90-117` |
| `mirri session-list` 仅输出会话 ID + 创建时间（workspace 参数被忽略） | `rust/crates/mirri-cli/src/commands.rs:44-89` |
| 通知通道现状：事件经会话级 `EventBus` → `event_projection` → WS `event.*` 帧（持久、带 seq）；**无全局/服务器级通知通道**——无会话可挂靠的启动通知没有现成落点 | `rust/crates/mirri-core/src/session/event_projection.rs:977-990`；`rust/crates/mirri-server/src/ws.rs:83-89, 260, 342` |
| 规格层已知缺口：「进程死亡即清零」在会话管理域 context §1.8 已记录为已知问题 | `docs/specs/session-management-context.md` §1.8 |

## 6. 差距定性（现状 vs 本域需求）

| 能力 | 现状 | 差距 |
|---|---|---|
| 退出方式区分（优雅/异常） | 关闭链路不写任何文件；无哨兵/心跳 | 全缺（哨兵文件机制） |
| 退出瞬间在跑清单 | 正常退出不记录；异常退出后可由「未收尾回合」磁盘反推，但无消费者 | 全缺（优雅退出记录 + 反推路径） |
| 「中断」语义 | 回合结局枚举无 `interrupted`；重启后 `last_turn_reason` 恒为 `None` | 全缺（语义新增） |
| 启动通知 | 无全局通知通道；无任何启动检查提示 | 全缺 |
| 会话树中断徽标 | 徽标机制存在（停止/停滞），但重启后永不点亮 | 缺数据源与状态回填 |
| 就地恢复操作 | 无任何恢复入口（面板/按钮/菜单皆无） | 全缺 |
| 续跑机制 | 无 `turn.resume` 类 op；`session.resume` 仅重放 | 全缺（续跑发起新回合） |
| 批量恢复 | 无 | 全缺 |

## 7. 既有裁决/相邻域约束（本域设计不可违反）

| 约束 | 出处 |
|---|---|
| 回合结局词汇族已跨域登记（SDR-006 本域批次扩充 interrupted 成因值），用户取消 vs 系统停滞 vs 服务退出的词汇分立是既有纪律；本域新增「运行中断」语义不得复用 `cancelled` | `docs/specs/turn-scheduling-requirements.md` SDR-003 原则二 / SDR-006；glossary「回合结局」（L3） |
| 「会话恢复（S1）」= 进程重启后的会话重建（重放），与子代理「续作（A6）」词汇已拆分；本域的「继续任务」是**新语义**（续跑），须避免与两者混淆 | glossary S1 / A6 |
| 单活动回合：同一代理同一时刻至多一个运行中回合；新回合请求排队 | `docs/specs/turn-scheduling-requirements.md` US1.1 |
| 异步唤醒源单一插座：任何「晚到的消息进对话」走既有准入类别，不发明旁路调度——续跑发起属于用户消息类准入 | `docs/specs/turn-scheduling-requirements.md` E4 |
| 跨域提案 P-028（恢复会话时未消费任务结果的主动提示）与本域同族，落点在 turn-scheduling 提案登记表，实施时需对齐 | `docs/specs/proposals.md` P-028 |
| 引擎目标门：一切引擎行为变更落 rust core（TS 引擎已退役） | 根 `AGENTS.md`「Engine-target gate」 |
| 原子写基建（0600 临时文件 + rename）与私目录基建已有，哨兵文件应复用 | `rust/crates/mirri-persistence`（`write_file_atomically`） |
