# Rust Core 现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-04（§6 追加：2026-09-11；§7 追加：2026-09-13、重写：2026-10-02；§8 追加：2026-09-14；§10 追加：2026-10-28） ·
> 配套设计文档：[00-overview.md](00-overview.md) · [01-protocol-unification.md](01-protocol-unification.md) · [05-crate-structure.md](05-crate-structure.md) · [06-cross-platform.md](06-cross-platform.md) · [08-multi-workspace.md](08-multi-workspace.md) · [09-session-title-generation.md](09-session-title-generation.md) · [10-agent-context-assembly.md](10-agent-context-assembly.md) · [11-model-resolution.md](11-model-resolution.md) · [14-acp-desktop-runtime-sharing.md](14-acp-desktop-runtime-sharing.md) ·
> 本文不描述目标、不含裁决——终态与裁决在设计文档；执行状态在 [ROADMAP.md](ROADMAP.md)。

## 1. 总体架构现状（对应 00 §1）

Mirri Code 当前以 TypeScript 引擎（`agent-core-v2` + `kap-server`）为运行时核心，三端各自通过不同方式对接：

| 前端 | 传输 | 引擎 |
|------|------|------|
| TUI (`apps/mirri-code`) | in-process 内存 RPC (`createRPC()`) | TS `agent-core-v2` via `SDKRpcClientV2` |
| Web (`apps/mirri-web`) | REST + WebSocket (`/api/v1/*`, `/ws`) | TS `kap-server` → `agent-core-v2` |
| ACP (`packages/acp-server`) | stdio JSON-RPC 2.0 | TS `agent-core-v2` via `Klient` facade |

Rust 核心（`rust/crates/`，17 个 crate）具备完整的领域功能（Session/Agent/Tool/Permission/Hook/Skill/Plan/Goal/MCP/Persistence）与三端接入交付物（`mirri-node` NAPI、`mirri-acp` stdio binary、`SDKRpcClientRust`）。方法目录与契约 pin 以 mirri-proto 注册表（`rust/crates/mirri-proto/src/rpc/registry.rs`）为事实源；`mirri-server/src/ws_rpc.rs` 承载统一 WS `/api/v1/rpc`（JSON-RPC 2.0），serve 面由注册表驱动。

引擎开关：`MIRRICODE_RUST_ENGINE`（`apps/mirri-code/src/cli/experimental-v2.ts`），优先级 rust > v2（默认）> v1。

## 2. 协议现状（对应 01 §1.1，统一协议设计启动前）

Rust core（`rust/crates/mirri-server`）对外暴露 3 种传输，消息格式与能力分布互不对等：

| 传输 | 端点 | 消息格式 | 覆盖能力 |
|------|------|---------|---------|
| HTTP REST | `/api/v1/*` | `{code, msg, data, request_id}` 信封，恒为 HTTP 200 | 16 个方法映射为 REST 资源；approval/question 走独立 HTTP 端点 |
| WebSocket | `/api/v1/ws` | 自定义帧 `{type: ...}`（`server_hello`/`client_hello`/`subscribe`/`session_event`/`ping`/`pong`/`error`） | 只做事件推送与控制；**不能发起 RPC** |
| Unix socket IPC | 行分隔 JSON-RPC 2.0 | `{jsonrpc, method, params, id}` | 16 个方法全目录；**无事件推送** |

问题清单：

1. **协议碎片化** — 3 种传输 × 2 种消息格式 × 不对等的能力分布
2. **双连接依赖** — Web 客户端必须同时维护 HTTP + WS 两条连接，自行做跨通道关联
3. **`message.send` 同步阻塞** — 直到整个 turn 结束才返回（最长 300s，`rpc.rs` `ENGINE_TURN_TIMEOUT`）
4. **审批/提问通道割裂** — `approval.resolve` / `question.answer` 走独立 HTTP POST
5. **IPC 格式孤立** — 同一注册表（`RpcRegistry`）被三种包装各包一次

## 3. Crate 结构现状（对应 05 §1.6）

当前 `rust/crates/` 有 **17 个 crate**（核心依赖图 13 个 + 外围 4 个）。与原设计的关键差异：

| crate | 目标归属 | 动作 / 现实状态（2026-09-04） |
|-----------|---------|------|
| `mirri-os` | OS 抽象层（曾规划为 platform 名，同名 crate 从未存在） | trait 化：`fs` / `glob` / `process` / `pty` / `search` / `path_access`（feature-gated，依 `nix` 0.29 / `portable-pty` 0.9）；`path_codec` / `shell` / `fs_watcher` / `ipc` 等规划模块待实施 |
| `mirri-kosong` | LLM provider | 保留独立；LLM provider 装配于 `mirri-embedded/src/providers/` |
| `mirri-providers` | （曾规划的薄桥 crate） | **无此 crate**；provider 装配位于 `mirri-embedded/src/providers/`（anthropic / openai / openai_responses / web） |
| `mirri-collaboration` | 多 agent 协作 | **原计划「并入 `mirri-core::swarm` / `subagent`」未执行**，仍为独立 crate；是否并入待 C6/N2 决策 |
| `mirri-plugin-host` | 插件生命周期 | **原计划「并入 `mirri-core::hook`」未执行**，仍为独立 crate；是否并入待 C6/N2 决策 |
| `mirri-embedded` | 嵌入式引擎适配（EngineAdapter 实现） | `src/embedded_server/`（EmbeddedEngine 面），`mirri-acp` / `mirri-node` / `mirri-cli` / `mirri-test-harness` 消费 |
| `mirri-remote` | remote 帧 / transport 适配（EngineAdapter 实现） | `src/frames.rs`（RemoteEngine / 帧适配），依赖 `mirri-transport` / `mirri-proto` |
| `mirri-auth` | OAuth Device Code（RFC 8628） | 独立轻量 crate（`mirri-cli login` / `mirri-server` auth middleware 消费）；不进核心依赖图 |

**mirri-core 零 IO 违反现状（快照 2026-09-07）**：`mirri-core/src/` 非测试代码现存约 **183 处 `std::fs` 调用、分布约 20 个模块**（工具面 read/write/edit/grep/glob/read_media/path_security/skill，状态面 profile/config/task_persist/task/workspace_scope，基建面 dir_tree/blob/image_pipeline/session_export/terminal_pool/tool_result_truncation）。05 §1.2 迁移清单零执行；裁决 Z1 已定向 trait 化收敛（最低优先级），迁移前由 `rust/AGENTS.md` 契约 6 冻结令约束不恶化（P6 起新代码已不再直接新增）。

## 4. 跨平台模块现状（对应 06 §1.4）

`mirri-os` 已具备部分平台抽象（`fs` / `glob` / `process` / `pty` / `search` / `path_access`）。规划模块的现实状态：

| 模块 | 文件 | 现实状态（2026-09-04） |
|------|------|------------------------|
| 路径编解码 | `src/path_codec.rs` | 待实施（mirri-os 中不存在） |
| Shell 抽象 | `src/shell.rs` | 待实施 |
| 文件监听 | `src/fs_watcher.rs` | 待实施 |
| PTY 抽象 | `src/pty.rs` | **成立**：现有 `pty` 模块已具备底层能力，收敛为 `PtyProvider` trait 即可 |
| IPC 传输 | `src/ipc.rs` | 部分成立：mirri-os 内待实施；**`mirri-server/src/ipc.rs` 已存在**（服务端监听面） |
| 文本行尾 | `src/line_ending.rs` | 待实施 |
| 文件权限 | `src/permission.rs` | 待实施 |
| 环境变量 | `src/env.rs` | 待实施 |
| 平台分发器 | `src/platform/mod.rs` | 待实施 |

## 5. 统一协议实现锚点（对应 01 附录 A）

| 统一协议元素 | 现有代码锚点 |
|-------------|-------------|
| `server_hello` / `client_hello` | `rust/crates/mirri-server/src/ws.rs`（现为 `{serverId, version}` / `{clientId}`）；kap-server `server_hello {ws_connection_id, protocol_version, max_event_buffer_size, capabilities}` |
| `-32700/-32600/-32601/-32602/-32603/-32001/-32002` | `rust/crates/mirri-server/src/rpc.rs` `RpcError::json_rpc_code` |
| `{code, msg, data, request_id}` 信封（迁移期） | `rust/crates/mirri-server/src/rpc.rs` `ApiResponse`/`ApiErrorResponse` |
| `data.errorId` / `data.domainCode` | `rust/crates/mirri-server/src/rpc.rs`（现有实现字段为 `error_id`/`domain_code`，统一协议迁移时改为 camelCase） |
| `session.create/list/get/close/resume/fork/export`、`message.send`、`approval.resolve`、`question.answer`、`tool.list/call`、`agent.list/get`、`workspace.list`、`config.get` | `rust/crates/mirri-server/src/rpc.rs` 16 个 handler |
| 旧 WS 控制帧（subscribe/unsubscribe/interrupt/ping/session_event） | `rust/crates/mirri-server/src/ws.rs` |
| durable 事件源（`AgentCreated`/`SessionClosed`/`ToolApprovalRequested`/`QuestionAsked`/`MessageReceived`…） | `rust/crates/mirri-core/src/session/events.rs` `SessionEvent`（`EventBus`，capacity 256，Lagged = resync 信号） |
| `{seq, epoch}` cursor、`resync_required` 三 reason | `packages/kap-server/src/transport/ws/v1/protocol.ts`（cursor、`ResyncReason`）、`wsConnectionV1.ts`（serverCursors、replay） |
| 事件投影（agent.status.updated、background.task.*、config.changed 等） | `packages/kap-server/src/transport/ws/v1/events.ts`、`sessionEventBroadcaster.ts` |
| `terminalMode=engine` | `rust/crates/mirri-core/src/terminal_pool.rs` |
| goal / plan / subagent / cron / task 领域 | `rust/crates/mirri-core/src/{goal, plan, subagent, cron, task}.rs` |
| 路径校验 | `mirri_core::validate_workspace_path`（`session.export` 已使用） |
| Bearer 子协议 | `packages/kap-server/src/transport/ws/bearerProtocol.ts` |

## 6. 工作区现状（对应 08 §1，多工作区设计启动前 · 快照 2026-09-11）

**引擎运行时 = 单工作区绑定**。数据层已多工作区、进程内未多工作区：

### 6.1 单根假设的载体

| 位置 | 事实 |
|---|---|
| `rust/crates/mirri-core/src/session/manager/mod.rs:164-189` | `SessionManager` 持单 `Arc<WorkspaceScope>`（"bound to one WorkspaceScope"）；`workspace()` 访问器被 mirri-embedded 内 40+ 处调用 |
| `session/manager/mod.rs:353-355` | `session.create` 无 cwd 参数，`metadata.work_dir` 由绑定根回填 |
| `mirri-embedded/src/embedded_server/session_handlers.rs:325-358` | `session.list` 每行 cwd 强制回填绑定根 |
| `mirri-embedded/src/embedded_server/session_index.rs:349-445` | 索引 listing 只枚举自己 bucket 的 `bucket_dir` |
| `mirri-embedded/src/embedded_server/workspace_handlers.rs:85-104` | `workspace.createOrTouch` 诚实空应答（"requested root is never registered"，返回绑定工作区 view） |
| `mirri-embedded/src/embedded_server/workspace_handlers.rs:110-154` | `workspace.update/delete` `applied: false` |
| `mirri-server/src/rpc.rs:784-889` | `workspace.list/createOrTouch/update/delete` 是**传输注册表 handler，在 engine fall-through 之前拦截**——engine 侧实现正确 serve 面也不可见 |
| `mirri-embedded/src/embedded_server/mcp_config_handlers.rs:901-923` | `McpReconcileKernel.workspace_root` 单值（project 层配置锚定） |
| `mirri-cli/src/main.rs:141-201, 442-449` | `mirri serve` 单 `workspace_root` 参数启动（:162 canonicalize） |
| `rust/crates/mirri-core/src/scope/config.rs` | 信任只有静态 `WorkspaceTrustLevel`（配置值，非 per-workspace 服务） |
| `mirri-embedded/src/embedded_server/session_handlers.rs:1769-1777` | `session.search` 单根：扫描单 manager，"every hit reports the bound workspace id" |
| `mirri-server/src/ws_rpc.rs:858-872` | `session.subscribe` 经 `state.session_manager` 单绑定解析（事件订阅面） |

### 6.2 已具备的多工作区地基

| 位置 | 事实 |
|---|---|
| `mirri-core/src/scope/app_scope.rs:25-91` | `AppScope` 多工作区 CRUD（HashMap + create/get/remove/list）**已存在但仅测试使用**，无生产组合根 |
| `mirri-session-index/src/lib.rs:66-89` | sqlite schema 本身多工作区（`workspaces` 表 + session 行 `workspace_id` + `(workspace_id, updated_at)` 索引）；`list/count_active` 支持多 workspace 过滤（273-336） |
| `mirri-session-index/src/registry.rs` | 跨引擎 `workspaces.json` 注册表：读容错、写保留 TS tombstone、db 打开 seed + upsert 写穿 |
| `mirri-core/src/session/persistence.rs:511-579` | 会话持久化按 workdir 分桶（`wd_<slug>_<hash12>`，`workdir_key.rs:41`），**读取侧已跨桶扫描**；写侧落 manager 自己的 bucket |
| `mirri-node/src/engine_bridge.rs:97-115` | 进程级 `shared_user_mcp_pool_for_home`（N-workspace 复用先例，文档已预留） |
| `mirri-session-index/src/lib.rs:140-150` | `ListOpts` 仅 `limit`/`offset`——**无 id 游标**（TS 的 `before_id`/`after_id` 需索引层实现）；`workspace_ids` 过滤已支持 |
| `mirri-session-index/src/registry.rs:108-149` | 注册表写侧只会**保留**已有 tombstone（读-改-写整文档）——无"新增 tombstone"API、读侧不过滤已删行；原子替换防损坏不防丢更新（并发方新写行可被静默丢弃） |
| `mirri-core/src/session/workdir_key.rs:16-21, 60-92` | 工作区 id 派生 = 词法解析（刻意不 canonicalize，TS parity）——同目录异拼写可注册两次；`mirri serve` 的根却做 canonicalize（`main.rs:162`） |
| `packages/agent-core-v2/src/app/workspace/fileWorkspacePersistence.ts:67-82` | **TS 侧注册表写方**：`save` 用内存内全量目录整文档替换（无跨进程锁、无合并、load 时 seed）——与 rust 写方交错时可静默丢弃对方新写入的行（补救 = 重新注册） |

### 6.3 客户端现状（web，rust 传输）

`apps/mirri-web/src/api/daemon/ws-rpc-api.ts`：`workspace.list/createOrTouch/update/delete` 调用侧已映射（1736-1758，camelCase wire）；`createSession` **主动丢弃** `cwd/workspaceId`（510-528，注释声明后端不识别）；`listSessions` 不发送任何过滤参数（460-474，注释声明后端忽略）。选工作区的前端流程本身是客户端状态切换，后端语义到位后客户端零结构性改动。

### 6.4 会话寻址派发面现状（评审补充，快照 2026-09-11）

按 session-id 寻址的 RPC 现状**全部经单绑定 manager 解析**，是"只路由 create/list 两个入口"方案的盲区所在：

- **embedded 侧**：`embedded_server` 各 handler（session.get/close/resume/fork/archive/export、message.send/steer、审批、提问、compaction、cron、事件中继 `event_relay.rs:80`、会话级 MCP bridge）经 `self.manager` 寻址——分布 `session_handlers.rs`（19 处）、`message_handlers.rs`（4 处）、`interaction_handlers.rs`（4 处）、`mcp_config_handlers.rs`（5 处）、compaction/env/cron/event_relay/mcp_bridge/mod 各 1-6 处。
- **serve 传输侧**：`ServerState.session_manager`（`state.rs:36`）喂给 ~14 个传输注册表持有的会话寻址 handler（审批/提问/agent/tool/文件/git/btw，`rpc.rs:3246-3277` 一带）；restore-on-demand 钩子 `live_or_restored_session`（`rpc.rs:479-492`）绑定单 manager；`session.subscribe`（`ws_rpc.rs:858-872`）事件订阅经单 manager 解析；fire 模式状态派生 `prepare_fire_watch`（`state.rs:417-440`）同为会话寻址点（评审复评补充）。
- **后果**：只路由 `session.create`/`session.list` 时，非默认工作区创建的会话在 message.send / 事件订阅 / 审批 / resume 等派发面不可见——多工作区方案必须包含统一的 session-id → manager 解析层。

## 7. 会话标题生成现状（对应 09 §1，快照 2026-10-02）

- **机制（旁路调用，ADR-020）**：主 agent 用户轮次的三个成功 settle 站点（wait / fire / steer，`message_handlers.rs`）触发一次旁路 LLM 调用（编排层 `embedded_server/side_title.rs`，`tokio::spawn` 后台执行；纯函数层 `mirri-core/src/session/title_llm.rs`）：与主循环同一 `ConfigResolvingLlm` 构造 + `ContextMemory` normal 投影 + 同一 media pipeline + `loop_tools()`（无 image_in/video_in 能力时剔除 `ReadMediaFile`），末尾追加 `TITLE_GENERATION_INSTRUCTION` 指令消息；30s 超时、全新 `CancelToken`、不进 in-flight 注册表、不可取消、不记 wire log；usage 三桶经 `tracing::info!`（"side title call usage"）输出。被取消（Aborted）轮次仍走成功 settle 路径，照常触发；错误 settle / 硬中止 settle / no-provider 派发分支直接走规则式兜底。
- **写入路径**：成功 → `SessionMetaHandle::set_auto_title`（守卫：`is_custom_title == false` 且标题仍为占位；共享脱敏 + 200 字符截断，与规则式同一实现）→ `SessionEvent::SessionMetaUpdated` → 二次 `note_turn_settled` 写穿 `state.json` + 索引（settle 的写通先于标题落地）。`set_auto_title` 返回 `None` 时区分：并发已命名（保持 claim、静默）vs 清洗后为空（释放 claim + 规则式兜底，与失败臂同形）。
- **并发**：`side_title_claimed` 进程内原子一次性 claim（CAS，不持久化——重启后未命名会话在下一用户轮次 settle 重试）；失败 / 超时 / 清洗为空经 `release_side_title_generation()` 复位；用户在途改名经 `set_auto_title` 守卫原子胜出。
- **规则式兜底**：条件与实现不变——`is_custom_title == false` 且标题为占位（空 / `"Untitled Session"` / `"New Session"`）时，用户消息文本经脱敏（私钥块 / bearer / `key=value` 密钥赋值 / `sk-` token / 长 token 正则替换为 `[redacted]`）与空白折叠后截断 200 字符作为标题；清洗后为空（如纯图片消息）返回 None、保持未命名。
- **手动改名**：`session.rename` 校验非空且 ≤200 字符，写入标题并置 `is_custom_title = true`，发布同一事件；fork / create 等路径经入参标题各自维护 `is_custom_title`；自动写入不置位。
- **元数据**：`is_custom_title: bool` 为会话元数据既有字段。
- **主循环零痕迹**：`SetSessionTitle` 工具（文件 / 注册 / 目录）、`TitleInjector` + `TITLE_REMINDER`、`TurnFlow.title_injector`、挂载 / 卸载 / 清理接线、`AgentScopeConfig.title_handle`、`title_materialized` 标志已全量移除——主循环任何轮次请求不含命名相关内容。前缀同一性由自动化测试钉扎（工具定义全量同一 + 消息前缀逐字节扩展）；真实 provider 三桶实测证据尚未具备（记账见 ROADMAP）。
- **旧会话兼容**：wire log 中历史 `SetSessionTitle` tool_use / tool_result 帧按普通消息内容回放，不执行（恢复测试钉扎）。
- **宿主差异**：mirri-server 远程引擎的消息路径无自动标题逻辑（现状不变），未命名会话保持占位标题。
- **LLM 能力底座**：rust LLM 抽象层（mirri-kosong）提供流式 provider（anthropic / openai / openai_responses / google）与工厂 trait；消息处理宿主层持有可选的 LLM 工厂（agent 配置驱动），旁路调用与主循环复用同一解析。

## 8. 模型裁定现状（对应 11 §1，快照 2026-09-14）

> **后记（2026-09-15）**：除末条供给面外，本节所述缺陷已全部落地修复（spec 11 批次，21 commits `a490e09e`…`0cf27b29`；执行结果与 followup 债务见 [ROADMAP.md](ROADMAP.md) §4 模型裁定条目）。以下条目保留为修复前的现状取证。

- **内建具体模型多处落点**：未经用户配置的具体模型名硬编码存在于多处——会话装配缺省配置（`scope/config.rs:387` 的缺省 `model_id`；`scope/config.rs:42` 另有缺省 `default_model`，均为 `claude-sonnet-4-20250514`）与内建子代理类型（`profile.rs:2580-2621`：`agent` / `coder` / `plan` 三个内建 profile 的 `model` 字段为 `claude-sonnet-4-20250514`，`explore` 为 `claude-haiku-4-20250514`），共涉及**两种具体模型名、至少六处落点**。来源为早期 squash 提交 `281c34d8d`，无意图记录；TS 引擎无硬编码默认（`agentTool.ts:268` 一带解析不出即失败）。**已修复**（A7：双落点清除 + 守卫脚本扫描 358 文件零命中）。
- **子代理继承结构性失效**：三级裁定（显式指定 → 类型默认 → 派发者模型）在生产空转——`ProfileManager::resolve` 要求 profile 必带模型（`profile.rs:1249-1254`：`profile.model.is_empty()` 直接报 RequestInvalid），第三级"继承派发者"不可达；叠加内建类型自带模型，实际效果是"子代理永远用内建模型，除非显式传参"。**已修复**（A3/A6/A14：必带模型要求废除，三级链真实可达 + 派发时点钉扎）。
- **派发工具广播模型名**：Agent / AgentSwarm 工具描述把内建条目的模型名拼进说明文本（`tools/agent_tool.rs` / `tools/agent_swarm_tool.rs` 的描述拼装，测试断言 `desc.contains("claude-sonnet-4")` 可证）——LLM 据此感知并选用该模型名。**已修复**（A8：描述去硬编码，可用模型清单段与配置同源动态生成）。
- **真实会话取证**（session-0b8117a4-ccc8-4d35-981d-664d83a5eff1，2026-09-14）：主 agent 使用 `fast`（DeepSeek-V4-Flash）；主 agent 连续 4 次以 `subagent_type="agent"` 派发子代理，全部以内建模型因 `ANTHROPIC_API_KEY not set` 失败；第 5 次改用 `subagent_type="coder"` + 显式 `model="fast"` 才成功。三个用户报告缺陷（内建模型不该存在 / 主 agent 换模型子代理仍用内建 / profile defaultModel 不生效）在同一会话中全部复现。**三个缺陷全部消除**（e2e `e2e_subagent_inherits_parent_custom_model_without_explicit_model` 实证）。
- **切换入口分裂**：发消息携带模型参数是 per-prompt 一次性覆盖，不落会话日志（TS 引擎落——parity 破坏点）；恢复后模型回退，不与关闭前一致；恢复兜底使用内建模型而非配置 `default_model`。**已修复**（A1/A9/A10：send 参数并入单一切换路径 + 落日志 + 逐请求生效 + 恢复回退链对齐配置 default_model）。
- **校验口径分裂**：会话级设置只认登记表精确键；创建 / 发消息入口不校验；裸未知名滑入环境变量回退（`ProviderLlmFactory` 的 env 链）。**已修复**（A11/A15：全入口统一校验 + 配置漂移报错不回退；`mirri print` 裸名 env 全链残留记 ROADMAP followup）。
- **多 agent 会话**：模型变更记录无 agent 归属字段，多 agent 会话恢复后模型状态折叠错位。**已修复**（A17：切换记录为会话级单一状态，恢复取最后一条切换记录）。
- **用量记账**：用量按模型的键取回合装配时快照，回合中途切换后归因陈旧。**已修复**（A13：逐请求归因；ADR-007 记录在途窗口的内存/wire 细微差异为已知边界）。
- **Profile 供给面缺口**：rust 侧不扫描项目根 `.mirri-code/agents` 目录；NAPI 层无 profile 热重载（`bootstrap.rs:717-719` 自认 R3/R4 未做）——用户配置角色的发现与生效范围受限，是"类型默认"一级的真实供给短板。（spec 11 非目标，归 agent-profile 域，仍开放。）

## 9. 会话分叉管道现状（对应 12，快照 2026-10-09）

> **后记（2026-10-09，F1-F6 落地）**：本节所述差距已全部消除——`session.fork` 重构为按记录截取重写的会话级管道（六批 `5917c86d` / `586bd6c8` / `e5e9cf73` / `d0bc1015` / `c3ee2a3b` / F6 收口；`session/fork_pipeline.rs` 截取核心 + `SessionManager::fork_session_with_options` 管道编排）。落地要点：§9.1 的原语链全部复用（`migrate_records_in_memory` 读源 + `JsonlAppendLog` 写目标）；§9.2 的括号结构即截取边界（目标回合 `turn.ended` 含，硬末条）；§9.3 的 `forked_from`/`agents` 硬编码缺口消除（`PersistedSessionMetadata::from` 映射 + fork 铸造）；§9.4 的忙碌判定**未采用**（ADR-F4 修订版裁决锚点校验替代会话级忙碌门——`turn.ended` 是唯一可命名锚点空间，活跃回合自然不可寻址）；§9.5 的实体处置表逐项落地（成员 journal 各自括号截取 / blobs 全量 / agents manifest 携带 / tasks+snapshot 不复制）；§9.5 末条的 cron 截取复制风险由 ADR-F1 过滤表消除（三变体全剔除）；§9.6 的 cache key 配线仍缺（US5.5 引擎侧通路，归模型请求域）。验收 V1-V8 全部钉住（V7 为源/fork 各自 resume 后 context memory 序列化对拍）。以下条目保留为重构前的现状取证。

> 管道语义与需求侧的现状证据在 [../session-management-context.md](../session-management-context.md) §1.3/§1.4/§1.5a/§1.7（三引擎对照）；本节只记 Rust 侧管道重构的**机制级现状**（设计 12 的输入）。

### 9.1 wire 读取/重写原语（可复用积木）

- 读取 + 迁移：`migrate_records_in_memory(path) -> Vec<Value>`（`mirri-persistence/src/migration.rs:654`）读全记录做版本迁移；`migrated_records_to_wire_ops`（`mirri-core/src/session/manager/restore.rs:570-596`）转 `Vec<WireOp>`，容忍末行撕裂。
- 整文件原子重写先例：`migrate_log_file`（`migration.rs:559`）——读全文件 → 逐行迁移 → temp+rename。
- 追加日志：`JsonlAppendLog::create`（`append_log.rs:109`）truncate 建新文件；`open_for_append`（`append_log.rs:146`）续写。
- resume 流程（`restore.rs:92-277`）：校验 wire.jsonl 存在 → `load_state` → `create_session_with_config` → `replay_agent_state_opt`（migrate→ops→skip(base_op_index)→fold）→ `restore_subagents`（折叠 `agents/<id>/wire.jsonl`，:228）→ 重开日志。

### 9.2 回合括号结构

- `WireOp::TurnEnded { turn_id: u64, ... }`（`mirri-proto/src/wire.rs:225-235`）；turn_id 由 `TurnFlow::allocate_turn_id`（`mirri-core/src/turn_flow/mod.rs:385-389`）从 1 递增分配。
- reducer 侧 `WireOp::TurnPrompt` 使 `turn_counter += 1` 并发 `TurnStarted { turn_id }`（`mirri-proto/src/reducer.rs:246-251`）——两路一致的前提是每次 fire 先记 TurnPrompt。
- 顺序：turn 的消息记录**先于** `turn.ended`（`message_handlers.rs:656-669` vs `:797-807`，注释明确"turn.ended closes the bracket turn.prompt opened"）；已验证消费者先例 `fold_turn_usages`（`turn_usage.rs:127-130`）。

### 9.3 state.json 写入链的 forked_from 缺口

- `create_session_inner` → `persistence.save_state`（`manager/mod.rs:545`）→ `PersistedSessionMetadata::from(&SessionMetadata)`（`persistence.rs:124-148`）——其中 `forked_from: None`、`agents: None` **硬编码**（`persistence.rs:144-145`）；scope 层 `SessionMetadata` 无 forked_from 字段（`persistence.rs:150-163` 转换）。
- 读侧 `forked_from` 仅作 TS 兼容 alias 解析（`persistence.rs:116-118`）；round-trip 只靠 load-mutate-save（`persistence.rs:484-507`）保住既有值。
- fork 现状不复制 wire.jsonl：`dispatch_fork_session`（`session_handlers.rs:1519-1592`）→ `fork_session`（`manager/mod.rs:674-736`）→ 新会话 wire log 全空，仅 `fork_agent` 搬 context memory。

### 9.4 忙碌判定（护栏可直用）

- `session_is_busy(&self, session_id)`（`mirri-embedded/src/embedded_server/mod.rs:2465-2472`）——查 `in_flight_turns` map（键 `"{session_id}:{agent_id}:{turn_id}"`，:902）前缀匹配；与 `build_session_view` 的 busy 字段同源（`mod.rs:612-617`）。
- 注册/清除在 message_handlers 的 turn 生命周期（注册 :709-722，完成清除 :1006）——embedded 层信号；core 层无 session 级聚合（`TurnFlow::is_turn_active` 是 per-agent 的，`turn_flow/mod.rs:375-377`）。

### 9.5 会话目录布局（fork 复制决策面）

`session_dir`（`persistence.rs:276-281`）下的实体与 fork 处置：

| 实体 | 位置 | 证据 | fork 处置 |
|---|---|---|---|
| `state.json` | 根 | `persistence.rs:306-308` | 重建（§9.3 缺口） |
| `wire.jsonl` | 根 | `persistence.rs:287-289` | 截取复制 |
| `snapshot.json` | 根 | `persistence.rs:316-318`；`manager/mod.rs:642-665` | 不复制（op_index 与截取后不符；resume 的 stale 检测兜底 `restore.rs:455-477`） |
| `agents/<id>/wire.jsonl` | 子目录 | `persistence.rs:295-300` | 按成员截取复制 |
| `tasks/` | 子目录 | `task_persist.rs:7-11`；`manager/mod.rs:524` | 不复制（resume 会 rehydrate 并把 running reconcile 为 stopped，`restore.rs:129-131`——复制产生幽灵任务） |
| `blobs/` | 子目录 | `blob.rs:246,262-263`；`restore.rs:139-149` | 复制（media rehydrate 依赖 `blobref:`） |

- cron 无独立目录：仅以 `cron.add/delete/cursor` wire ops 持久化（`cron.rs:316-333`），reduce 进 `state.cron_tasks`（`reducer.rs:741-788`）——**fork 截取复制会带回 cron 状态**，是设计 12 ADR-F1 的裁决对象。

### 9.6 归属标识（cache key）现状

- Rust 模型请求配线无 cache key 通路（`mirri-core/src/{llm,profile}.rs` 零匹配）；TS 两侧现状与裁决见会话管理域 context §1.7。

## 10. ACP 远程链现状（对应 14，快照 2026-10-28）

- **远程模式装配**：`MIRRI_ACP_REMOTE_ENGINE`（URL）+ `MIRRI_ACP_REMOTE_TOKEN` → `RemoteAdapter::connect(url, token, ["approval","question"])`，handler 以单引擎构造（`rust/crates/mirri-acp/src/run.rs:72-85/145-146`）——远程模式无 cwd 引擎池（嵌入模式才有 factory 池，`run.rs:109-138`）。
- **连接期全量自动订阅**：`RemoteAdapter::connect` 在握手后调 `session.list` 并对返回的全部会话逐个 `session.subscribe`（`rust/crates/mirri-remote/src/lib.rs:315-344`）；自动订阅触发集常量为 `["session.create","session.resume","session.fork"]`（`lib.rs:87`），create 类调用的响应在订阅 ack 后才交付（subscribe-before-create）。
- **服务端订阅语义**：订阅 persisted-only 会话按需复活（restore-on-demand，`rust/crates/mirri-server/src/ws_rpc.rs:1055-1082`）；审批反向 RPC "最后订阅者赢"（订阅时把本连接 callback 盖到会话 artifacts）；连接 teardown 只 revoke callback 不清 artifacts 上的槽（`ws_rpc.rs:2117-2136`，ADR-008 never-attached vs attached-then-broken 约束）。`session.unsubscribe` 服务端存在（`ws_rpc.rs:918`），适配器侧无任何退订调用。
- **ACP 宿主调用面**：`session/fork` 实际调用引擎方法 `forkSession`（`rust/crates/mirri-acp/src/acp_handler.rs:1106`）——不在适配器触发集内；ACP 会话级 MCP overlay 经 `session.create`/`session.resume` 的 `mcpServers` 参数透传（`acp_handler.rs:543-555`）。
- **desktop 服务发现面**：desktop 固定端口 58827（`apps/mirri-desktop/src/main/server/daemon.ts:35`）、状态文件 `<home>/server/desktop-state.json`（pid+port，`state.ts:11-44`）、token 文件 `<home>/server.token`（`rust/crates/mirri-server/src/server_token.rs:23`）、healthz 公开路由（`rust/crates/mirri-server/src/http.rs:43`）。desktop-lite 自有 daemon 与状态目录（`server-lite/`），不在本探测面。
- **测试覆盖差异**：`RemoteAdapter` 单测覆盖连接认证/审批 round-trip/连接死亡（`rust/crates/mirri-remote/tests/remote_adapter_tests.rs`，885 行）；ACP 远程宿主整链（`MIRRI_ACP_REMOTE_ENGINE` 端到端）无测试引用该变量。
