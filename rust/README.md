# Mirri Code — Rust Engine

Rust 重写的 agent-core 引擎，是 TypeScript 版 `packages/agent-core` 的对等替代。目标是提供更高的性能、更好的内存安全性和更快的冷启动速度，同时保持与 TUI / Web / ACP 客户端的无缝对接。

## 快速开始

### 环境要求

- Rust toolchain（`rustup`，stable channel，edition 2024）
- 可选：`ANTHROPIC_API_KEY` 或 `OPENAI_API_KEY` 环境变量

### 构建

```bash
cd rust
cargo build --workspace
```

### 运行 CLI

```bash
# 启动 HTTP/WS/IPC 服务器
cargo run --bin mirri -- serve

# 指定工作区和端口
cargo run --bin mirri -- serve --workspace /path/to/project --port 8080

# 非交互式单次 prompt（print 模式）
cargo run --bin mirri -- print "解释这个项目的架构"

# 指定 provider 和 model
ANTHROPIC_API_KEY=sk-ant-... cargo run --bin mirri -- print "写一个 hello world" --provider anthropic --model claude-sonnet-4-20250514

# 列出已保存的会话
cargo run --bin mirri -- session-list

# 查看配置
cargo run --bin mirri -- config-show
```

### 运行测试

```bash
cargo test --workspace -- --test-threads=1
```

### 代码质量检查

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Crate 结构

```
rust/crates/
├── mirri-core            # 核心引擎：Agent、Session、TurnFlow、工具、权限、Profile
├── mirri-embedded        # 进程内引擎，组合根（EmbeddedServer + bootstrap::assemble）；ACP / NAPI / CLI print 模式使用；含 LLM provider 适配器
├── mirri-remote          # WS 客户端 EngineAdapter——把远端引擎桥接成 seam 实现（镜像 TS daemon json-rpc-client 协议）
├── mirri-server          # 传输 + 鉴权 + 事件投影，坐在 EngineAdapter 上（HTTP/WS/IPC，JSON-RPC）——不是引擎实现
├── mirri-cli             # 命令行入口（serve / print / migrate / session-list / config-show）
├── mirri-transport       # 传输层抽象（EngineAdapter trait、frame、event）
├── mirri-proto           # Wire 协议定义 + RPC 方法注册表（契约事实源：registry.rs + authoritative-methods.tsv）
├── mirri-persistence     # 会话持久化（state.json + wire.jsonl）
├── mirri-kosong          # LLM provider 传输层（SSE 解析、HTTP 客户端）
├── mirri-mcp             # MCP（Model Context Protocol）客户端
├── mirri-acp             # ACP（Agent Client Protocol）host
├── mirri-node            # Node.js NAPI 绑定（cdylib）
├── mirri-collaboration   # 多代理协作协议（7 个协作工具）
├── mirri-os              # OS 级抽象（文件系统、进程）
├── mirri-plugin-host     # 插件系统 host（开发中）
├── mirri-test-harness    # 测试工具（MockLlm、ScriptedLlm、wiremock）
```

## 核心概念

### Engine Adapter

所有客户端（TUI / Web / ACP / NAPI）通过 `EngineAdapter` trait 与引擎交互。两个实现：

- **`mirri-embedded::EmbeddedServer`** — 进程内引擎，适用于 ACP、NAPI 和 CLI print 模式
- **`mirri-remote::RemoteAdapter`** — 通过 WS 把远端引擎桥接成 seam 实现（镜像 TS daemon 的 json-rpc-client 协议）

`mirri-server` 不是 adapter：它坐在 `EngineAdapter` 上提供传输 + 鉴权 + 事件投影，把 serve wire 派发到其背后的引擎（组合根把 server 与 embedded 引擎装配在一起；`mirri serve` 即此装配）。

方法可用性以 mirri-proto 的方法注册表为准（`crates/mirri-proto/src/rpc/registry.rs` 与同目录的权威方法表 `authoritative-methods.tsv` 是唯一事实源；数字一律引 `mirri_proto::rpc::current_names_for` 与注册表 pin 测试，不另行维护）。当前（rust-core-mcp-model M1-M7 / P1-P8 合入后）：embedded 185 个、server 173 个（serve wire）、其中 both 158 个（两端共享）、v2 缺口方法已全部实现（0 个 unimplemented）；注册表共 200 行。客户端应按方法查询可用性，不能假设两端面相同。

### TurnFlow

一个 turn 代表模型的一次完整推理周期（可能包含多步 tool call）。`TurnFlow` 管理：

- LLM 流式调用和事件分发
- Tool call 执行和结果收集
- 上下文压缩（全量 + 微量）
- Steer（运行中转向）
- 系统提醒注入（plan / permission / goal / todo / background task）
- Hook 触发（19 种事件类型）

### 内置工具

| 工具 | 说明 |
|------|------|
| `Read` / `Write` / `Edit` | 文件读写编辑 |
| `Bash` | Shell 命令执行 |
| `Glob` / `Grep` | 文件搜索 |
| `Agent` / `AgentSwarm` | 子代理调度（通过 `SessionSubagentHost`） |
| `AskUserQuestion` | 结构化用户提问 |
| `WebSearch` / `FetchURL` | 网页搜索和 URL 抓取（需注入 provider） |
| `ReadMediaFile` | 多媒体文件读取（含图片尺寸嗅探） |
| `Skill` | Skill 激活 |
| `TodoList` | 任务清单管理 |
| `EnterPlanMode` / `ExitPlanMode` | 计划模式 |
| `CreateGoal` / `UpdateGoal` / `GetGoal` | 目标管理 |
| `CronCreate` / `CronList` / `CronDelete` | 定时任务 |

### Profile 系统

Profile 定义了 agent 的配置模板（model、provider、system prompt、工具集等）。支持 `extends` 继承和环境变量展开：

```toml
[[profiles]]
name = "coder"
model_id = "claude-sonnet-4-20250514"
provider_type = "anthropic"
system_prompt = "You are a coding agent."

[[profiles]]
name = "explorer"
extends = "coder"           # 继承 coder 的所有字段
model_id = "claude-haiku-4-20250506"  # 覆盖 model
```

配置文件中的 `${VAR}` 会被环境变量展开：

```toml
[providers.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
```

### 权限系统

三种权限模式：

- **Manual** — 每个敏感操作需要用户确认
- **Auto** — 自动批准（无 UI 交互时使用）
- **Yolo** — 全部放行（不推荐生产使用）

权限策略（19 条）控制每个工具的审批行为，支持通配符匹配（如 `WebSearch*`）。

### 会话持久化

会话通过 wire log（`wire.jsonl`）实现事件溯源。每个操作记录为一个 `WireOp`，重放时重建完整状态。支持：

- Session resume（从磁盘恢复）
- Session fork（分支创建）
- Session undo（上下文回滚）
- Context clear（清空上下文）

### 压缩策略

- **全量压缩（FullCompaction）** — LLM 生成摘要替换历史消息
- **微量压缩（MicroCompaction）** — 轻量裁剪，不调用 LLM，用于上下文溢出时的快速缓解

## 与 TypeScript 引擎的关系

Rust 引擎是 TS 引擎的对等替代，不是简化版。当前的策略是：

1. 两套引擎并行运行，客户端通过 adapter 层对接
2. Rust 引擎通过 E2E 测试验证功能等价性
3. TS 引擎在 Rust 引擎充分验证后才会退役

## 开发指南

### 添加新工具

1. 在 `mirri-core/src/tools/` 下创建新文件
2. 实现 `Tool` trait（`name()`、`parameters()`、`execute()`、`capabilities()` 等）
3. 在 `tools/mod.rs` 的 `register_builtin_tools()` 中注册
4. 在 `BUILTIN_TOOL_NAMES` 中添加名称
5. 在 `permission/policies.rs` 中添加权限策略
6. 添加测试

### 添加新 RPC 方法（M5 演练流程）

注册表是唯一事实源，加方法的成本被压到最小（§7 第 7 条演练方法 `core.echo` 即此流程）：

1. **注册表** — 在 `mirri-proto/src/rpc/registry.rs` 加一行（`both` / `emb` / `srv`，视可用性），并在同目录 `authoritative-methods.tsv` 加对应行（13 列；TSV 奇偶测试会拦漂移）。数字 pin 测试会要求同步更新计数。
2. **embedded 路由** — 在 `mirri-embedded/src/embedded_server/routes/<domain>.rs` 加一个 `Route`（handlers 返回 typed serde 响应 struct，不手拼 `json!`）。
3. **TS 契约**（+1 处）— 在 `packages/node-sdk/test/fixtures/rust-engine-methods.ts` 加 fixture 行，必要时在 `rust-engine-contract-e2e.test.ts` 的 `KLIENT_RESPONSE_SCHEMAS` 加 zod schema。

serve wire 不需要任何传输代码：注册表把方法标记为 `both`（或 `srv`）后，mirri-server 的 engine fall-through 自动把它派发到背后的引擎。`core.echo` 即零传输代码的示范。

### 测试约定

- 测试命名使用 **Given-When-Then** 风格：`should_return_X_when_Y`
- 使用 `--test-threads=1` 运行（部分测试修改进程级状态如环境变量）
- Mock LLM 通过 `mirri-test-harness` 的 `MockLlm` / `ScriptedLlm` 注入

### LlmFactory

`ProviderLlmFactory` 从环境变量构建 LLM provider：

- `ANTHROPIC_API_KEY` + `ANTHROPIC_BASE_URL`（默认 `https://api.anthropic.com/v1`）
- `OPENAI_API_KEY` + `OPENAI_BASE_URL`（默认 `https://api.openai.com/v1`）

注入方式：

```rust
use mirri_embedded::{EmbeddedServer, ProviderLlmFactory};

let server = EmbeddedServer::with_llm_factory(
    session_manager,
    Some(Arc::new(ProviderLlmFactory::default())),
);
```

`EmbeddedServer::for_workspace_root` / `for_cwd`（`mirri-acp` 与 NAPI 默认构造使用）以及 `mirri serve` 已默认装配 `ProviderLlmFactory`、ProfileManager（激活 `Agent` / `AgentSwarm` 工具）和本地 `FetchURL` provider；只有 `EmbeddedServer::new` 构造的服务器才需要像上面这样手动注入。

## 常见问题

### 没有 LLM provider 时能运行吗？

可以。不注入 `LlmFactory` 时，`message.send` 会返回 `"no LLM provider configured"` 错误，但其他 RPC 方法（session 管理、文件读取、工具列表等）正常工作。适合开发测试和 CI。

### 如何调试 TurnFlow？

设置 `RUST_LOG=mirri_core::turn_flow=debug` 查看详细的 turn 执行日志。`AgentActivityView` 提供细粒度的活动状态追踪。

### 如何对接 Web UI？

启动 `mirri serve`，Web UI 通过 WebSocket 连接 `/api/v1/ws`，使用 JSON-RPC 协议交互。可用方法面以 mirri-proto 方法注册表为准（`crates/mirri-proto/src/rpc/registry.rs` 与同目录权威表 `authoritative-methods.tsv`；当前 serve wire 58 个方法，见注册表 pin 测试）。
