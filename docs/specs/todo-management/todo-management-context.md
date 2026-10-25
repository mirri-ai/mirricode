# 任务清单域（todo）— 现状盘点（Context）

> 快照日期：2026-10-22 ·
> 文档类型：参考类（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新，不含目标、不含决策（那些见 [todo-management-requirements.md](todo-management-requirements.md) / [todo-management-architecture.md](todo-management-architecture.md)）。
> 代码事实基线：TS v2 = `packages/agent-core-v2/src/session/todo/`（权威语义源）；TS v1 = `packages/agent-core/src/tools/builtin/state/todo-list.ts`（维护冻结）；Rust = `rust/crates/mirri-core/src/`；消费端 = `apps/mirri-code`（TUI）、`apps/mirri-web`、`packages/kap-server`。

## 1. TS v2 引擎现状（权威语义源）

### 1.1 数据形状与存储

- `TodoItem` = `{ title: string, status: 'pending'|'in_progress'|'done' }`（`session/todo/todoItem.ts:13`）；`readTodoItems` 对脏数据过滤校验（`:18`）。
- 存储挂在**主 agent（`main`）的 wire 模型**上：`TodoModel = defineCheckpointedModel('todo', () => [])`（`session/todo/todoOps.ts:19`）——**undo 检查点化模型**：每次 `tools.update_store(key:'todo')` 替换 `current`；`context.append_message` 命中 undo 锚点时把当前值压栈 `checkpoints`；`context.undo` 弹栈恢复；`context.apply_compaction` / `context.clear` 清空检查点（`agent/contextMemory/conversationTime.ts:51-86`）。
- 写入路径：`SessionTodoService.setTodos` → `wire.dispatch(todoSet({key:'todo', value}))`（`session/todo/sessionTodoService.ts:93`）→ TodoModel fold → `onDidChange` emitter 广播（`:96`）。每次写入 = 主 agent wire 日志追加一条 `tools.update_store` 记录。
- 读取路径：`getTodos` = `main.accessor.get(IWireService).getModel(TodoModel).current`（`:74`）——**唯一事实源是 wire 折叠态**，facade 不自持副本（头注释 `:5-7`）。

### 1.2 undo 联动

- 主 agent 绑定 `context.undone` 订阅：undo 发生后从 TodoModel 重读当前值，与 `lastKnownTodos` 逐项比对（`todoItemsEqual` `:149`），有差异才发 `onDidChange`（`:110-115`）。**undo 的正确性由 checkpointed model 保证**：`context.undo` reducer 恢复的是 undo 锚点时刻的清单快照。

### 1.3 共享与子代理

- 会话级单例：`ISessionTodoService`（Session 作用域）；主/子 agent 共享同一清单。每个 agent（含子代理）经 `IAgentContextInjectorService` 注册 `todo_list_reminder` 注入器（`bindAgent` `:99-104`）。

### 1.4 stale reminder（提醒注入）

- 纯函数 `todoListStaleReminder`（`session/todo/todoListReminder.ts:29`）：激活前提 = 工具策略里 `TodoList` 处于 active（`sessionTodoService.ts:123` `toolPolicy.isToolActive`）；触发阈值 = 距上次**写入**（带 `todos` 数组的调用）≥10 个 assistant 回合 **且** 距上次提醒 ≥10 个回合（`todoListReminder.ts:15-16`）；查询态调用（无 `todos`）不重置写入计数（`hasTodoListWrite` `:77`）；去重靠注入 origin 变体 `todo_list_reminder` 识别旧提醒（`:91`）。
- 文案：固定提醒语 + 当前清单渲染（`:98-108`），提醒语原文见 builtin-tools §4.4 对应节。

### 1.5 工具本体（`agent/tools/todo-list/`）

- `todo-list.ts:39` 契约（zod schema，`todos` 可选数组）；`todoListTool.ts:36` 实现：读=查询渲染、空数组=清空、整体替换后回 `Todo list updated.\n{清单}\n\n{write-reminder}`。经 `registerAgentToolService` 自注册，profile 激活门控。

### 1.6 压缩后保留

- full compaction 的 summary 后处理 `postProcessSummary`：当前清单非空时以 `## TODO List` 标题追加到压缩摘要（`agent/fullCompaction/fullCompactionService.ts:774-780`）——压缩抹掉历史后，清单仍随摘要保留在模型上下文里。

### 1.7 resume 恢复

- wire 重放天然恢复：resume 折叠 wire 日志 → TodoModel 恢复 `current`。SDK 的 `toolStore` 折叠（`packages/node-sdk/src/v2/resume-replay.ts:80` `agent.tools.storeData()`）即从该模型读取，随 `session.resume` 响应返回给消费端。

## 2. Rust 引擎现状

### 2.1 数据形状与工具本体（与 TS 基本对齐）

- `crates/mirri-core/src/todo.rs`：`TodoStatus` 枚举（serde snake_case；`FromStr` 大小写不敏感——比 TS zod 严格枚举宽松）、`TodoItem`、`TodoList = Mutex<Vec<TodoItem>>`（`set`/`get`/`clear`，`:82-118`）、`render_todo_list`。
- 工具 `crates/mirri-core/src/tools/todo_list.rs:23`：`TodoListTool` 持 `Arc<TodoList>`，三形态语义 / 输出文案 / write-reminder 与 TS 一致（`:139-151`）。
- 作用域：`SessionScope.todo_list`（`scope/session_scope.rs:647`）会话级单 `Arc`，主/子共享（`agent_scope.rs:79`）；profile 白名单含 TodoList（`profile.rs:2810,2847`），explore/plan profile 不含（`:2859`）。

### 2.2 网页端的派生路径（两引擎不同）

- TS v2 会话：冷快照 `readColdSnapshot` 折 `tools.update_store` 记录恢复清单（`packages/kap-server/src/services/transcript/transcriptService.ts:584`，读 `agents/main/wire.jsonl`）。
- rust 会话：web 后端（`rust/crates/mirri-server/src/http.rs`）**无 transcript 冷路径**；面板由 `apps/mirri-web/src/composables/latestTodos.ts` 从消息历史里最后一条带 `todos` 数组的工具调用派生（`useMirriWebClient.ts:2153` 消费）。**压缩后调用明细被折叠出历史 → 派生返回空，面板变空**——独立于 G3 的第二压缩缺口。

### 2.3 resume 响应的诚实缺口

- rust 引擎的 resume 响应组装（`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:828` 起）明确把 `toolStore` 声明为诚实缺口而整体省略（"no client-side tool store exists"）——终端恢复渲染器读取的 `toolStore['todo']` 恒缺。

### 2.4 痛点清单（现状缺口，按消费链排列）

| # | 缺口 | 事实锚点 | 用户可见后果 |
|---|---|---|---|
| G1 | **写入不落 wire 记录**：工具只调 `todo_list.set()`，全 rust crates 无 `WireOp::ToolsUpdateStore` 构造点（仅 `mirri-proto/src/wire.rs:442` 枚举定义 + `journal_route.rs:95` 路由匹配） | `tools/todo_list.rs:139` | rust 会话的清单对持久化体系不可见 |
| G2 | **resume 不恢复**：proto `ToolsState`（`mirri-proto/src/state.rs:333`）只有 `active`/`user_registrations`/`disabled_user_tools`，无 tool store 字段；reducer 无 `ToolsUpdateStore` 折叠分支；`restore_session_scoped_state`（`session/manager/restore.rs:1147`）恢复 goal/plan/cron 但跳过 todo；resume 响应省略 `toolStore`（§2.3） | `restore.rs:1127-1261` | TUI resume 从 `agent.toolStore?.['todo']` 取值（`apps/mirri-code/src/tui/controllers/session-replay.ts:118`）恒为空 → **面板清空**；web cold snapshot 折 `tools.update_store`（`packages/kap-server/src/services/transcript/transcriptService.ts:584`）同样拿不到 |
| G3 | **压缩摘要丢清单**：`post_process_summary` 接缝存在但被硬编码 `None`（`compaction/round.rs:421` 注释自认 "TODOs are not currently available in the compaction path"） | `round.rs:422` | 长会话压缩后清单从模型上下文消失，模型"忘记"进度 |
| G4 | **undo 不联动**：rust 有 `ContextUndo` wire op（`mirri-proto/src/wire.rs:188`）与 `ConversationUndoService`（`undo_service.rs`），但内存 `Arc<TodoList>` 无检查点、无重同步 | `undo_service.rs`（全文无 todo 处理） | undo 后清单与对话历史错位（回到旧清单对应的答复，清单却停在后续状态） |
| G5 | **主 agent 无 stale reminder**：`set_todo_list` 仅在 subagent host 两处调用（`subagent/host.rs:1080,1756`）；嵌入式主 agent turn flow（`mirri-embedded/src/embedded_server/mod.rs:4083` `get_or_create_turn_flow` + `message_handlers.rs:308-330` 组装序列）从未接线——`todo_injector` 恒 `None` | `turn_flow/mod.rs:414,751,1642` | rust 主 agent 超过 10 回合不更新清单**不会收到任何提醒**（TS 会）；子代理反而有——注入面倒挂 |
| G6 | **提醒注入无激活门控**：rust `TodoListReminderInjector::inject`（`turn_flow/injection/todo_list.rs:49`）直接计数注入，无 TS `isToolActive` 等价检查 | `injection/todo_list.rs:49` | profile/策略禁用 TodoList 时仍可能注入提醒（仅子代理路径受 G5 影响） |
| G7 | **rust 会话网页面板压缩后变空**：面板从消息历史派生（§2.2），压缩折叠掉清单写入的调用明细后派生源为空；即便引擎落了记录（G1 修复），派生路径也不读记录 | `apps/mirri-web/src/composables/latestTodos.ts` | rust 会话压缩后网页清单面板消失，直到下次写入 |

### 2.5 运行中的 UI 刷新（两边一致，非缺口）

- TUI：从 `tool.call` 事件匹配 `TodoList` 调用、解析 args 刷新面板（`apps/mirri-code/src/tui/controllers/session-event-handler.ts:612`）。
- web：transcript 投影把确认的 TodoList 写入转为 `todo.upsert`（`packages/kap-server/src/services/transcript/coreEventMap.ts:782`）；冷启动另从 wire 记录折 `readColdSnapshot`（`transcriptService.ts:584`）。
- 事件面差异（TS `onDidChange` vs rust 无）不影响**运行中**刷新，只影响恢复路径（G2）。

## 3. TS v1（维护冻结，仅存档）

- `packages/agent-core/src/tools/builtin/state/todo-list.ts`：agent 级 `ToolStore`（非会话级），`TODO_STORE_KEY='todo'`，写经 `tools.update_store`（v1 store 语义）。提醒注入器 `agent/injection/todo-list.ts` 与 v2 逻辑同源。冻结不改，仅作行为参照。

## 4. 工具描述契约基线

面向模型的描述、参数契约、输出契约、错误面的**契约原文**已在 tool-calling 域登记：[tool-calling-builtin-tools.md](../tool-calling-builtin-tools.md) §4.4（TodoList 节）与 §5 备忘第 7 条（"TodoList 写入走共享存储（key `'todo'`），UI 面板与重放均从该 store 水合"）。本域不重复照录；两侧差异裁决见本域 requirements/architecture。
