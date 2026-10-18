# Rust Core Crate 结构设计

> **文档类型：设计类**（按 `docs/specs/AGENTS.md` 治理约定）——只因设计变更修改。结构快照（2026-09-04）：17-crate 现实对照与规划差距见 §1.6，差距的当前状态以 [ROADMAP.md](ROADMAP.md) 为准。
> （OS 抽象 crate 实名为 `mirri-os`；`mirri-embedded` / `mirri-remote` 承载 EngineAdapter 实现；
> `mirri-collaboration` / `mirri-plugin-host` 并入未执行，仍为独立 crate）。
> 原「13 个 → 11 个」的收敛目标已废弃，见 §1.6 与 §2。

> 状态: Draft
> 日期: 2026-08-29
> 作者: Architecture Team
> 关联文档: [00-overview](00-overview.md) · [01-protocol-unification](01-protocol-unification.md) · [03-phase3-tui-acp-migration](plans/03-phase3-tui-acp-migration.md) · [06-cross-platform](06-cross-platform.md)

本文档定义架构迁移完成后 Rust workspace（`rust/crates/`）的最终 crate 结构与依赖关系。它回答三个问题：

1. **拆成哪些 crate** —— 单一职责的边界划在哪里
2. **谁依赖谁** —— 依赖方向如何保证 core 零 IO、前端自由组合
3. **每个 crate 暴露什么** —— public API surface 与测试策略

## 1. 设计原则

### 1.1 每个 crate 单一职责

每个 crate 只回答一个问题：

| Crate | 它回答的问题 |
|-------|-------------|
| `mirri-core` | 引擎如何思考（域逻辑） |
| `mirri-proto` | 线上说什么话（wire 格式） |
| `mirri-transport` | 话怎么传（管道抽象） |
| `mirri-os` | OS 差异如何隐藏（路径/Shell/FSWatcher/IPC） |
| `mirri-persistence` | 说过的话怎么存（append-log） |
| `mirri-mcp` | 如何接外部 MCP server |
| `mirri-server` | 如何服务远程前端（HTTP/WS/IPC 监听） |
| `mirri-acp` / `mirri-node` | 如何嵌入到宿主（stdio / NAPI） |
| `mirri-cli` | 人类如何启动它 |
| `mirri-test-harness` | 如何证明以上全部正确 |

判据：**一个 crate 的主题被问「它还做什么」时，如果答案需要第二个动词，就该拆**。

### 1.2 `mirri-core` 零 IO 依赖

`mirri-core` 是纯领域层，禁止直接执行任何 IO（网络、文件系统、进程、时钟之外的一切副作用入口）。它只通过 trait 声明能力，由外层注入实现：

- 文件/路径/进程/pty → `mirri-os` 的 `PathCodec` / `ShellProvider` / `FsWatcher` / `PtyProvider` trait（trait 本身无 IO 实现）
- 存储 → `mirri-persistence`（core 只依赖其纯类型接口，不持有文件句柄）
- 外部 MCP server → `mirri-mcp` 的 client trait
- LLM → `Llm` trait（实现方在 provider crate）
- 定时/后台任务 → core 内部基于注入的 clock/spawner，而非直接 `tokio::time`

> **现状说明**：当前 workspace 中 `mirri-core` 直接依赖 `mirri-os`（含 fs/process/pty 的**实现**）。迁移后 core 对它的依赖收敛为 **trait-only**（见 3.5），具体 OS 实现由 `mirri-acp` / `mirri-node` / `mirri-server` 等接入层注入。逐模块迁移路径见下方「零 IO 迁移清单」。

> **裁决 Z1（zero-IO 取向：trait 化收敛，2026-09-07 用户裁决）**：mirri-core 维持本节零 IO 不变量为终态承诺——存量 IO 通过 trait 化渐进收敛：文件系统操作经注入的 `FsProvider`、pty/进程经 `PtyProvider` / `ShellProvider`、定时与后台任务经注入的 clock/spawner；实现一律由接入层在组合根注入。终态以 `cargo tree -p mirri-core` 依赖白名单强制（禁 tokio fs feature / `reqwest` / `nix` / `rusqlite` 等 IO 型依赖）。**已弃置的替代路线**：豁免登记（承认 core 直接执行 IO、改写不变量以对齐现状）——否决理由：永久放弃可注入文件系统的演化能力（沙箱强化、虚拟文件系统、in-memory 测试替身），且文档不变量与代码事实长期背离会侵蚀其余契约的可信度。迁移约束：按模块分批、行为零变化（move-only）、优先级排在功能收口之后（最低）；迁移完成前冻结增量——新增 IO 一律走 trait 或 `mirri-os`。执行切分与验收标准见 [plans/07-followup-completion.md](plans/07-followup-completion.md) T13；存量现状盘点见 [context.md](context.md) §3。

零 IO 的强制执行手段：

- CI 中对 `mirri-core` 的 Cargo.toml 做依赖白名单检查（禁止 `reqwest`、`tungstenite`、`rusqlite`、`nix` 等 IO 型依赖直接出现）
- `mirri-core` 的单元测试禁止触碰真实文件/网络；文件类用例一律走 `mirri-os` 的 in-memory test double 或 `tempfile`（仅 dev-dependencies）

**零 IO 迁移清单**（core 中每个触碰 IO 的模块及其迁移路径）：

| 现状模块（`mirri-core` 内） | 触碰的 IO | 迁移路径 |
|---------------------------|----------|---------|
| `terminal_pool` | pty / 伪终端、子进程 | 抽成 `mirri-os::PtyProvider` trait（spawn / resize / read / write / exit 事件）；trait 本身零 IO，实现由接入层在组合根注入 `Scope` |
| `validate_workspace_path` | 文件系统存在性 / 边界校验 | 变为 `mirri-os` 的 `PathCodec` / `WorkspaceBoundary` trait 方法，core 只依赖 trait |
| `session_export` | 文件打包（zip）/ 写出 | 文件 IO 委托给注入的 `Persistence` trait（`mirri-persistence` 实现），core 只组装导出数据 |
| 其余直接 fs/process 依赖 | 文件读写、子进程、glob | 逐一 trait 化（`ShellProvider`、`FsWatcher` 等既有 trait 兜底）并改为组合根注入 |

**CI 门禁**（与上文依赖白名单检查合并为一条零 IO 检查）：

- `cargo tree -p mirri-core` 的结果中不得出现 `tokio` 的 `fs` 特性（`tokio::fs`）、`reqwest`、`rusqlite`、`nix` 等 IO 型依赖
- 源码级断言：`mirri-core` 内禁止直接出现 `std::process::Command`、`tokio::fs` 符号（CI grep / 自定义 lint 强制执行）

### 1.3 Transport 通过 trait 抽象

所有跨进程交互（正向 RPC / 事件流 / 反向 RPC）只依赖两个 trait：

- `Transport` —— 双向帧管道（`send` / `recv` / `close`），定义于 `mirri-transport`
- `EngineAdapter` —— 引擎接入面（`call_method` / `subscribe_events` / `on_reverse_rpc`），定义于 `mirri-core::adapter`（core 只声明接入语义，不感知具体传输）

WS、stdio、FFI 只是 `Transport` 的三个实现；Remote、Embedded、InProcess 只是 `EngineAdapter` 的三个实现（实现由 `mirri-remote` / `mirri-embedded` 提供，见第 4 节）。**协议层、core 业务逻辑、前端代码都不感知具体传输**。

### 1.4 前端 crate 组合 core + transport，不重写逻辑

`mirri-acp`、`mirri-node`、`mirri-server`、`mirri-cli` 是**组装层（composition root）**：它们只做三件事——

1. 构造 `mirri-core` 的 scope（DI 容器）
2. 选择 `Transport` / `EngineAdapter` 实现
3. 拉起进程/库的生命周期（shutdown、信号处理）

组装层禁止包含业务规则；任何「如果 session 没有 workspace 就 …」的分支都必须下沉到 `mirri-core`。

### 1.5 无循环依赖

依赖图必须是有向无环图（DAG）。规则：

- 依赖方向永远从「靠近用户」指向「靠近引擎」：`frontend → transport → core → proto/os`
- `mirri-core` 是依赖图的**汇点之下的一层**：只有它依赖 `mirri-proto` / `mirri-os` / `mirri-persistence` / `mirri-mcp`，这四个 crate 之间互不依赖
- `mirri-proto`、`mirri-os` 位于图的叶子，**不依赖任何 mirri crate**
- CI 用 `cargo machete`（未声明依赖）+ 自定义依赖方向检查（或用 `cargo lint` / CI 脚本校验 `cargo tree` 无反向边）阻止回归

### 1.6 现状与目标结构的差异

当前 `rust/crates/` 有 **17 个 crate**，修订后的目标结构即 17-crate 现实（核心依赖图 13 个 + 外围 4 个，见 2.2）；原「13 个 → 11 个」的收敛目标已废弃。逐 crate 的差异与现实状态表见 [context.md](context.md) §3（参考类，带快照日期）。

目标依赖图（第 2 节）画核心 13 个 crate；`mirri-kosong`、`mirri-auth`、`mirri-collaboration`、`mirri-plugin-host` 作为外围 crate 显式装配（并入决策待 C6/N2）。

## 2. Crate 依赖图

### 2.1 核心依赖图

```
mirri-cli ──→ mirri-server ──→ mirri-transport ──→ mirri-core
                                      ↑
mirri-acp ──→ mirri-transport ────────┘
                                      ↑
mirri-node ──→ mirri-transport ───────┘
mirri-acp / mirri-node ──→ mirri-embedded（EngineAdapter：EmbeddedEngine，src/embedded_server/）
mirri-acp / mirri-node ──→ mirri-remote（EngineAdapter：RemoteEngine / 帧适配，src/frames.rs）
mirri-embedded ──→ mirri-core · mirri-transport
mirri-remote  ──→ mirri-transport · mirri-proto
                mirri-core ──→ mirri-proto
                mirri-core ──→ mirri-os
                mirri-core ──→ mirri-persistence
                mirri-core ──→ mirri-mcp
```

### 2.2 完整依赖图（17 crates）

```
                     ┌───────────────────────────────────────────┐
                     │                前端 / 宿主层                 │
                     │                                            │
                     │  mirri-cli      mirri-acp      mirri-node  │
                     └──────┬────────────┬─────────────┬─────────┘
                            │            │             │
                     ┌──────▼────────────▼─────────────▼─────────┐
                     │              mirri-server                  │  (HTTP/WS/IPC 监听)
                     └──────────────────┬─────────────────────────┘
                                        │
                     ┌──────────────────▼─────────────────────────┐
                     │             mirri-transport                 │  (Transport trait + WS/stdio/NAPI 实现
                     │                                            │   + JSON-RPC codec)
                     └──────────────────┬─────────────────────────┘
                                        │
                     ┌──────────────────▼─────────────────────────┐
                     │  mirri-embedded · mirri-remote              │  (EngineAdapter 实现：
                     │                                            │   EmbeddedEngine @ embedded_server/、
                     └──────────────────┬─────────────────────────┘   RemoteEngine @ frames.rs)
                                        │
                     ┌──────────────────▼─────────────────────────┐
                     │                mirri-core                   │  (引擎域逻辑，零 IO)
                     └───┬───────────┬───────────┬──────────┬─────┘
                         │           │           │          │
                 ┌───────▼───┐ ┌─────▼─────┐ ┌───▼────────┐ ┌▼────────┐
                 │mirri-proto│ │  mirri-os │ │mirri-persistence│mirri-mcp│
                 └───────────┘ └───────────┘  └────────────┘  └─────────┘
                        (wire 类型)   (OS trait + 实现)   (append-log)  (MCP client)

mirri-test-harness ──→ mirri-server · mirri-transport · mirri-core · mirri-embedded · mirri-proto
                       (黑盒测试：起 server / 复用 client / EmbeddedEngine / wire 类型)
```

**外围 / 待决策 crate**（不进入核心依赖图主体；`mirri-auth` 由接入层显式装配，`mirri-collaboration` / `mirri-plugin-host` 并入决策待 C6/N2）：

```
mirri-cli ─────→ mirri-auth          （`mirri login`，OAuth Device Code / RFC 8628）
mirri-server ──→ mirri-auth          （auth middleware）
mirri-kosong ──→ 实现 mirri-core 的 Llm trait（provider 装配于 mirri-embedded/src/providers/，由接入层灌入 Scope）
mirri-collaboration → mirri-core      （独立 crate；并入 core::swarm/subagent 未执行，待 C6/N2 决策）
mirri-plugin-host  → mirri-embedded / mirri-node   （独立 crate；并入 core::hook 未执行，待 C6/N2 决策）
```

> 注：共 **17 个 crate** = 核心依赖图 **13 个**（cli / acp / node / server / transport / embedded / remote / core / proto / os / persistence / mcp / test-harness）+ 外围 **4 个**（`mirri-auth`、`mirri-kosong`、`mirri-collaboration`、`mirri-plugin-host`）；外围 crate 不参与 core 的零 IO 依赖面。

### 2.3 依赖方向规则（可机械检查）

1. `mirri-proto`、`mirri-os`：叶子，不依赖任何 mirri crate
2. `mirri-core`：只依赖 `proto` / `os`（仅 trait）/ `persistence` / `mcp`（及 LLM provider trait）
3. `mirri-transport`：依赖 `core`（`EngineAdapter` trait，仅 trait）+ `proto`（+ 外部 ws/io 库）；`EngineAdapter` 实现位于 `mirri-embedded` / `mirri-remote`
4. `mirri-server` / `mirri-acp` / `mirri-node`：依赖 `transport` + `core`（+ `os` 实现）；`mirri-acp` / `mirri-node` 另依赖 `mirri-embedded`（EmbeddedEngine）与 `mirri-remote`（帧适配）
5. `mirri-cli`：依赖 `server` / `acp` / `auth`（`mirri login`）
6. `mirri-test-harness`：只出现在各 crate 的 `dev-dependencies`，永远不被生产代码依赖

检查工具：CI 步骤 `cargo tree --edges normal` 断言第 1–5 条；`cargo machete` 断言无幽灵依赖；`cargo udeps` 断言无冗余依赖。

## 3. 各 Crate 职责

### 3.1 `mirri-core` —— 引擎核心（域逻辑，零 IO）

| | |
|---|---|
| **依赖** | `mirri-proto`（wire 类型）、`mirri-os`（仅 trait）、`mirri-persistence`（仅纯接口）、`mirri-mcp`（client trait） |
| **被依赖** | `mirri-transport`、`mirri-server`、`mirri-acp`、`mirri-node`、`mirri-test-harness`（dev） |

**职责**：Session 生命周期、Agent Loop、Tool 注册与执行、Permission 决策、Hook/Skill/Plan/Goal 引擎、Swarm 协作、Profile/Config 管理、Background Task、Telemetry。详见第 5 节模块结构。

**导出（public API surface）**：

- `EngineHandle` / `SessionManager` —— `create_session` / `attach_scope` / `destroy_session`
- 域类型全集：`SessionMetadata`、`AgentConfig`、`ToolDefinition`、`ToolResult`、`PermissionPolicy`、`HookDefinition`、`SkillDefinition`、`PlanItem`、`GoalState`、`AgentProfile`、`ServerConfig`
- 服务 trait：`Llm`、`McpClientFactory`、`BackgroundTaskExecutor`、`TelemetrySink`
- 接入面 trait：`EngineAdapter` / `ReverseRpcHandler` / `RpcNotification`（`adapter` 模块，见 4.3；实现由 `mirri-transport` 提供）
- `Scope`（DI 容器）：`Scope::new()` / `scope.resolve::<T>()` / test 用 `Scope::mock()`
- 错误体系：`EngineError`、`ErrorCode`、`EngineResult`（见 5.2 error）

**不导出**：

- **任何 IO 类型**：`std::net`、`tokio::net`、`std::fs` 不得出现在 public 签名中
- **具体 OS 实现**：不直接 `use` 平台相关代码；路径一律走 `PathCodec`
- **transport/server 概念**：`Transport`、`JsonRpcFrame` 属于 `mirri-transport`；`EngineAdapter` trait 定义于 `mirri-core::adapter`（异步 + 流式 API，见 4.3），core 不感知任何具体传输实现
- 任何网络/协议序列化细节

**测试策略**：

- 每个模块 `#[cfg(test)]` 单元测试 + `proptest`（状态机类：session 生命周期、tool 并发、plan/goal 状态迁移）
- 域逻辑测试用 `InProcessEngine` + in-memory `mirri-os` double，不建 TCP 端口
- 既有测试在迁移中按模块搬迁保留，作为回归基线（当前数字以 CI 为准）

### 3.2 `mirri-proto` —— Wire 协议定义

| | |
|---|---|
| **依赖** | `serde`、`serde_json`、`schemars`、`chrono`、`uuid` |
| **被依赖** | 除 `mirri-os` 外的所有 crate |

**职责**：wire.jsonl 的**唯一权威类型定义**：wire op 枚举、事件、状态快照、JSON Schema 生成、reducer（op 序列 → 状态）。

**导出**：

- `WireOp`（全部 op 变体）、`WireEvent`、`WireState`
- `schema::build_schema()` —— 由 `schemars`/`jsonschema` 生成并校验的协议 JSON Schema
- `reducer::apply(state, op) -> state` —— 纯函数，无 IO，conformance 测试的核心
- 类型版本标记：`WireVersion`、`OP_NAMESPACE`

**不导出**：帧编解码（属 `mirri-transport`）、存储（属 `mirri-persistence`）、任何 IO。

**测试策略**：op 序列化 round-trip（`serde_json` 对称性）；schema 示例校验（`jsonschema` crate）；reducer 的属性测试（任意合法 op 序列 → 状态不变量）。

### 3.3 `mirri-transport` —— 传输抽象（新 crate）

| | |
|---|---|
| **依赖** | `mirri-core`（`EngineAdapter` trait）、`mirri-proto`、`tokio`、`tokio-tungstenite`、`serde_json` |
| **被依赖** | `mirri-server`、`mirri-acp`、`mirri-node`、`mirri-test-harness` |

**职责**：协议之上的管道抽象。定义 `Transport` 核心 trait，以及 JSON-RPC 2.0 帧编解码（`JsonRpcCodec`）与三种传输实现（WS / stdio / NAPI）。`EngineAdapter` trait 定义于 `mirri-core::adapter`，其实现位于 `mirri-embedded`（EmbeddedEngine，`src/embedded_server/`）与 `mirri-remote`（RemoteEngine / 帧适配，`src/frames.rs`），见第 4 节。

**导出**：

- `Transport`、`JsonRpcFrame`（Request / Response / Notification / Event 四类）
- `JsonRpcCodec`（encode/decode，含 stdio LDJSON 分帧、WS text frame 分帧）
- `WsTransport`（客户端：`WsTransport::connect(url, headers)`；服务端会话：`WsSessionTransport`）
- `StdioTransport`（`StdioTransport::spawn(command, args)` 或绑定 stdin/stdout）
- `NapiTransport`（FFI 用，见 4.4）
- `EngineAdapter` 实现（trait 定义于 `mirri-core::adapter`，见 4.5）：`EmbeddedEngine` @ `mirri-embedded`（`src/embedded_server/`）、`RemoteEngine` @ `mirri-remote`（`src/frames.rs`）；`InProcessEngine` 为测试形态（`mirri-test-harness` / core 内测，无独立实现 crate）
- `EventStream`（`ReceiverStream` 包装 `subscribe_events` 的 `mpsc::Receiver` 为 `futures::Stream`）

**不导出**：具体 HTTP server 逻辑、TLS 证书管理、业务方法目录（方法名只在 `mirri-core` 的注册表中出现）。

**测试策略**：

- codec 单元测试：非法帧、超大帧（上限）、分帧边界（跨 chunk 的 JSON）
- `StdioTransport` ↔ `StdioTransport` 回环测试（spawn 一个 echo 进程）
- `WsTransport` 用 `tokio-tungstenite` 的 in-memory 双端测试
- 全部集成在 `mirri-test-harness` 层再做跨进程验证

### 3.4 `mirri-server` —— HTTP + WS + IPC 服务端

| | |
|---|---|
| **依赖** | `mirri-core`、`mirri-transport`、`mirri-os`、`mirri-proto`、`mirri-persistence`（session 恢复）、`mirri-auth`（middleware） |
| **被依赖** | `mirri-cli`、`mirri-test-harness`（dev） |

**职责**：监听层。把「核心引擎」暴露给远程前端：

- `GET /healthz` · `GET /meta` · `POST /shutdown`（REST 仅保留这三个端点）
- `WS /api/v1/rpc` —— JSON-RPC 2.0 双向连接：每连接一个 `WsSessionTransport`，server 侧用 `EngineAdapter`（`CoreAdapter` 实现，直接调度 core）
- IPC socket 监听（macOS/Linux domain socket，Windows named pipe），复用同一协议
- Auth middleware（token 校验，接入 `mirri-auth`）

**导出**：`ServerConfig`、`serve(config, scope) -> ServeHandle`（`ServeHandle::shutdown()`）、`accept_ws`。

**不导出**：协议类型（来自 `mirri-proto`）、业务逻辑（全部委托 core）、静态资源托管。

**测试策略**：`mirri-test-harness` 黑盒测试为主（起真实 server + 真实 client）；auth 用 mock token；shutdown 幂等性测试（重复 POST /shutdown）。

### 3.5 `mirri-os` —— OS 抽象层（trait + 实现两层）

| | |
|---|---|
| **依赖** | 无 mirri 依赖（叶子；外部依赖 `nix` 0.29、`portable-pty` 0.9） |
| **被依赖** | `mirri-core`（trait-only）、`mirri-server`、`mirri-acp`、`mirri-node`、`mirri-test-harness`（dev） |

**职责**：把 OS 差异收口为 trait + 实现两层（详见 [06-cross-platform](06-cross-platform.md)）。

**现有模块（2026-09-04）**：`fs` / `glob` / `process` / `pty` / `search` / `path_access`
（process 已实现分级 kill 语义；pty 已具备底层能力，待收敛为 `PtyProvider` trait；trait 化路径见 06 §1.4）。

**规划模块（待实施）**：

- `PathCodec` —— POSIX 风格 wire 路径 ↔ 各 OS 原生路径互转（`src/path_codec.rs`）
- `ShellProvider` —— 统一 shell 语义（命令构造、环境、退出码）（`src/shell.rs`）
- `FsWatcher` —— 目录/文件监听（`notify` 封装，事件归一化）（`src/fs_watcher.rs`）
- `IpcTransport` —— domain socket / named pipe 抽象（`mirri-server` 与 `mirri-node` 共用）（`src/ipc.rs`；注意 `mirri-server/src/ipc.rs` 已存在，为服务端监听面）

**导出**：trait 定义（`cfg` 无关，所有平台可编译）+ 按 OS 的实现：

- feature `unix`（`cfg(unix)`：POSIX 路径、`nix` 进程控制）
- feature `windows`（`cfg(windows)`：`\\?\` 路径处理、named pipe）
- 所有实现均为 `#[cfg(target_os)]` 门控，**trait 面跨平台一致**

**不导出**：任何业务类型；不依赖 `mirri-proto`。

**测试策略**：trait 契约测试（同一套测试 body 在 mock impl 与真实 impl 上各跑一遍，用 `#[cfg_attr]` 宏或 trait 泛型测试）；路径编解码的性质测试（往返不变量：`decode(encode(p)) == p`，Windows 上含大小写/短路径/UNC 用例）；FSWatcher 用 `tempfile` 事件序列断言。

### 3.6 `mirri-persistence` —— Append-log 存储

| | |
|---|---|
| **依赖** | `mirri-proto`、`rusqlite`（session 索引） |
| **被依赖** | `mirri-core`、`mirri-server` |

**职责**：wire.jsonl 追加日志（append-only、原子写、崩溃恢复）、按 session 的 wire 回放（replay）、session 导出（zip 打包）、持久化事件日志（见 3.6.1）。

**导出**：`AppendLog`（`append(op)` / `read_from(offset)` / `fsync`）、`EventLog`（见 3.6.1）、`SessionStore`（session 元数据索引）、`SessionExporter`。

**不导出**：文件格式定义（属 `mirri-proto`）；不依赖 `mirri-core`（避免反向依赖）。

**测试策略**：崩溃恢复（写一半的文件 → 截断到最后一个合法 op）；追加并发（多 writer 序列化）；回放正确性（`replay == reducer::apply` 全序列）；golden 文件对比。

#### 3.6.1 持久化事件日志（durable event log）

为事件流提供可回放、可恢复的持久化：每个 session 一条 **append-only** 事件日志。

- **seq 分配**：`append(event) -> seq` 为每条事件分配**单调递增**的 `seq`（session 内唯一）；每 session **单 writer**（同一时刻只有一个写者），无并发竞争、不需要锁
- **epoch 语义**：`epoch` 记录「服务器启动 / 日志轮转 / session 恢复」的代数，用于区分日志的世系
- **游标回放**：`read_from(seq)` 从任意 `seq` 开始回放——断线重连、晚到订阅（订阅时携带游标）用它补齐缺失事件
- **物理格式**：`wire.jsonl` 的扩展——每行事件追加 `seq` / `epoch` 字段（如 `{"seq": 42, "epoch": 3, "event": {...}}`），旧行 / 旧 epoch 仍可读取
- **缓冲管理**：内存 ring buffer（容量可配置，默认 **1024** 条）承接热路径写入；溢出后落盘（磁盘回退）；`read_from` 越过 ring 起点时从磁盘读
- **epoch 生命周期**：epoch 持久化在 **session 元数据文件**（`SessionStore`）中；服务器重启 → epoch+1 → 触发日志轮转（新文件 / 新截断点）

**导出**：`EventLog`（`append` / `read_from(seq)` / `rotate`）、`EventCursor`。

**测试策略**：seq 单调性与单 writer 不变量（`proptest`）；ring buffer 溢出边界（跨 ring / 磁盘衔接的 `read_from` 回放）；epoch 递增后旧文件回放仍可用；崩溃时未 fsync 的尾部事件不产生半条记录（复用 3.6 崩溃恢复用例）。

### 3.7 `mirri-mcp` —— MCP Client

| | |
|---|---|
| **依赖** | `serde_json`、`tokio`、`reqwest`（SSE） |
| **被依赖** | `mirri-core`（client trait）、`mirri-test-harness`（dev） |

**职责**：MCP（Model Context Protocol）客户端：stdio / SSE 两种传输、tool discovery（`tools/list`）、server 生命周期（spawn/健康检查/优雅关闭）、capability 协商。

**导出**：`McpClient`、`McpTransport`（stdio/SSE 二实现）、`McpServerHandle`（生命周期）、`McpToolSpec`。

**不导出**：MCP server 实现（本产品是 MCP 的 client，不做 host 之外的 server 面）；业务工具逻辑。

**测试策略**：与官方/本地 mock MCP server 的集成测试（spawn 一个 echo server）；传输断线重连；工具发现结果 schema 校验。

### 3.8 `mirri-acp` —— ACP Host（新 crate）

| | |
|---|---|
| **依赖** | `mirri-core`（嵌入引擎）、`mirri-transport`（`StdioTransport`）、`mirri-embedded`（`EmbeddedEngine`，`src/embedded_server/`）、`mirri-remote`（帧适配）、`mirri-os`（实现层） |
| **被依赖** | `mirri-cli`（`mirri acp` 子命令复用其库面）、`mirri-test-harness`（dev） |

**职责**：ACP（Agent Client Protocol）宿主二进制。stdio 上跑 JSON-RPC 2.0，引擎以 `EmbeddedEngine` 方式内嵌（同进程，零网络开销）。反向 RPC 桥：`approval.ask` / `question.ask` / `fs.read` / `fs.write` / `terminal.*` 经 stdio 转发给 IDE 宿主，宿主响应回到引擎。

**导出**（lib + bin 双 target）：`run_acp_stdio(config, scope) -> io::Result<()>`（供 cli 复用）、`AcpProtocolHandler`（帧分发：forward RPC / 事件流 / reverse RPC 三向路由）、`AcpReverseBridge`。

**不导出**：IDE 适配层（协议是宿主的中立 ACP 规范）；不依赖 `mirri-server`（无网络面）。

**测试策略**：`mirri-test-harness` 用 `StdioTransport::spawn(acp_binary)` 做全协议黑盒测试；reverse RPC 回环（mock IDE 宿主应答 approval/question）；ACP SDK conformance 测试（Phase 3 验收项）。

### 3.9 `mirri-node` —— NAPI Binding（新 crate）

| | |
|---|---|
| **依赖** | `mirri-core`（嵌入引擎）、`mirri-transport`（`NapiTransport`）、`mirri-embedded`（`EmbeddedEngine`，`src/embedded_server/`）、`mirri-remote`（帧适配）、`mirri-os`（实现层）、`napi` / `napi-derive` |
| **被依赖** | `mirri-test-harness`（dev） |

**职责**：TUI 的 Node.js 原生绑定。`cdylib` 产物（`.node`）被 `apps/mirri-code` 直接 `require()`，引擎以 `EmbeddedEngine` 内嵌在 Node 进程内（零拷贝 FFI、sub-100ms 交互）。

**导出**（NAPI surface，JS 侧）：

- `createEngine(config) -> EngineHandle`、`engine.call(method, params) -> Promise<Value>`、`engine.close()`
- 事件回调桥：`engine.on('event', cb)`、`engine.on('reverse-rpc', async (req) => resp)`（async 回调在 Node 线程池跑，NAPI `napi_create_async_work`）
- 能力探测：`engine.platform()` / `engine.version()`

**不导出**：Rust 内部类型直接跨 FFI 泄漏（全部经 `serde_json::Value` 序列化边界）；不暴露任何 `unsafe` 面给 JS。

**构建系统**：`napi-derive` + `@napi-rs/cli`，`napi build --platform --release` 产出各平台 `.node`（见 7.2）。

**测试策略**：Rust 侧单元测试（NapiTransport 编解码）；JS 侧集成测试（Node test runner + `node:test`，或 vitest）：`createEngine → call → event` 全链路，进程内跑真实 core；`miri` + sanitizer 跑 FFI 边界；内存泄漏检查（反复 create/close 后 heap 平稳）。

### 3.10 `mirri-cli` —— CLI 入口

| | |
|---|---|
| **依赖** | `mirri-server`（`mirri serve`）、`mirri-acp`（`mirri acp`）、`mirri-auth`（`mirri login`）、`mirri-persistence`（`mirri migrate`） |
| **被依赖** | 无（最顶层） |

**职责**：`mirri` 二进制的参数解析与子命令调度（`clap`）：

- `mirri serve` —— 启动 WS/HTTP server（`mirri-server::serve`）
- `mirri acp` —— 启动 ACP 模式（`mirri-acp::run_acp_stdio`）
- `mirri login` —— OAuth Device Code 登录（token 落盘）
- `mirri migrate` —— wire.jsonl 版本迁移
- `mirri doctor` —— 环境自检（可选）

同时导出 `Cli` 参数类型（lib + bin 双 target），供集成测试复用。

**不导出**：任何引擎/协议逻辑（纯 shell 层）。

**测试策略**：`tests/cli_tests.rs` 用 `assert_cmd` 黑盒断言（子命令、退出码、输出格式）；`mirri acp` 子命令复用 ACP 测试套件。

### 3.11 `mirri-test-harness` —— 跨 Server 一致性测试

| | |
|---|---|
| **依赖** | `mirri-server`、`mirri-transport`、`mirri-core`、`mirri-proto`（仅 dev 用途） |
| **被依赖** | 仅各 crate 的 `dev-dependencies` |

**职责**：同一套测试套件对「任何讲 Mirri wire 协议的服务」跑黑盒测试 —— Rust `mirri-server`、TS 旧 server（`kap-server`）、`mirri-acp` binary、NAPI 内嵌引擎：

- `HttpClient` / `WsClient` / `IpcClient` —— 三个协议客户端
- `MockLlm` —— 确定性 LLM 替身（side-effect 注入、回放脚本）
- `EngineUnderTest` —— 被测引擎统一句柄（把 server / acp / napi / in-process 四种形态统一成同一个测试接口）
- Conformance suite —— 协议方法目录全量（正向 RPC / 事件流 / 反向 RPC）
- Snapshot 比较 —— wire.jsonl golden 文件 + `reducer` 状态快照对比

**不导出**：`harness::conformance` 套件之外的一切；它的核心价值是**作为库被各 crate 的 dev-dependencies 引用**，而非独立运行的二进制（同时提供 `cargo test -p mirri-test-harness` 一键全跑）。

**测试策略**：见第 8 节。

## 4. Transport Trait 设计

> 以下为 schema 定义（示意签名），非最终实现代码。`EngineAdapter` trait 定义于 `mirri-core::adapter`，采用 **async fn + 流（stream）** API（见 4.3）；`Transport` trait 定义于 `mirri-transport`。真实实现为 `async fn`（或基于 `tokio::sync::mpsc` + 专用线程的阻塞封装），同步/异步形态由实现方内部收敛，业务代码不感知。

### 4.1 `JsonRpcFrame` 与帧编解码

```
// 四个帧类别，与 01-protocol-unification 保持一致
type JsonRpcFrame =
  | Request     { id, method, params }            // 正向 RPC
  | Response    { id, result | error }            // 正向 RPC 应答
  | Notification{ method: "event.*", params }     // 事件流（Server → Client）
  | ReverseReq  { id, method: "reverse.*", params }// 反向 RPC（Server → Client，需应答）
```

- 同一连接上三类流量按帧类型复用，`id` 唯一性由发送方保证
- `JsonRpcCodec` 负责 encode/decode：
  - **stdio**：单行 JSON（LDJSON），`\n` 分帧，单帧上限 16 MiB，超限报 `FRAME_TOO_LARGE`
  - **WS**：text frame，一帧一消息
  - **NAPI**：直接 `Buffer`/`Value` 传参，无物理分帧，逻辑帧仍为完整 JSON 文本（保证三端线格式一致）

### 4.2 `Transport` trait

```
trait Transport: Send + Sync {
    fn send(&self, frame: JsonRpcFrame) -> Result<()>;
    fn recv(&self) -> Result<JsonRpcFrame>;
    fn close(&self) -> Result<()>;
}
```

语义契约：

- `send` 全量投递或返回错误（内部用有界 channel + 专用 IO 线程；对 IO 线程的阻塞错误做通道关闭传播）
- **双向**：一端是发起方（client），另一端是响应方（server）；`recv` 同时承载正向应答与事件/反向请求（发送方语义见协议文档）
- `close` 幂等：重复调用返回 `Ok(())`；close 后 `send` 返回 `TransportClosed`。
- 背压：`recv` 侧消费慢时，内部队列有界（默认 1024 帧），溢出时发送侧报 `Backpressure` 错误
- 帧保序：同一方向上的帧严格 FIFO；`close` 会先冲刷已入队帧再关闭

### 4.3 `EngineAdapter` trait

> 定义于 `mirri-core::adapter`，采用 **异步 + 流式** API；`mirri-transport` 提供其三实现（Remote / Embedded / InProcess，见 4.5）。

```
trait EngineAdapter: Send + Sync {
    /// 正向 RPC：调用引擎方法并等待应答
    async fn call_method(&self, method: &str, params: Value) -> Result<Value>;
    /// 事件流：返回有界 mpsc channel 的接收端，消费慢即形成自然背压
    fn subscribe_events(&self) -> mpsc::Receiver<RpcNotification>;
    /// 反向 RPC：注册宿主能力回调（Arc 化闭包、返回 Future，不阻塞引擎线程）
    fn on_reverse_rpc(&self, handler: ReverseRpcHandler);
}

// ReverseRpcHandler：Arc 化的异步闭包（Arc'd closure returning a Future）
type ReverseRpcHandler = Arc<
    dyn Fn(ReverseRequest) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>
        + Send + Sync
>;
```

语义：

- `call_method`：async 正向 RPC——`await` 引擎应答（远程 = 一条 Request/Response 往返；内嵌 = 直接调用 scope 内注册的 handler；FFI = 等待 Node 线程池回调结果），不阻塞引擎线程
- `subscribe_events`：引擎 → 前端的单向**事件流**（`assistant.delta` / `tool.result` / `turn.ended` / `goal.*` …）；返回 `mpsc::Receiver<RpcNotification>`，消费者按自身节奏拉取，有界 channel（默认容量 1024）在消费慢时形成**自然背压**；`ReceiverStream` 可直接作为 `futures::Stream` 使用
- `on_reverse_rpc`：引擎需要宿主能力的回调（`approval.ask` / `question.ask` / `fs.read` / `fs.write` / `terminal.*`）；handler 是 **Arc 化的闭包、返回 `Future`**（远程经 transport 往返，内嵌经 channel），悬挂请求不会阻塞引擎线程

### 4.4 三种 `Transport` 实现

| 实现 | 方向 | 帧承载 | 关键点 |
|------|------|--------|--------|
| `WsTransport` | client（`connect`）/ server 会话（`accept` 后包装） | WS text frame | `tokio-tungstenite`；握手携带 token header；连接断开 → `TransportClosed` → 上层自动重连（指数退避） |
| `StdioTransport` | 子进程 or 被宿主 spawn | LDJSON，`\n` 分帧 | `StdioTransport::spawn` 管理子进程 stdin/stdout + 退出码监控；被 spawn 侧绑定 `stdin`/`stdout` 句柄；stderr 留给日志 |
| `NapiTransport` | Node ↔ Rust（同进程） | 无物理分帧 | 经 NAPI 的 `Buffer` / `Function` 回调；`send` 即 `napi_call_function`（释放 JS 线程，避免死锁），`recv` 由事件循环 push |

统一不变量：**三端线格式一致**（decode 后的 `JsonRpcFrame` 逐字节等价），这正是 cross-engine conformance 测试能跑通的前提。

### 4.5 三种 `EngineAdapter` 实现（实现位置）

trait 定义于 `mirri-core::adapter`；实现不再由 `mirri-transport` 承载，分布在下述 crate：

| 实现 | 承载 | 实现位置 | 适用 |
|------|------|---------|------|
| `RemoteEngine` | 包一个 `Transport`（WS 客户端侧） | `mirri-remote`（`src/frames.rs`） | Web 前端 → `mirri-server`；`call_method` 走网络，事件由 recv 泵推入 `subscribe_events` 的 channel，反向 RPC 分发到注册 handler |
| `EmbeddedEngine` | 直接持 `mirri-core` 的 `Scope` | `mirri-embedded`（`src/embedded_server/`） | `mirri-acp`（stdio 宿主）、`mirri-node`（NAPI 宿主）；`call_method` 直接调 scope 内 handler，事件经 `subscribe_events` 的有界 channel 推送，反向 RPC 经桥回调宿主 |
| `InProcessEngine` | 与 `EmbeddedEngine` 同构，但零成本、无进程边界语义简化 | 测试形态，无独立实现 crate（`mirri-test-harness` 复用 embedded 面或 core 内测） | 单元测试、`mirri-test-harness` 的 in-process 形态、CLI 内嵌模式 |

`mirri-server` 的 server 侧等价物：`CoreAdapter`（实现 `mirri-core::adapter` 的 `EngineAdapter` trait，直接调度 core），与 `RemoteEngine` 呈镜像关系 —— 二者共享 `mirri-core` 注册的方法目录作为唯一事实源。

### 4.6 传输 × 适配器组合矩阵

| 前端 | Transport | EngineAdapter（实现位） | 延迟特征 |
|------|-----------|------------------------|---------|
| TUI（Node） | `NapiTransport` | `EmbeddedEngine`（`mirri-embedded`） | 零网络开销 |
| Web（浏览器） | WS（浏览器侧）→ `WsTransport`（server 侧会话） | `RemoteEngine`（`mirri-remote`）↔ `CoreAdapter` | localhost RTT |
| ACP（Zed/JetBrains） | `StdioTransport` | `EmbeddedEngine`（`mirri-embedded`） | 零网络开销 |
| 测试 | `StdioTransport` / `WsTransport` / in-memory | `InProcessEngine` / `RemoteEngine`（`mirri-remote`） | 视形态 |

## 5. `mirri-core` 内部模块结构

### 5.1 模块总览

```
mirri-core/src/
├── lib.rs            # crate 根：pub use 收敛、引擎门面（EngineHandle）
├── error/            # 错误体系（EngineError / ErrorCode / 分类）
├── scope/            # DI 容器（WorkspaceScope / SessionScope / AgentScope）
├── adapter/          # EngineAdapter trait · ReverseRpcHandler（接入面，零 IO）
│
├── session/          # SessionManager · SessionScope · SessionMetadata
├── agent/            # AgentLoop · AgentScope · AgentConfig
├── tool/             # ToolRegistry · ToolDefinition · ToolResult · path_security
├── permission/       # PermissionManager · PermissionPolicy · policies/
├── hook/             # HookEngine · HookDefinition · bridges/
├── skill/            # SkillEngine · SkillDefinition
├── plan/             # PlanEngine · PlanItem
├── goal/             # GoalEngine · GoalState
├── swarm/            # SwarmCoordinator（多 agent 编排）
├── subagent/         # SubAgent（子 agent 生命周期）
├── profile/          # ProfileManager · AgentProfile
├── config/           # ConfigManager · ConfigWatcher
├── background/       # BackgroundTaskManager
│
├── context/          # ContextMessage · projection（横切）
├── message/          # 消息与内容分片（横切）
├── llm/              # Llm trait · streaming 事件（横切）
└── telemetry/        # TelemetrySink · spans（横切）
```

> 迁移说明：目标目录名与现状源码的对应关系见 5.2 每模块的「现状」行。搬迁 = `git mv` + `mod` 调整，**不改变行为**，既有测试原样保留。

### 5.2 各模块职责

| 模块 | 核心类型 | 职责 | 现状对应（待搬迁/改名） |
|------|---------|------|------------------------|
| `session/` | `SessionManager`、`SessionScope`、`SessionMetadata` | session 创建/销毁/继承、上下文打包、会话级生命周期 | `session.rs`、`session_activity.rs` |
| `agent/` | `AgentLoop`、`AgentScope`、`AgentConfig` | 主循环：LLM 调用 ↔ 工具执行 ↔ 事件发布；turn 状态机 | `loop_engine/`、`turn_flow/`、`message_pipeline.rs` |
| `tool/` | `ToolRegistry`、`ToolDefinition`、`ToolResult`、`path_security` | 工具注册/发现/执行、参数校验、路径安全（沙箱外路径拒绝） | `tool_registry/`、`tool.rs`、`tools/`、`tool_result_truncation.rs`、`tool_select.rs`（路径安全逻辑自 `mirri-os::path_access` 上移） |
| `permission/` | `PermissionManager`、`PermissionPolicy`、`policies/` | 审批策略树（allow/deny/ask 三态）、policy 组合、与 reverse RPC 的 `approval.ask` 桥 | `permission/`、`tool_policy.rs` |
| `hook/` | `HookEngine`、`HookDefinition`、`bridges/` | 生命周期钩子（session/turn/tool 事件 → 外部动作）、插件桥 | `hook_engine/`、`hooks.rs`（`mirri-plugin-host` 并入未执行，仍为独立 crate，待 C6/N2 决策） |
| `skill/` | `SkillEngine`、`SkillDefinition` | `SKILL.md` 发现与注入 | `skill.rs` |
| `plan/` | `PlanEngine`、`PlanItem` | plan-mode：任务分解、进度跟踪、展开/收起 | `plan.rs` |
| `goal/` | `GoalEngine`、`GoalState` | 目标跟踪、完成判定、事件发布（`goal.*`） | `goal.rs` |
| `swarm/` | `SwarmCoordinator` | 多 agent 编排：子 agent 生成、委派、结果回收 | `swarm.rs`、`subagent.rs`（`mirri-collaboration` 并入未执行，仍为独立 crate，待 C6/N2 决策） |
| `subagent/` | `SubAgent` | 子 agent 生命周期：创建 / 运行 / 结果回收 | `subagent.rs`（规划承接 `mirri-collaboration`；并入未执行，待 C6/N2 决策） |
| `adapter/` | `EngineAdapter`、`ReverseRpcHandler`、`RpcNotification` | 引擎接入面 trait（异步 + 流式）：前端 / 宿主与引擎的统一调用面，零 IO；实现由 `mirri-embedded` / `mirri-remote` 提供（见 4.3、4.5） | 新模块（trait 定义自 `mirri-transport` 上移至 core） |
| `profile/` | `ProfileManager`、`AgentProfile` | agent 画像（名称/指令/温度…）、profile 切换 | `profile.rs` |
| `config/` | `ConfigManager`、`ConfigWatcher` | TOML 配置、env override、热重载（watcher 走 `mirri-os::FsWatcher` trait） | `config.rs`、`mirri-cli::watcher`（上移） |
| `background/` | `BackgroundTaskManager` | 会话内后台任务（含 cron 调度）、任务结果续接 | `task.rs`、`cron.rs` |
| `scope/` | `WorkspaceScope`、`SessionScope`、`AgentScope` | 三级 DI 容器：workspace 级单例 → session 级 → agent 轮次级；`resolve::<T>()` | `scope.rs` |
| `error/` | `EngineError`、`ErrorCode`、`ErrorKind` | 错误分类（validation / permission / io-abstraction / llm / internal）、错误码 → 协议 `error.code` 映射 | `error.rs` |

### 5.3 横切模块

不属于任何聚合根、但贯穿全部模块：

- `context/` —— `ContextMessage`、projection 选项、上下文快照（`context.rs`、`context_memory/`、`context_size.rs`、`compaction/`）
- `message/` —— wire 级 `Message` 与内容分片（`message.rs`、`origin.rs`）
- `llm/` —— `Llm` trait 与 streaming 事件（`llm.rs`；实现方在 `mirri-kosong`，provider 装配于 `mirri-embedded/src/providers/`，由接入层灌入 Scope）
- `telemetry/` —— `TelemetrySink`、tracing span、用量统计（`telemetry.rs`、`usage.rs`、`activity_view.rs`）

### 5.4 模块依赖规则

- scope 是唯一允许被所有模块依赖的「基础设施」模块；**业务模块之间只依赖公开类型，不互相 import 内部**（经 `lib.rs` 的 `pub use` 收敛）
- `error/` 被所有模块依赖，不依赖任何业务模块
- 模块内 `pub(crate)` 隔离：聚合根（session/agent/tool/…）的跨模块访问一律经 `scope::resolve`，编译期不出现直接类型互引（用 `cargo module` / rust-analyzer export 检查兜底）

## 6. Cargo.toml workspace 配置

### 6.1 workspace.package（统一元数据）

```toml
[workspace.package]
edition = "2024"
version = "0.1.0"          # 统一版本，见第 9 节
license = "Apache-2.0"
rust-version = "1.85"      # MSRV，随 edition 2024 工具链基线
```

所有 crate 一律 `edition.workspace = true`、`version.workspace = true`、`license.workspace = true`，**禁止单 crate 覆盖版本号**（保证 workspace 单版本发布，见 9.1）。

### 6.2 workspace.dependencies（共享依赖）

集中声明，crate 内用 `{ workspace = true }` 引用：

```toml
[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1.0.0-alpha.17"          # JSON Schema 生成
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
futures = "0.3"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
thiserror = "2"
anyhow = "1"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
sha2 = "0.10"
base64 = "0.22"
regex = "1"
globset = "0.4"
jsonschema = "0.26"
proptest = "1"                        # 仅 dev
tempfile = "3"                        # 仅 dev
pretty_assertions = "1"               # 仅 dev
# 接入层专用（不进 mirri-core / mirri-proto）
tokio-tungstenite = "0.24"            # mirri-transport / mirri-server
napi = { version = "3", features = ["napi8", "serde-json"] }   # mirri-node
napi-derive = "3"                     # mirri-node
rusqlite = { version = "0.32", features = ["bundled"] }        # mirri-persistence
reqwest = { version = "0.12", features = ["json", "stream"] }  # mirri-mcp / auth
nix = "0.29"                          # mirri-os(unix)
portable-pty = "0.9"                  # mirri-os(unix)
clap = { version = "4", features = ["derive"] }                # mirri-cli
```

分层规则：`mirri-proto` / `mirri-os` / `mirri-core` 的骨架依赖只有 serde/serde_json/thiserror/tracing/uuid/chrono 一级；网络/进程类依赖只允许出现在表中「接入层专用」以下。

### 6.3 features 规划

| Crate | Feature | 语义 |
|-------|---------|------|
| `mirri-os` | `unix` / `windows` | OS 实现门控（默认按 `cfg(target_os)` 自动启用对应项）；trait 面始终可用 |
| `mirri-transport` | `ws` / `stdio` / `napi` | 传输实现开关；`napi` 仅 `mirri-node` 启用 |
| `mirri-server` | `http` / `ws` / `ipc` | 监听面开关（`ipc` 依赖 `mirri-os::IpcTransport`） |
| `mirri-mcp` | `stdio` / `sse` | MCP 传输开关 |
| `mirri-core` | `default = []` | 保持最小；不设业务 feature（避免 feature 组合爆炸导致的测试矩阵膨胀） |
| `mirri-node` | 无 feature | `crate-type = ["cdylib", "lib"]`（cdylib 供 napi 产物，lib 供 Rust 侧测试） |

### 6.4 profiles

```toml
[profile.dev]
opt-level = 1                        # 开发期可调试 + 不太慢

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1                    # 发布构建：更优代码
strip = "symbols"                    # 二进制瘦身（debuginfo 单独产物）
panic = "unwind"

[profile.dist]                       # 对外发布专用（ACP/CLI 二进制）
inherits = "release"
lto = "fat"
debug = "line-tables-only"
```

- CI 一律用 `--profile release`（或 `dist`）构建产物做测试，避免 dev/release 语义漂移
- `[workspace.lints]`：`clippy::all = "warn"`、`rust-2018-idioms`、`unsafe-code = "deny"`（`mirri-node` 例外，见 8.4）

### 6.5 附加配置

- `default-members = ["crates/mirri-cli", "crates/mirri-acp", "crates/mirri-node", "crates/mirri-server"]` —— 日常 `cargo build` 只编译接入层
- `.cargo/config.toml`：交叉编译 target 别名（见 7.4）、`[target.'cfg(unix)']` linker 设置
- CI 质量门：`cargo fmt --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo machete` · `cargo deny check`（许可证/漏洞）

## 7. 构建与发布

### 7.1 构建命令总览

| 产物 | 命令 | 依赖 |
|------|------|------|
| 全部 crate（含测试） | `cargo build --workspace --all-targets` | 本机 toolchain |
| 全量测试 | `cargo test --workspace` | 本机 toolchain |
| `mirri` CLI 二进制 | `cargo build --release -p mirri-cli` | 本机 toolchain |
| `mirri-acp` 二进制 | `cargo build --release -p mirri-acp` | 本机 toolchain |
| `mirri-node` `.node` | `napi build --platform --release` | Node ≥ 20 + rust toolchain（见 7.2） |

### 7.2 `mirri-node` 构建（napi-rs）

- 源码侧：`#[napi]` 宏标注导出函数，`crate-type = ["cdylib", "lib"]`（cdylib 供 napi 产物，lib 供 Rust 侧测试）
- 命令：`napi build --platform --release`（`@napi-rs/cli`），产物按 `{platform}-{arch}.node` 命名（如 `darwin-arm64.node`）
- 每个平台单独跑一次构建：`x86_64-apple-darwin` / `aarch64-apple-darwin` / `x86_64-pc-windows-msvc` / `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`
- 发布形态：npm package 的 `optionalDependencies`（每个平台一个 `@mirri/node-{platform}-{arch}` 包指向对应 `.node`），主包 `@mirri/node` 按 `process.platform/arch` 解析加载
- 版本一致性：`.node` 产物版本必须与 npm 包版本一致（CI 校验）

### 7.3 `mirri-acp` / `mirri-cli` 二进制构建

- 单一自包含 binary，无动态链接依赖（Linux 用 `musl` target 静态链接，Windows 用 MSVC 运行时静态链接）
- 产物命名带平台后缀：`mirri-x86_64-unknown-linux-musl`、`mirri-aarch64-apple-darwin` …
- `mirri acp` 与 `mirri acp-standalone` 两种投放：CLI 内置子命令 + 独立 binary（IDE 插件直接下载独立 binary）

### 7.4 交叉编译矩阵

| Target | 工具链 | 备注 |
|--------|--------|------|
| `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` | `rustup target add`；CI Ubuntu runner | 发行用 musl 变体静态链接 |
| `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` | `cross` 或 zig cc | 静态 binary，供容器/服务器分发 |
| `x86_64-apple-darwin` / `aarch64-apple-darwin` | macOS runner 原生构建 | Apple Silicon 上交叉 x86_64 用 `rustup target add x86_64-apple-darwin` |
| `x86_64-pc-windows-msvc` | Windows runner（或 `cargo-xwin`） | 与 `napi build --platform` 的 Windows 产物同源 |

发布流水线（GitHub Actions 矩阵）：`matrix: [target] × [cli, acp, node]`，每格产出对应 artifact → 汇总到 release asset / npm registry。构建产物带 commit SHA 校验（`build.rs` 注入 `GIT_COMMIT` 到 `--version` 输出）。

### 7.5 发布顺序（依赖序）

1. `mirri-proto` / `mirri-os`（叶子，先行发布锁定协议）
2. `mirri-core` / `mirri-persistence` / `mirri-mcp` / `mirri-transport`
3. `mirri-server` / `mirri-acp` / `mirri-node`
4. `mirri-cli`、npm 包、`mirri-test-harness`（随 workspace 发布）

## 8. 测试策略

### 8.1 各 crate 单元测试（`#[cfg(test)]`，随 crate 内嵌）

- **`mirri-proto`**：序列化 round-trip、schema 校验、reducer 属性测试（`proptest`）
- **`mirri-core`**：模块级单元测试 + 状态机 `proptest`；`InProcessEngine` + in-memory platform double；不触文件/网络
- **`mirri-transport`**：codec 边界（非法帧/超长帧/分帧）、`StdioTransport` 回环、`WsTransport` in-memory 双端
- **`mirri-os`**：trait 契约测试（mock 与真实实现双跑）、路径编解码往返不变量、FSWatcher 事件序列
- **`mirri-persistence`**：append/崩溃恢复/回放正确性、golden 文件
- **`mirri-mcp`**：与 mock MCP server 的集成（spawn echo server）、传输断线
- **`mirri-server` / `mirri-acp` / `mirri-node` / `mirri-cli`**：组装层逻辑 + 冒烟 + 各自二进制/绑定形态的黑盒测试

### 8.2 `mirri-test-harness` 集成测试

- `HttpClient` / `WsClient` / `IpcClient` 三客户端，覆盖 `mirri-server` 的三种监听面
- `MockLlm` 注入确定性 LLM 行为（脚本化应答序列），保证测试可复现
- Snapshot 对比：wire.jsonl golden 文件 + `reducer::apply` 状态快照，协议变更时先更新 golden 再合代码（CI 强制）

### 8.3 跨引擎一致性测试（Phase 2/3 验收核心）

同一 conformance suite 对四种引擎形态执行，断言**协议级行为一致**：

| 形态 | 构造 | 断言重点 |
|------|------|---------|
| Rust server | `mirri-server` 起真实端口 + `WsClient` | 线格式逐字节一致 |
| TS 旧 server | `kap-server`（Phase 2 过渡期） | 行为等价（快照对比） |
| ACP binary | `StdioTransport::spawn(mirri-acp)` + mock IDE 宿主 | stdio reverse RPC 全链路 |
| NAPI 内嵌 | Node 进程加载 `.node` + `createEngine` | event/reverse-rpc 桥 |

通过标准：四种形态在同一测试用例上全部绿，才允许协议/引擎行为变更合入。

### 8.4 FFI / unsafe 测试（`mirri-node`）

- `unsafe-code = "deny"` 全局生效，`mirri-node` 的 unsafe 集中在 `#[napi]` 宏展开层，业务代码零 unsafe
- CI nightly job：`miri` 跑 `mirri-node` 的 Rust 侧 FFI 测试；Linux sanitizer（ASan/UBSan）跑 `mirri-acp` 二进制
- JS 侧：Node 集成测试（`node:test`），覆盖 `createEngine → call → event` 全链路 + 反复 create/close 的内存平稳性（heap 快照对比）

### 8.5 测试矩阵与准入标准

- 平台矩阵：Linux / macOS / Windows × {unit, integration, conformance, clippy, fmt}
- 准入标准（对应 00-overview §6）：
  - `cargo test --workspace` 全绿
  - `cargo clippy --workspace --all-targets -- -D warnings` 零 warning
  - 三平台全量通过
  - 协议变更必须同步更新 golden 文件与 conformance suite

## 9. 版本管理

### 9.1 workspace 统一版本

- 所有 crate 共享 `workspace.package.version`（当前 `0.1.0`），**pre-1.0 阶段不做逐 crate 独立版本**：
  - 理由：crate 间全部 path 依赖同仓库发布，独立版本只增加同步成本；统一版本让「workspace 即产品」的语义清晰
  - 机制：`version.workspace = true` + CI 校验（`cargo metadata` 断言所有 mirri crate 版本一致）
- 进入 1.0 后决策点：若出现外部消费方按 crate 粒度升级的需求，再切换为逐 crate semver（届时 `workspace.dependencies` 中的 mirri 内部路径依赖改为 `version = "=x.y.z"` 锁定）

### 9.2 changeset 策略

采用 **changesets**（`.changeset/*.md` per-PR 变更描述），发布于合并时生成：

- 每个 PR 必须携带 `.changeset/<slug>.md`（`minor` / `patch` / `none`），注明：变更主题、影响面（涉及哪些 crate）、协议影响（是否改 wire 格式/方法目录）
- 协议/线格式变更一律标 `minor`（0.x 阶段对应 breaking），并强制要求更新 `mirri-proto` 的 schema golden；`none` 仅限纯内部重构
- 发布时由 changeset 聚合生成 CHANGELOG（根 CHANGELOG + 每 crate 的 changelog 文件），tag 格式 `mirri-v0.1.0`

### 9.3 发布流程（checklist）

1. 验证：`cargo test --workspace` + clippy + 三平台 conformance 全绿
2. 版本：`cargo set-version 0.<minor>.<patch>`（统一 workspace 版本）→ changesets 聚合成 changelog
3. 构建：7.4 矩阵全 target 构建 + artifact 冒烟（`mirri --version`、ACP conformance、`node -e "require(...)"`）
4. 发布顺序：按 7.5 依赖序发 crates（内部 registry 或 git tag 分发，按产品决策不使用 crates.io 公开发布）→ npm 包（`@mirri/node` + 平台包）→ release asset（二进制）
5. 回滚：二进制与 npm 包保留上一版本 tag；`.node` 与 npm 主包版本强一致由 CI 校验防呆

### 9.4 依赖引入规范

- 新外部依赖必须：进 `workspace.dependencies`（禁止 crate 内私搭版本）+ `cargo deny` 白名单（Apache-2.0 / MIT 优先）+ 说明用途的注释
- `mirri-core` / `mirri-proto` / `mirri-os` 的依赖新增需双人评审（零 IO 红线）
- 内部 crate 一律 path 依赖（同仓库开发期）+ 发布期锁定版本，禁止 git 分支依赖

---

## 附录 A：验收标准速查（对应 00-overview §6）

- [ ] `cargo test --workspace` 全量通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 无 warning
- [ ] `cargo machete` 无未声明依赖；依赖方向 CI 检查通过
- [ ] 同一 conformance suite 在 server / acp / napi / TS 四种形态全绿
- [ ] 三平台（Linux / macOS / Windows）构建 + 测试通过
