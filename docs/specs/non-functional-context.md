# 非功能性 — 现状盘点（context）

> 快照日期：**2026-09-12**（基于工作区代码现状盘点；file:line 均为当日核对）。
> 本文只记录现状与差距，不含目标、不含裁决——需求见 [non-functional-requirements.md](non-functional-requirements.md)，机制见 [non-functional-architecture.md](non-functional-architecture.md)，执行状态见 [non-functional-ROADMAP.md](non-functional-ROADMAP.md)。
> 引擎口径：v2 = `packages/agent-core-v2`（主），v1 = `packages/agent-core`（遗留路径，`MIRRICODE_LEGACY_FLAG` 进入），标注 `[v1-source]` 的能力仅 v1 路径消费、主要由 v1 代码承载，或被 Rust 引擎路由复用的 v1 模块；`[web-source]` 标注网页端、`[desktop-source]` 标注桌面端。

## 1. 启动管线现状

### 1.1 进程入口（CLI 宿主）

- 入口链：`apps/mirri-code/package.json:25`（bin `mirri` → `dist/main.mjs`）→ `src/main.ts:117-136`（`main()`：进程名 → `installCrashHandlers()` @ `main.ts:119` → 全局代理分发器 `main.ts:120-123` → 原生模块钩子 `main.ts:124`（`src/native/module-hook.ts:26-55`）→ 原生资产自检 `main.ts:125` → 微任务后台清理旧原生缓存 `main.ts:127-134`（`src/native/native-assets.ts:343-363`）→ Commander 解析 `main.ts:136-138`）。
- 启动失败路径：`process.exitCode = 1` → 打印错误 + `See log: <全局日志路径>` → exit 1（`main.ts:156-176`，日志路径打印 `:174, :189`）。
- 惰性 import（全主链路仅 4 处动态 import）：vis 资产与 vis-server（`src/cli/sub/vis.ts:61, :134`）、ACP 服务端（`src/cli/sub/acp-native.ts:65`，注释明确"lazily so parsing the CLI does not initialize the engine"）、插件 node 入口（`src/cli/sub/plugin-run-node.ts:20`）。TUI/pi-tui/SDK 均为静态 import，主链路无按需分包。
- 更新预检：`runUpdatePreflight`（`src/cli/update/preflight.ts:682-824`），后台静默安装 detached spawn（`preflight.ts:623-635`），`MIRRICODE_NO_AUTO_UPDATE` 可禁用（`preflight.ts:409-413`）。

### 1.2 模式与引擎路由

- 模式清单与差异：TUI（`src/cli/run-shell.ts:32`）、print/一次性（`src/cli/run-prompt.ts:98`，`--output-format text|stream-json`）、web 前台（`src/cli/sub/web/run.ts`，进程内启动 kap-server；`MIRRICODE_RUST_ENGINE=1` 时改为 spawn Rust `mirri serve` 子进程 `run.ts:370-448`）、acp（`src/cli/sub/acp.ts:29-35`）、server（已废弃 shim）、vis、upgrade/export/provider/login/doctor（`src/cli/commands.ts:86-108`）。
- 引擎路由优先级：`MIRRICODE_RUST_ENGINE`（Rust NAPI，`packages/node-sdk/src/sdk-rpc-client-rust.ts:189-207`）> v2 默认（`sdk-rpc-client-v2.ts:1834-1850`）> `MIRRICODE_LEGACY_FLAG`（v1）；选择点 `src/cli/experimental-v2.ts:69-76`（`createHarnessForEngine`）。

### 1.3 TUI 初始化顺序与就绪分层

- 顺序：`startedAt` 计时（`run-shell.ts:36`）→ 客户端偏好 tui.toml（`src/tui/config.ts:1-70`）→ 主题单例（先于 pi-tui 抓 stdin，`run-shell.ts:48-50`）→ 遥测 bootstrap 构造（`run-shell.ts:53`）→ 引擎 harness（`run-shell.ts:59-75`）→ 配置读取（计时 `configMs`，`run-shell.ts:84-89`）→ TUI 构造（`run-shell.ts:90-97`）→ 遥测初始化 + crash phase=runtime（`run-shell.ts:99-106`）→ stty 保存/接管（`run-shell.ts:123-135`）→ 兜底异常 handler（`run-shell.ts:159-182`）→ `tui.start()`（`run-shell.ts:215`）。
- `tui.start()` → 首帧：信号注册（SIGTERM/SIGHUP，`src/tui/mirri-tui.ts:506-508`）→ 主初始化（实验特性 → 可用模型刷新 → 会话创建/恢复 `mirri-tui.ts:710-763`）→ 底部栏/欢迎页/补全（`mirri-tui.ts:578-585`）→ 事件循环启动（pi-tui 抓 stdin、首帧 `requestRender`，`packages/pi-tui/src/tui.ts:645-657`）→ 后台任务（横幅/输入历史/模型后台刷新/fd 二进制/会话列表/技能命令刷新，`mirri-tui.ts:580-588, :626-639, :663-668`）→ `finishStartup()`（`mirri-tui.ts:514-515`）。
- 启动性能事件：`startup_perf`（duration_ms/config_ms/init_ms/mcp_ms，`run-shell.ts:219-224`）；MCP 就绪耗时取自引擎 `getMcpStartupMetrics()`（`mirri-tui.ts:1413-1422` → `packages/node-sdk/src/sdk-rpc-client-v2.ts:1759-1763`）。无独立的 first_frame 事件。
- v2 引擎侧启动：bootstrap 组合根（`packages/agent-core-v2/src/app/bootstrap/bootstrap.ts:152-197`）→ 配置构造即异步加载 + 磁盘监听重载（`app/config/configService.ts:252-258`）→ 会话创建（`workspace/sessionLifecycle/sessionLifecycleService.ts:203-301`：skill 目录 `void ready` 不等待 `:283`；5 个 agent-profile loader 并行 `:284-290`；仅保守 flag 时 `await mcp.ready` `:291-293`）→ agent 创建（`session/agentLifecycle/agentLifecycleService.ts:118-191`，默认不等 `mcpReady` `:150`）。
- MCP 快路径（默认启用，回滚开关 `MIRRICODE_DISABLE_MCP_FAST_PATH`）：`session/mcp/mcpFastPath.ts:15-28`；等待点推迟到首个 LLM step 前（`agent/mcp/mcpService.ts:117-122`）与每次 MCP 工具执行前（`:123-127`）。MCP 本体并行连接（`mcpCore/connection-manager.ts:207-224`）。

### 1.4 原生资产与外部二进制

- SEA 内嵌资产：`src/native/native-assets.ts`（缓存目录解析 `:136-155`，版本哈希寻址 `:157-170`，逐文件 sha256 校验后落盘 `:180-240`）；模块钩子重定向失败透传（`src/native/module-hook.ts:26-55`）；陈旧缓存清理保留当前+最新兄弟（`native-assets.ts:288-337`）。
- ripgrep 四级解析：PATH → vendor（恒无）→ `<home>/bin/rg` 缓存 → 下载（v1 路径 `packages/agent-core/src/tools/support/rg-locator.ts:100-152, :176-260` `[v1-source]`；v2 路径 `packages/agent-core-v2/src/os/backends/node-local/tools/rgLocator.ts`，下载超时 600s `:26`）；下载模块级锁防并发（v1 `rg-locator.ts:107-121`）。
- rg 缺失降级（**两条路径行为不同**）：工具层（grep/glob 工具）同步等待下载完成后继续，失败报错 + 安装指引，另有下载解析结果埋点（v1 `tools/builtin/file/grep.ts:212-252` `[v1-source]`；v2 `agent/tools/os/grep/grepTool.ts:157-172`）；引擎 fs 层不触发下载，rg 不可用即直接用纯 Node 实现 + 降级埋点（v1 `services/fs/fsSearchService.ts:127-149`；v2 `workspace/workspaceFs/fsService.ts:500`）+ 安装指引文案（v1 `rg-locator.ts:354-370`）。
- Rust NAPI 引擎加载失败返回 null → 每次调用报 `not_implemented`（`sdk-rpc-client-rust.ts:189-207`）；node-pty/剪贴板/Rust 引擎为 optionalDependencies（`apps/mirri-code/package.json`）。

### 1.5 桌面壳两阶段就绪 [desktop-source]

- Electron（`apps/mirri-desktop/package.json`）；启动 `src/main/index.ts:346-441`（单实例锁 → 清理陈旧 daemon → 托盘/IPC → 窗口）。
- 两阶段：健康检查通过即 loadURL（`src/main/ensure-server.ts:334-365`，注释 `:144-149`），后台轮询至完全就绪（`:380-391`）；健康探测参数 `:9-14`（超时 20s、轮询 200ms）；端口复用 `:343-350`；关窗驻留托盘 `index.ts:223-233`。

## 2. 并发模型现状

- **回合串行**：FIFO turn 队列，一次一个 LLM step（v2 `agent/loop/loopService.ts:4-16` 文档注释；步预算 `:661-664`，`maxStepsPerTurn` 未配置则无上限）。v1 同构 `[v1-source]`（`packages/agent-core/src/loop/`）。
- **工具按资源冲突并行**：v2 自研 ToolScheduler，`ToolAccesses.conflict` 判定（`agent/toolExecutor/toolScheduler.ts:27-99`），非冲突立即 start，结果按完成顺序 `Promise.race` 流式产出（`toolExecutor/toolExecutorService.ts:180-283, :459-503`）；无全局数值上限。v1 对应 `loop/tool-scheduler.ts:1-60` `[v1-source]`。
- **swarm 斜坡调度**：初始 5 个 + 每 700ms 放行 1 个（v2 `session/swarm/agentRunBatch.ts:46-47, :193-221`；v1 `session/subagent-batch.ts:32-33` `[v1-source]`）；硬上限仅环境变量 `MIRRICODE_AGENT_SWARM_MAX_CONCURRENCY`（v2 `agentRunBatch.ts:54, :639-651`；v1 `subagent-batch.ts:40, :670-687`），默认不限；429 容量收缩/3 分钟恢复 +1（v2 `:441-489`）。
- **MCP 并行连接/重连去重**：`mcpCore/connection-manager.ts:207-224, :226-256`。
- **kap-server 单进程多会话**：一个 Core Scope（`packages/kap-server/src/start.ts:254-270`）；实例注册表心跳 15s（`src/instanceRegistry.ts:25`）；端口顺延重试上限 100（`start.ts:679`）。REST + WS（无 SSE）；WS 出站缓冲：1000 帧、16ms 合批、单批 64 帧、高水位 1MB、背压重试 5ms 起步封顶 100ms（`src/transport/ws/v1/wsConnectionV1.ts:55-68`）；服务端无心跳不主动断开（`:15-18`）。
- **取消传播**：`loopService.cancel`（`agent/loop/loopService.ts:243-296`）→ 步信号 `AbortSignal.any`（`:677-680`）→ 工具 2s 宽限强收（`toolExecutorService.ts:69, :912-948`）+ "用户手动中断勿自动重试"注入（`:905-910`）；swarm 整批收尾 `aborted`（`agentRunBatch.ts:531-559`）；取消原语 `UserCancellationError`（`_base/utils/abort.ts:16-58`）。
- 全仓无 p-limit/Semaphore 依赖；grep 工具内 mtime stat 并发 32（`agent/tools/os/grep/grepTool.ts:78, :416-418`）。

## 3. 超时清单现状（file:line 索引）

| 等待 | 默认值 | 位置 |
|---|---|---|
| LLM 模型请求 | **无请求级超时**（仅 AbortSignal） | `agent-core-v2/src/kosong/model/modelRequesterImpl.ts:112-135` |
| MCP startup | 30s | `agent-core-v2/src/mcpCore/connection-manager.ts:50, :266-269` |
| MCP 工具调用 | 可配（未配不限） | `connection-manager.ts:344-345`；`client-shared.ts:59-63`；配置 `app/mcpConfig/configSection.ts:24-37`（env `MIRRI_MCP_STARTUP_TIMEOUT_MS`/`MIRRI_MCP_TOOL_TIMEOUT_MS`） |
| MCP liveness ping | 5s | `mcpCore/client-shared.ts:33` |
| 子代理任务 | 2h | `agent-core-v2/src/session/subagent/configSection.ts:45-72`（env `MIRRI_SUBAGENT_TIMEOUT_MS`）；v1 `agent-core/src/session/subagent-host.ts:33` `[v1-source]` |
| swarm 单任务 | 可配 `task.timeout` | `session/swarm/agentRunBatch.ts:602-633` |
| 工具取消宽限 | 2s | `agent/toolExecutor/toolExecutorService.ts:69, :912-948` |
| 前台 shell | 120s | `agent-core-v2/src/agent/shellCommand/shellCommandService.ts:69`；v1 `agent/tool/index.ts:35` `[v1-source]` |
| rg 搜索 | 20s | `os/backends/node-local/tools/runRg.ts:14`、`workspace/workspaceFs/internal/runRg.ts:15` |
| workspace grep | 30s | `workspace/workspaceFs/fsService.ts:93, :493` |
| rg 下载 | 600s | `os/backends/node-local/tools/rgLocator.ts:26` |
| git 上下文收集 | 5s | `session/agentLifecycle/profile/gitContext.ts:21` |
| MCP OAuth 流 | 15min | `agent/mcp/tools/auth.ts:45` |
| 外部 hooks | 30s（可配 1-600s） | `app/externalHooksRunner/runner.ts:24`、`agent/externalHooks/configSection.ts:27` |
| task 等待 | 30s | `agent/task/taskService.ts:869` |
| SQLite busy | 5s | `app/sessionIndex/sqliteSessionStore.ts:14, :104` |
| minidb 锁 | 1s | `persistence/backends/minidb/miniDbQueryStore.ts:70` |
| 遥测上报 | 10s | `packages/telemetry/src/transport.ts:39`；v2 `app/telemetry/cloudTransport.ts:56, :314-340` |
| 遥测 flush | 50 条 / 30s | `packages/telemetry/src/sink.ts:29-30`；v2 `app/telemetry/cloudAppender.ts:75-76` |
| 调试 RPC 派发 | 30s | `kap-server/src/transport/serviceDispatcherRoutes.ts:56, :137` |
| 网页 REST | 30s | `apps/mirri-web/src/api/daemon/http.ts:14, :306-311`（不自动重试） |
| 网页握手 | 10s | `apps/mirri-web/src/api/daemon/json-rpc-client.ts:146, :237-244`（故意无 per-request 超时 `:286`） |
| 网页心跳 | 15s × 3 | `json-rpc-client.ts:147-148, :463-478`（超时 close 4000） |
| 重连退避 | 1s×2 封顶 30s + jitter | `apps/mirri-web/src/api/daemon/ws.ts:156-166` |
| Rust server 就绪探测 | 30s/250ms/1s | `cli/sub/web/run.ts:76-82` |

## 4. 重试现状

- **步级重试（v2 核心）**：`agent/stepRetry/stepRetryService.ts:70-158`（注册 loop 错误处理器 `:83-88`；重试重入队列头部 `:156`）；可重试分类 `kosong/contract/errors.ts:208-222`（408/409/429/500/502/503/504/529）；退避 `_base/utils/retry.ts:10-31`（10 次尝试、500ms×2 封顶 32s、+25% jitter、尊重 retry-after `:33-37`）；配置 `[loop_control] maxRetriesPerStep`（`agent/loop/configSection.ts:27, :31`）。
- v1 对应 `[v1-source]`：`packages/agent-core/src/loop/retry.ts:15-139`（同参数）。
- **请求降级链**：`agent/llmRequester/llmRequesterService.ts:447-507`（媒体链 normal → media-degraded → media-stripped；结构可恢复错误独立降级 strict，仅 normal 态触发）。
- **401 刷新重放一次**：`kosong/model/modelRequesterImpl.ts:185-202`。
- **swarm 429 重排队**：`session/swarm/agentRunBatch.ts:324-330, :404-455`（3s×2 指数）。
- **流中断无自动重连**：`modelRequesterImpl.ts:155-158`（靠步级重试恢复，`stepRetryService.ts:4-7` 文档）。
- **MCP 重连去重**：`mcpCore/connection-manager.ts:226-256`。
- **遥测传输重试**：1s/4s/16s、仅瞬态（5xx/429）重试，失败落盘（`packages/telemetry/src/transport.ts:19-20, :84-110, :279-286`）。
- **模型 HTTP 客户端层无显式重试/超时配置**（用 SDK 默认）：`packages/kosong/src/providers/anthropic.ts:1177-1184`、`openai-legacy.ts:693-708`、`openai-responses.ts:1188`；重试语义全部在上层（见上）。
- 429 语义细分：OpenAI quota 类不可重试（`kosong/provider/bases/openai/openai-common.ts:113-144`）；Kimi 分类（`kosong/provider/providers/kimi/kimi-errors.ts:65-82`）。

## 5. 流式与呈现性能现状

- **引擎流式管道**：provider 回调 → `AsyncEventQueue.push`（`kosong/model/modelRequesterImpl.ts:73-79, :147-150`）→ 消费端 `for await` + `await onPart`（`llmRequesterService.ts:397-419`）。**队列缓冲无界**（`_base/asyncEventQueue.ts:15-72`，纯 push 数组）——消费端 await 是唯一反压点。
- **TUI 差分渲染**：逐行 diff 只重绘变化区间（`packages/pi-tui/src/tui.ts:1359-1399`）；渲染请求合并 + `MIN_RENDER_INTERVAL_MS = 16`（`:319, :722-769`）；同步输出防撕裂 `\x1b[?2026h/l`（`:1310, :1332`）；全量重绘仅首帧/尺寸变化（`:1359-1389`）；调试开关 `PI_DEBUG_REDRAW`（`:1351-1357`）。
- **TUI 转录滑动窗口**：最近 15 回合（env `MIRRICODE_TUI_MAX_TURNS`）、迟滞 5、可展开 3、回合内保留 30 步（`apps/mirri-code/src/tui/utils/transcript-window.ts:28-41`）；应用点整块销毁（`src/tui/mirri-tui.ts:1880-1892, :1992-2010`）。
- **工具输出截断**：超 50,000 字符落盘 + 2,000 字符预览（v2 `agent/toolResultTruncation/toolResultTruncationService.ts:24-25`，接入 `toolExecutorService.ts:657-661`）。
- **web 事件合并**：高频事件标记 + 帧级批处理（rAF 调度、单 slice ≤100 项、合并流上限 32KiB、50ms 兜底延迟、超大 delta 拆分、偏移连续时无损合并）（`apps/mirri-web/src/composables/client/eventBatcher.ts:11-16, :33-38, :81-204, :232-330`）；接线 `useMirriWebClient.ts:1036-1106`。
- **worker 卸载**：KaTeX/Mermaid 走 `?worker&type=module`（`apps/mirri-web/src/components/chat/Markdown.vue:20-21, :64-65`；`vite.config.ts:229-234` ES worker 以支持按需分包）。
- **大内容降级**：文本 ≥120k / 代码 ≥60k / 围栏 ≥32 / 单围栏 ≥30k 时高亮降级纯文本（`apps/mirri-web/src/lib/markdownPerformance.ts:9-12, :36`）。
- **web 无虚拟滚动**（全仓无命中）；长会话靠分页加载（`apps/mirri-web/src/components/chat/ConversationPane.vue:75-76, :649-670`）。
- **性能埋点**：LLM 流时序（首 token 延迟/流时长/构建耗时/服务端首 token 等，`modelRequesterImpl.ts:106-241` → 日志 `llmRequesterService.ts:701-720`）；工具 durationMs（`toolExecutorService.ts:472-478, :306-327`）；turn 计时（`activityViewService.ts:257-261`）；DI 启动耗时（`_base/di/instantiationService.ts:56-73`）；`MIRRICODE_DEBUG=1` 显示 turn step 调试计时（`src/tui/controllers/session-event-handler.ts:397`）。

## 6. 日志体系现状

- **v2 双层日志**（主路径）：App 级 `AppLogService` + Session 级 `SessionLogService`（`agent-core-v2/src/_base/log/logService.ts:132-166`、`src/session/sessionLog/sessionLogService.ts:32-79`）；轮转 writer `fileLog.ts:49-235`（PENDING_MAX=1000 `:20`、每批 fsync `:170-182`、目录 fsync 一次 `:115-118`、stderr 失败节流 30s `:21`）。配置 env `MIRRI_LOG_*`（`_base/log/logConfig.ts:16-20, :49-60`），默认全局 6MB×5 / 会话 5MB×3，全局路径 `<homeDir>/logs/mirri-code.log`（`:41-43`）。
- **v1 日志栈** `[v1-source]`：根 logger symbol 单例挂 globalThis（`agent-core/src/logging/logger.ts:34-269`）；双 sink（全局 + 每 session `<sessionDir>/logs/mirri-code.log`，`:58-85`）；轮转 sink `sinks.ts:33-221`（PENDING_MAX=1000 `:7`、fsync `:155-162`、同步逃生 `:68-78`）；配置 env `MIRRICODE_LOG_*`，v1 显式不读 config.toml（`logging/resolve-config.ts:22-37`）；由 node-sdk 客户端构造时配置（`packages/node-sdk/src/sdk-rpc-client.ts:75, :98`；rust 变体 `sdk-rpc-client-rust.ts:474`）。**两套 env 前缀不同**（`MIRRI_LOG_*` vs `MIRRICODE_LOG_*`）——已知不一致。
- **v1 services/logger/logger.ts 是纯 DI 接口声明，仓内无实现**（`agent-core/src/services/logger/logger.ts:5-17`；`services/AGENTS.md:96` 注明 adapter 在退役的 v1 server）——易混淆点，已澄清。
- **kap-server 日志**：pino NDJSON（`kap-server/src/services/pinoLoggerService.ts:14-31`，不桥接引擎日志）；访问日志按信封错误码（`requestLogging.ts:27-50`）；`--log-level` CLI 参数（`apps/mirri-code/src/cli/sub/web/shared.ts:21-111`）。
- 无 `--debug`/`--verbose` flag（在拒绝列表，`apps/mirri-code/test/cli/options.test.ts:390-399`）；测试统一 `MIRRICODE_LOG_LEVEL=off`（`apps/mirri-code/vitest.config.ts:16`）。
- 紧急 flush：`flushDiagnosticLogsSync`（v1 `logging/logger.ts:285-287`），TUI 应急退出 `run-shell.ts:151` 与启动失败 `main.ts:203` 调用。

## 7. 遥测体系现状

### 7.1 共享遥测包（`packages/telemetry`，CLI 装配路径）

- 三段管道：`TelemetryClient`（内存队列上限 1000 溢出丢最旧，`client.ts:20, :115-116`；作用域派生 `:168-223`；禁用即清一切 `:72-82`）→ `EventSink`（50 条/30s，`sink.ts:29-30`；上下文 enrichment `:99-117`）→ `AsyncTransport`（端点 `https://telemetry-logs.kimi.com/v1/event` `transport.ts:16`；10s 请求超时 `:39`；重试 1s/4s/16s 仅瞬态 `:20, :84-110`；401 去鉴权重试一次 `:181-188`；失败落盘 `failed_*.jsonl` 权限 0600 `:112-122`；暂存 7 天过期 + 损坏清理 `:124-169`）。
- bootstrap：`initializeTelemetry`（`bootstrap.ts:36-75`：装配 + 周期 flush + 系统指标采样 + 暂存重发）；禁用判定 env 优先（`:25-34`，`MIRRICODE_DISABLE_TELEMETRY`，truthy 值集合 `:8`）。
- 系统指标：预热 1.5s + 每 5 分钟（`systemMetrics.ts:5-6`），采集 rss/heap/external/arrayBuffers/cpu/loadavg/free/total/constrained（`:72-103`）。
- crash 处理：`crash.ts:16-72`（uncaughtExceptionMonitor + unhandledRejection；AbortError 过滤；唯一监听者时 rethrow 保默认崩溃；rejection 去重 Set）。**唯一安装点 `apps/mirri-code/src/main.ts:119`**；phase 设置 `run-prompt.ts:142, :187`、`run-shell.ts:106, :187, :227`。
- 身份：deviceId `createMirriDeviceId`（`packages/oauth/src/identity.ts:46-68`，目录 0700/文件 0600，首启事件 `cli/telemetry.ts:62-64`；**`packages/oauth` 已于 2026-10-13 随 managed login 退役删除，本行系退役前实现记录**）；sessionId 引擎侧 `session_${uuid}`（`agent-core-v2/src/workspace/sessionLifecycle/sessionLifecycleService.ts:677`），TUI 用 `withTelemetryContext` 按会话绑定/解绑（`run-shell.ts:108-121`、`mirri-tui.ts:1574`）。

### 7.2 v2 引擎遥测服务

- `TelemetryService`（App scope，appender 列表，`app/telemetry/telemetryService.ts:27-98`）+ `track2` 类型化封装（`:48-53`）；**类型化事件注册表** `app/telemetry/events.ts`（`defineAgentTelemetryEvent`，事件定义 `:450-916`，如 `turn_started/turn_interrupted/turn_ended` 含 `duration_ms`/`trace_id`）。
- 云端 appender：`cloudAppender.ts`（同 50/30s 阈值 `:75-76`）、`cloudTransport.ts`（同超时/退避/落盘语义 `:54, :108-172, :305-340`）。
- kap-server 桥接：`initializeServerTelemetry`（`kap-server/src/services/telemetry.ts:47-74`，opt-in `start.ts:277-287`；关闭 3s 上限 `:34`；**env 名为 `MIRRI_DISABLE_TELEMETRY`，与 CLI 的 `MIRRICODE_DISABLE_TELEMETRY` 不同**——已知不一致）。
- ACP 适配器透传 track（`packages/acp-adapter/src/server.ts:900`、`src/session.ts:1377`）。

### 7.3 事件清单（抽样全量按包分组）

- **CLI 宿主（`apps/mirri-code/src`）**：`first_launch`（cli/telemetry.ts:63）、`startup_perf`（run-shell.ts:219）、`exit`（run-shell.ts:188, 229；run-prompt.ts:202）、`oauth_refresh`（run-prompt.ts:122, 125；run-shell.ts:66, 69）、`server_started`（sub/web/run.ts:352, 507）、`login/logout`（tui/commands/auth.ts:71, 178, 239）、`model_switch/thinking_toggle/theme_switch/experimental_features_apply/upgrade_preference_changed`（tui/commands/config.ts:483-744）、`connect`（tui/commands/provider.ts:226）、快捷键族 `shortcut_*` + `undo`（tui/controllers/editor-keyboard.ts:220-485）、`shell_command/input_queue`（mirri-tui.ts:1011, 1198）、goal 族（tui/commands/goal.ts:206-490）、`init_complete`（tui/commands/session.ts:171）、`input_command*/clear`（tui/commands/dispatch.ts:195-244）、更新族 `update_*`（cli/update/preflight.ts:352-611）。
- **v2 引擎（`agent-core-v2/src`，track2）**：`turn_started/turn_interrupted/turn_ended`（agent/loop/loopService.ts:483, 526, 539）、`api_error`（llmRequesterService.ts:279-312）、`tool_call`（toolExecutorService.ts:306-327）、cron 族（session/cron/sessionCronServiceImpl.ts:426-500）、`session_create_ms/session_started/session_load_failed`（sessionLifecycleService.ts:229-512；workspaceLifecycle/sessionLookup.ts:55）、`mcp_connected/mcp_failed`（workspaceMcpService.ts:118, 126）、`fs_grep_node_fallback`（workspaceFs/fsService.ts:500）、compaction 族（fullCompactionService.ts:748, 763）、`subagent_created`（subagent/mirrorAgentRun.ts:119）、媒体族（agent/media/）、plan/goal/skill/permission/approval/question/glob/grep/undo/model_switch 等（对应 v1 事件的 v2 版本，详见各服务文件）。
- **v1 引擎 `[v1-source]`（`agent-core/src`）**：`turn_*/api_error/tool_call*`（agent/turn/index.ts:523-1142）、`mcp_connected/mcp_failed`（session/index.ts:872, 880）、`session_started/session_load_failed`（rpc/core-impl.ts:422, 532）、compaction/goal/skill/background/permission/image/video/grep fallback 族（详见 `agent/`、`tools/`、`services/fs/` 各文件）。
- **node-sdk**：`session_started`（含 resumed，`mirri-harness.ts:314-328`）、`session_resume/reload/fork/export`（`:159-233`）。
- **web 端无 track 调用**（仅本地调试 trace 环形缓冲 + console patch，`apps/mirri-web/src/debug/trace.ts:71, :309-373`）。

### 7.4 opt-out 面现状

| 面 | 位置 | 说明 |
|---|---|---|
| 环境变量 `MIRRICODE_DISABLE_TELEMETRY` | `packages/telemetry/src/bootstrap.ts:6, :25-33` | truthy 即关；优先于配置 |
| config.toml 顶层 `telemetry` 布尔键 | `agent-core/src/config/schema.ts:301, :347`；读取 `apps/mirri-code/src/cli/telemetry.ts:53, :102` | web/server 路径坏配置时降级为开启、不阻塞启动（`:117-129`） |
| 网页设置页开关 | `apps/mirri-web/src/components/settings/SettingsView.vue:236-241, :644-652`（REST patch `packages/protocol/src/rest/config.ts:32, :57`） | 带"重启生效"提示 |
| **TUI 内开关** | **无**（`src/tui/commands/config.ts` 无 telemetry 项） | 提案 P1 |
| 服务端 env `MIRRI_DISABLE_TELEMETRY` | `kap-server/src/services/telemetry.ts:27` | 与 CLI 变量名不一致（提案 P2） |

## 8. 崩溃与退出现状

- 崩溃处理见 §7.1（crash.ts）。TUI 兜底异常 → 紧急退出：同步诊断日志 → 终端恢复（光标/修饰键/Kitty，`src/utils/terminal-restore.ts:18-30`）→ exit 1（`run-shell.ts:146-182`）。
- 信号矩阵：Ctrl+C 为键盘输入分级处理（取消 → 清空 → 双击退出，`tui/controllers/editor-keyboard.ts:123-180`；custom-editor.ts:427）；SIGTERM 优雅 143（`mirri-tui.ts:838-859`）；SIGHUP 紧急 129（`:844-896`）；stdout EIO 紧急（`:866-878`）；print 模式 130/143/129（`run-prompt.ts:396-427`）；web 模式优雅关停（`sub/web/run.ts:354-359`）。
- 退出时限：`CLI_SHUTDOWN_TIMEOUT_MS = 3000`（`src/constant/app.ts:22`，shutdownTelemetry 调用点 `run-shell.ts:189, :229`、`run-prompt.ts:146`、`main.ts:111` 等）；headless 整体清理 8s + stdio 排空 10s + 强杀 2s（`constant/app.ts:28, :35, :42`；`run-prompt.ts:139-156`；`src/cli/headless-exit.ts:29-96`）。
- TUI 正常退出链：`MirriTUI.stop()`（`mirri-tui.ts:786-831`）→ `closeSession` + `harness.close()` → onExit：track exit → shutdownTelemetry(3s) → "Bye!" + resume 提示 → 恢复 stty → exit（`run-shell.ts:184-212`）。会话数据由引擎持续落盘，退出无专门保存步骤。

## 9. 资源与内存现状

- **有界点**：日志轮转与缓冲（§6）、遥测内存队列 1000（§7.1）、遥测暂存 7 天（§7.1）、TUI 转录窗口（§5）、web 事件批处理上限（§5）、WS 出站缓冲 1000 帧/1MB（§2）、工具输出 50k 落盘 + 2k 预览（§5）、原生缓存版本清理（§1.4）。
- **无界点（诚实披露）**：引擎流式队列缓冲无界（§5）；工具并发无全局数值上限（§2，由资源声明决定）；swarm 并发默认无硬上限（§2）；进程无内存上限与自我保护（全仓无 heap 上限配置）——对应提案 P7/P9。
- 缓存均为普通 Map/WeakMap，无 LRU 淘汰（gitignore/realRoots 缓存 `workspaceFs/fsService.ts:104-106, :712-750`；视频上传缓存 `agent/media/videoResolverService.ts:48, :110-116`）。

## 10. 覆盖差距矩阵（requirements vs 代码）

本域为新建基线，"差距"= TS 已支持但需求文档未承诺（回归风险），或需求文档为增强（超出 TS 现状、已标注）：

| # | 能力 | TS 代码现状 | 需求文档 | 判定 |
|---|---|---|---|---|
| 1 | MCP fast path 回滚开关 | `mcpFastPath.ts:15-28`（`MIRRICODE_DISABLE_MCP_FAST_PATH`） | US1.2 验收第 3 条 | 已覆盖 |
| 2 | 遥测 opt-out 三面 + TUI 无开关 | §7.4 | US6.1 + 提案 P1 | 已覆盖（TUI 缺口转提案） |
| 3 | 两套遥测禁用 env 名并存 | `kap-server` vs CLI（§7.4） | 提案 P2 | 已覆盖（不一致转提案） |
| 4 | LLM 请求无请求级超时 | `modelRequesterImpl.ts:112-135` | US4.2 显式记录 + 提案 P6 | 已覆盖（取舍显式化） |
| 5 | 流式队列无界 | `_base/asyncEventQueue.ts` | NFR 诚实披露 + 提案 P7 | 已覆盖 |
| 6 | 双代日志栈并存（env 前缀不同） | §6 | 提案 P10 | 已覆盖 |
| 7 | crash 处理器唯一安装点在 CLI main | `main.ts:119` | US7.1（宿主层职责） | 已覆盖（进程宿主层安装，引擎侧崩溃另有 v2 onUnexpectedError 兜底 `telemetryService.ts`） |
| 8 | v2 会话/压缩/启动各阶段性能埋点 | §5 性能埋点 | US1.3/US6.5 | 已覆盖 |
| 9 | web 无虚拟滚动 | §5 | 引用对话渲染域提案 P1（本域 P8 登记依赖） | 已覆盖（不重复提案） |
| 10 | 首启事件 first_launch | `cli/telemetry.ts:62-64` | US6.1 验收第 1 条 | 已覆盖 |
| 11 | 无 first_frame 事件 | §1.3 | 提案 P3 | 已覆盖（缺口转提案） |
| 12 | 日志不读 config.toml（仅 env） | `resolve-config.ts:22-37`（v1 显式注释） | US5.1（环境变量控制级别） | 已覆盖 |
| 13 | 更新预检可禁用 + 静默 | `preflight.ts:409-413, :623-635` | US1.1 验收第 3 条 | 已覆盖 |
| 14 | 系统 CLI 无 --debug flag | `options.test.ts:390-399` | 需求未承诺 flag（日志级别 env 已覆盖 US5.1） | 一致（无需缺口条目） |

## 11. v1-source 能力清单

| 能力 | v1 代码位置 | 消费端 | v2 对应物 |
|---|---|---|---|
| 日志栈（根 logger/轮转 sink/`MIRRICODE_LOG_*`） | `agent-core/src/logging/`（§6） | v1 引擎路径（`MIRRICODE_LEGACY_FLAG`）与 Rust 后端客户端（`sdk-rpc-client-rust.ts:474`） | `_base/log/`（`MIRRI_LOG_*`） |
| 步级重试参数 | `loop/retry.ts:15-139` | v1 loop | `stepRetryService.ts` + `_base/utils/retry.ts` |
| swarm 斜坡/限流收缩 | `session/subagent-batch.ts` | v1 子代理批 | `session/swarm/agentRunBatch.ts` |
| rg 定位/下载/降级 + 埋点 | `tools/support/rg-locator.ts`、`services/fs/fsSearchService.ts`、`tools/builtin/file/{grep,glob}.ts` | v1 工具 | `os/backends/node-local/tools/rgLocator.ts` 等 |
| v1 遥测事件族 | `agent/turn/index.ts` 等全量（§7.3） | v1 引擎 | v2 类型化事件注册表 |
| v1 ToolScheduler | `loop/tool-scheduler.ts` | v1 loop | `agent/toolExecutor/toolScheduler.ts` |
| v1 services/logger 的 `ILogService` | `services/logger/logger.ts`（仅接口，仓内无实现） | 无实装消费 | v2 log service |

## 12. 尚待确认/未盘点区域

- `apps/mirri-desktop` 与 `apps/vis` 的遥测调用未逐文件盘点（桌面壳无自有 track，见 §7.3 口径）。
- v1 `services/logger` 是否有仓外实现消费（判定为无——退役 server 承载过，见 §6）。
- `packages/agent-core-v2/src/app/telemetry/events.ts` 全量事件数（定义约 50+ 个，本盘点抽样列举代表性事件；完整清单以该文件为准）。

## 13. env 命名规范归档（B11 增量，2026-09-13）

> 依据 `plan/batches/B11-exec-env-verification.md`（缺口 D-14～D-18/D-22）。本域与遥测/日志相关的 env 命名行归档如下；完整规范表与裁决理由见执行环境域 `docs/specs/execution-environment-context.md` §6.1。

| 用途 | 规范名（`MIRRI_*` 基准） | TS v2 现行 | v1/库栈残留 | Rust 现行（B11 后） |
|---|---|---|---|---|
| 遥测禁用 | `MIRRI_DISABLE_TELEMETRY` | kap-server（`packages/kap-server/src/services/telemetry.ts:27`，§7.4 提案 P2 的不一致面） | CLI `MIRRICODE_DISABLE_TELEMETRY`（`packages/telemetry/src/bootstrap.ts:6, :25-33`，§7.1） | 无（Rust 尚无遥测 sink，总计划 DEC-2 P-004/P-008 推迟；待实装时采用规范名） |
| 日志前缀 | `MIRRI_LOG_*` | `_base/log/logConfig.ts:52-58`（§6） | v1 `MIRRICODE_LOG_*`（`agent-core/src/logging/resolve-config.ts:22-37`，§6） | 无专用日志 env（待实装时采用规范名） |
| MCP fast path 回滚开关 | `MIRRI_DISABLE_MCP_FAST_PATH`（待实装） | `session/mcp/mcpFastPath.ts:16` 用 `MIRRICODE_DISABLE_MCP_FAST_PATH`（§10 差距 #1） | 同左（v2 现行即此名） | 无对应 |

**裁决基准**：Rust 现行 `MIRRI_*` 为规范名（先例 `rust/crates/mirri-core/src/config/env_layer.rs:479-485` 的 `env_var_with_alias` 主名+别名模式）；两代一致的 `MIRRICODE_HOME` 保留原名。归档不是全量改名令，TS 侧不修（总计划 DEC-1）。
