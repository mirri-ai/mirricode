# Rust Core 架构迁移 — Phase 3: TUI NAPI Binding + ACP Rust Binary 设计

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——可随执行推进更新状态；设计主体见 [../00-overview.md](../00-overview.md) 与 [../01-protocol-unification.md](../01-protocol-unification.md)，跨阶段状态汇总见 [../ROADMAP.md](../ROADMAP.md)。
> **路线裁决（持久）**：引擎开关实名 `MIRRICODE_RUST_ENGINE`（优先级 > v2 默认 > v1）；交付物（`mirri-node` / `mirri-acp` / `createMirriHarnessRust` / `SDKRpcClientRust` / NAPI 包 `packages/mirri-node`）以本文档的 FFI 与映射设计为准；方法目录以 mirri-proto 注册表为准，MCP 域以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 为准。

> 状态: Draft
> 日期: 2026-08-29
> 作者: Architecture Team
> 关联文档: [00-overview](../00-overview.md) · [01-protocol-unification](../01-protocol-unification.md) · [02-phase2-web-migration](02-phase2-web-migration.md) · [04-phase4-ts-retirement](04-phase4-ts-retirement.md)

## 1. 迁移目标

Phase 3 的目标是让 **TUI** 与 **ACP** 两条前端路径从 TypeScript 引擎（`agent-core-v2`）切换到 Rust 引擎（`rust/crates/mirri-core`），完成 00-overview 中「三条路径，一套协议」的最后两块拼图。Phase 2 已让 Web 路径通过 WS JSON-RPC 2.0 对接 Rust server（`mirri-server`），Phase 3 复用同一套协议帧，只更换传输。

| 前端 | 当前状态 | 目标状态 | 传输 | 引擎接入 |
|------|---------|---------|------|---------|
| TUI (`apps/mirri-code`) | in-process 内存 RPC (`createRPC()`) + `SDKRpcClientV2` → TS `agent-core-v2` | `SDKRpcClientRust` → NAPI binding → Rust engine | NAPI FFI（进程内，cdylib `.node`） | `EmbeddedEngine` |
| ACP (`packages/acp-server` / `packages/acp-adapter`) | IDE spawn `mirri acp` → TS `acp-server`（v2，经 Klient facade）或 `acp-adapter`（v1, `MIRRICODE_LEGACY_FLAG=1`） | IDE spawn `mirri-acp`（Rust binary，引擎内嵌） | stdio JSON-RPC 2.0（newline-delimited） | `EmbeddedEngine` |

### 1.1 具体目标

1. **TUI 零协议层损耗** — 消除 `createRPC()` 的 `setTimeout(0)` + `JSON.stringify/parse` 模拟网络开销（当前每个调用至少一轮 event loop 往返），改为 NAPI 直接调用。
2. **ACP 消除 Node.js 依赖** — IDE 集成不再需要目标机器安装 Node.js 运行时；`mirri-acp` 是自包含的静态链接 binary。
3. **统一协议面** — TUI 与 ACP 内部都讲与 WS 协议完全一致的 JSON-RPC 2.0 帧（方法名、参数结构、错误码、事件载荷一致），区别只在传输层；协议合规性由共享测试保证。
4. **单一引擎** — `mirri-core` 成为唯一引擎；TS `agent-core-v2` 在 TUI/ACP 侧只剩迁移窗口期的 fallback（Phase 4 退役）。

### 1.2 非目标（超出本阶段范围）

- Web 前端迁移（Phase 2 完成）。
- TS engine 的最终删除（Phase 4：删 `agent-core-v2`、`kap-server`；本阶段只删除 TUI 与 ACP 侧对它的调用路径）。
- `mirri-node` 之外的其它 FFI 宿主（不计划 Python/Go binding）。

## 2. NAPI Binding 设计（TUI 路径）

### 2.1 Crate 与产物

新建 crate `rust/crates/mirri-node/`，`crate-type = ["cdylib", "lib"]`（`cdylib` 产出 `.node` 供 JS `require()`；额外 `lib` target 供 Rust 侧 `cargo test` 直接链接），用 `napi-rs` v3（`napi-derive` + `napi` crate）编译为 `.node` 加载库：

```
rust/crates/mirri-node/
├── Cargo.toml          # [lib] crate-type = ["cdylib", "lib"]（lib 供 Rust 侧测试）, crate-name = "mirri_node"
├── package.json        # napi 配置: name "@mirri-ai/mirri-node", targets, 见 §2.9
├── build.rs            # napi-rs 生成常量（platform/version），可选
├── npm/                # `napi build` 生成的各平台 optionalDependencies 包目录
└── src/
    ├── lib.rs          # #[napi] 导出: createEngine(), RustEngine class
    ├── engine.rs       # RustEngine: 持有 Arc<dyn EngineAdapter> + ThreadsafeFunction 桥
    ├── event.rs        # 引擎事件 → ThreadsafeFunction → JS EventEmitter
    └── reverse_rpc.rs  # 反向 RPC 请求 → JS handler → 响应回传（oneshot 配对）
```

npm 侧发布名：主包 `@mirri-ai/mirri-node`，各平台二进制包 `@mirri-ai/mirri-node-<platform>-<arch>`（见 §2.9）。

### 2.2 架构

```
┌─── Node.js 进程 ────────────────────────────────────────────────┐
│  TUI (apps/mirri-code)                                           │
│   └─ MirriHarness                          ← 接口不变（§2.10）   │
│      └─ SDKRpcClientRust (packages/node-sdk) ← 替换 SDKRpcClientV2│
│         └─ createEngine(options) → MirriRustEngine               │
│            └─ require('@mirri-ai/mirri-node')                    │
│               └─ mirri-node.<platform>.node (NAPI cdylib)        │
└───────────────────────┬──────────────────────────────────────────┘
                        │ NAPI FFI（同进程，无序列化热路径）
┌───────────────────────▼──────────────────────────────────────────┐
│  mirri-node crate                                                 │
│   ├─ #[napi] RustEngine（JS 对象 ↔ Rust 实例数据）                 │
│   ├─ EngineAdapter 实现: EmbeddedEngine                           │
│   │   └─ mirri-core 领域服务（session/agent/tool/permission/...） │
│   └─ tokio::runtime（独立线程池，不占 Node 主线程）                │
└───────────────────────────────────────────────────────────────────┘
```

关键点：**帧语义统一**。`callMethod(method, params)` 的 `method` 名称、`params` 结构、错误码、事件载荷与 WS 协议（01 文档方法目录）完全一致；NAPI 路径只是省掉了字节序列化——`serde_json::Value` 与 JS 对象直接互转，等价于同一帧的逻辑表示。

**握手（handshake）**：`createEngine()` 内部仍执行 doc 01 §5 的握手（`server_hello` / `client_hello` + 能力协商）。FFI 传输虽然不会断连，但保持握手协议一致性使得 `EmbeddedEngine` 和 `RemoteEngine` 可以互换，且 capability negotiation 仍然有意义——客户端在 `client_hello` 的 `capabilities.reverseRpc` 中声明支持的反向 RPC 域（approval/question/fs/terminal），引擎据此决定走反向 RPC 还是本地降级（对齐 doc 01 §7.6 白名单语义）。若性能敏感可支持 `handshake: "skip"` 快速模式（跳过协商、使用默认能力集）。

### 2.3 TS API surface

```ts
// packages/node-sdk/src/rust-engine.ts（类型声明；实现来自 .node binding）

export interface RustEngineOptions {
  readonly homeDir: string;
  readonly configPath: string;
  readonly uiMode?: string;
  readonly env?: Readonly<Record<string, string | undefined>>; // 配置覆盖，对齐 MirriHarnessRuntimeOptions
  readonly threads?: number;      // Tokio worker 数，默认 = 可用 CPU 核数
  readonly maxPendingRpc?: number; // 并发 RPC 上限（背压），默认 1024
}

export interface EngineEvent {
  readonly method: string;        // 统一事件名: assistant.delta / tool.result / turn.ended ...
  readonly params: unknown;
}

export interface MirriRustEngine {
  /** 正向 RPC。method 为统一协议方法名（与 WS 协议一致）。 */
  callMethod(method: string, params?: unknown): Promise<unknown>;
  /** 事件订阅，返回取消函数。 */
  onEvent(handler: (event: EngineEvent) => void): () => void;
  /**
   * 反向 RPC 注册（approval.ask / question.ask / fs.readTextFile /
   * fs.writeTextFile / terminal.exec ...）。handler 的 Promise resolve 值
   * 即反向 RPC 的 result。返回取消函数。
   */
  onReverseRpc(method: string, handler: (params: unknown) => Promise<unknown>): () => void;
  /** 关闭引擎：停止 Tokio runtime、flush、回收内存。可重复调用（幂等）。 */
  dispose(): Promise<void>;
}

export function createEngine(options: RustEngineOptions): MirriRustEngine;
```

Rust 侧对应结构：

```rust
// rust/crates/mirri-node/src/engine.rs（示意）
#[napi(js_name = "RustEngine")]
pub struct RustEngine {
    adapter: Arc<dyn EngineAdapter>,
    event_tsfn: ThreadsafeFunction<serde_json::Value>,      // 事件 → JS
    reverse_rpc_tsfn: ThreadsafeFunction<ReverseRpcCall>,   // 反向 RPC 请求 → JS
    pending: Mutex<HashMap<u64, PendingCall>>,              // 正向 RPC 待完成表
    next_id: AtomicU64,
    disposed: AtomicBool,
}

#[napi]
impl RustEngine {
    #[napi(js_name = "callMethod")]
    pub fn call_method(&self, method: String, params: Option<serde_json::Value>) -> AsyncTask<CallMethodTask> { ... }

    #[napi(js_name = "onEvent")]
    pub fn on_event(&self, callback: JsFunction) -> JsFunction /* unsubscribe */ { ... }

    #[napi(js_name = "onReverseRpc")]
    pub fn on_reverse_rpc(&self, method: String, callback: JsFunction) -> JsFunction { ... }

    #[napi(js_name = "dispose")]
    pub fn dispose(&self) -> AsyncTask<DisposeTask> { ... }
}
```

### 2.4 事件处理：Rust → NAPI callback → Node.js EventEmitter

- 每个 `RustEngine` 实例内部持有一个 `EventEmitter`（Node 侧 `node:events`）。
- `onEvent(handler)` 即 `emitter.on('event', handler)`；`onReverseRpc(method, handler)` 即 `emitter.on(`reverse-rpc:${method}`, handler)`。
- Rust 侧：`EmbeddedEngine` 订阅 `mirri-core` 的 event bus（DomainEvent 流），把事件包装成统一事件帧（`RpcNotification`），经 `ThreadsafeFunction<serde_json::Value>`（`ErrorStrategy::Fatal`）投递到 Node 主线程，`emit` 到 EventEmitter。
- **背压**：`ThreadsafeFunction` 设 max queue size（默认 1024）；队列满时事件帧按优先级丢弃策略处理（delta 类可丢旧保新，turn.ended/approval 类不可丢——与 01 文档的事件可靠性分级一致）。
- Node 侧 handler 抛异常只 `console.error` + `process.emitWarning`，不崩进程、不中断事件流；Rust 侧回调投递永不阻塞引擎线程（TSFN `call` 非阻塞）。

### 2.5 线程模型

```
Node 主线程 (JS)                     Tokio worker 线程 (Rust engine)
──────────────                       ──────────────────────────────
callMethod() ──┐
               │ serde 直转 params
               ▼
        tokio::spawn ──────────────► adapter.call_method(method, params)
               ▲                          │ 领域服务执行（含 IO、LLM 流）
               │  result / error          ▼
        resolve Promise ◄────── ThreadsafeFunction::call (queued)
               │
onEvent handler ◄────────────────── ThreadsafeFunction::call (非阻塞)
               │
反向 RPC:      │
JS handler ◄───┼─────────────────── ThreadsafeFunction (reverse_rpc_tsfn)
   │           │
   └─ Promise ─┼───────────────────► oneshot::Sender 唤醒挂起的引擎任务
```

- 引擎跑在**独立 Tokio multi-thread runtime**（`RustEngine` 构造时创建，`Arc<Runtime>` 持有）。Rust 侧从不直接碰 Node 线程/JS 对象；反向 RPC 的响应经 `oneshot` channel 唤醒 `EmbeddedEngine` 中挂起的请求。
- 正向 RPC 完成通知与反向 RPC 请求共用同一 `reverse_rpc_tsfn` 通道（消息带来源标记：`RpcResponse{ id, result|error }` vs `RpcRequest{ id, method, params }`），保证投递顺序与主线程交错方式一致。
- `dispose()`：优雅关闭顺序见 §2.5.2（置 `disposed` → 取消 in-flight turn → flush 持久化 → abort TSFN → 5 s 超时强杀 → 释放引擎状态）。

#### 2.5.1 反向 RPC 健壮性：TSFN 队列、超时传播与死锁防护

- **TSFN 队列容量与溢出行为**：`reverse_rpc_tsfn` 队列容量上限 **1024**（构造时经 `maxPendingRpc` 可调；事件 `event_tsfn` 沿用 §2.4 的容量与按优先级丢帧策略）。队列满时**阻塞投递 + 超时**（默认 2 s），超时后丢弃该请求并以 `reverse_rpc_timeout` 失败——优先阻塞而非直接 drop：立即丢弃会让引擎侧挂起的 `oneshot::Sender` 静默悬空，失败不可观测；阻塞 + 显式超时错误码保证失败可见、可重试。
- **引擎侧超时取消传播到 JS**：反向 RPC 无宿主响应超时默认 30 s（§4.3）。超时后引擎侧 `oneshot::Sender` 被 drop，但 JS handler 的 Promise 可能仍 pending——需要配套清理机制：每个反向 RPC 帧携带独立 `reverseRpcId`，JS 侧维护 pending map；引擎侧超时/取消时经同一 TSFN 投递该 id 的取消帧（错误码 `reverse_rpc_timeout`），JS 侧据此 reject 对应 handler Promise 并移除 pending 条目；handler 正常 resolve/reject 后同样清理条目。禁止留下无归属的 pending Promise。
- **死锁断言（硬性规范）**：反向 RPC handler **不得同步调用 `callMethod`**（含同步等待其 Promise 完成 / 阻塞主线程等引擎返回值）。机理：handler 在 Node 主线程执行，其响应必须经**同一** `reverse_rpc_tsfn` 队列回流主线程；若 handler 同步阻塞等待一个依赖该队列周转才能完成的结果，队列无法前进 → 引擎侧挂起任务永不唤醒 → 死锁。handler 必须返回 Promise 异步编排；确需引擎状态时先返回降级/暂挂结果，后续用事件或异步 `callMethod` 补充。该约束由 lint + 反射测试强制（§9.1）。

#### 2.5.2 dispose() 优雅关闭规范

`dispose()` 的关闭顺序是显式契约（幂等；重复调用直接返回）：

1. **置 `disposed` 标志**：拒绝所有新 RPC（错误码 `engine_shutdown`）；`onReverseRpc` 新注册直接返回 no-op 取消函数。
2. **取消所有 in-flight turn**：向引擎发出 `message.abort` 语义的取消信号，等待 turn 任务 settle；此后不再投递新事件。
3. **flush 待持久化状态**：调用引擎持久化 flush（会话状态、事件队列写盘），保证 `dispose()` 返回后磁盘状态一致。
4. **Drop 所有 TSFN handle**：`event_tsfn` / `reverse_rpc_tsfn` 先 `abort()` 再 drop（§7.2）；队列中未投递回调不再执行；仍 pending 的 JS Promise 经 §2.5.1 的取消帧机制以 `engine_shutdown` reject。
5. **超时强杀**：步骤 2–4 总预算 **5 s**；graceful 超时后不再等待——直接 abort+drop TSFN → `Runtime::shutdown_background` → 释放 `Arc<EngineAdapter>` 最后引用、回收引擎状态，并记录 force-close 日志。

### 2.6 内存管理：小 payload JSON、大 payload 零拷贝

| 载荷类型 | 阈值 | 路径 | 拷贝次数 |
|---------|------|------|---------|
| 常规 RPC 参数/结果 | < 64 KB（默认） | `serde_json::Value` ↔ JS object（napi-rs serde 直转） | 0 次序列化往返 |
| 大 payload（session.export 的 transcript、goal.get_state、大 tool result、日志块） | ≥ 64 KB | `napi::JsBuffer`：Rust `Vec<u8>` → `JsBuffer::from_external`（所有权移交 JS，finalizer 释放） | 0 次拷贝（一次 serde → Vec<u8> 写入） |
| 超大流（未来优化，首版不做） | > 8 MB，流式 | `SharedArrayBuffer` 共享 slab：引擎直接写入预分配区，JS 侧读 length/offset | 0 次拷贝 |

- 大 payload 的 RPC 方法在 Rust 侧签名上显式声明为 `JsBuffer` 进出（如 `session.export` 返回 `Buffer`），JS 侧 `SDKRpcClientRust` 按方法名查「大载荷方法表」走 Buffer 分支，其余走对象分支。
- `SharedArrayBuffer` 仅作为 Phase 3 后期的可选优化（Node.js 无浏览器 COOP/COEP 限制）；首个发布版以 `external Buffer` 为上限，避免一次性引入 slab 复用的复杂度。
- 引擎内部复用写缓冲（`Vec<u8>` per-worker），避免大消息反复 `to_vec`。

### 2.7 错误处理：Rust panic → napi::Error → Node.js Error

- 每个 `#[napi]` 导出函数体包 `std::panic::catch_unwind(AssertUnwindSafe(...))`（napi-rs 宏自带同步路径兜底；我们额外覆盖异步任务内 panic）。
- Tokio 任务包装器（`CallMethodTask` 的 `complete()` 内）再包一层 `catch_unwind`：panic 捕获后转为 `napi::Error::from_reason`，消息带 panic payload + `Backtrace` 摘要。
- `std::panic::set_hook` 在 binding 初始化时安装：panic 同时写日志文件（`MIRRI_HOME/logs/`），不依赖 JS 侧。
- 错误码规约：协议层错误沿用 01 文档的 JSON-RPC 错误码（`-32700` ParseError / `-32600` InvalidRequest / `-32601` MethodNotFound / `-32602` InvalidParams / `-32603` InternalError + 引擎领域错误码）；NAPI 边界只新增 `engine_shutdown`（dispose 后调用）。JS 侧收到统一的 `MirriEngineError extends Error`（`name` + `code` + `method` 字段），与 `SDKRpcClientBase` 现有 `MirriError` 映射层对接。
- 反序列化失败（参数形状不符）返回 `InvalidParams` 错误而非 panic——边界上任何用户可控输入都不得触发 unwind。

### 2.8 构建与发布

- 工具链：`@napi-rs/cli`（`napi build --platform --release`），产物 `mirri-node.<platform>-<arch>.node`。
- **跨平台 prebuilt 矩阵**（CI：GitHub Actions）：

| 平台 | 架构 | 构建方式 | 产物 |
|------|------|---------|------|
| Linux | x64 / arm64 | manylinux 容器（x64）+ `cross`/容器（arm64） | `mirri-node.linux-x64-gnu.node` / `linux-arm64-gnu.node` |
| macOS | x64 / arm64 | 原生 runner 各一（或 universal2，首版分开） | `mirri-node.darwin-x64.node` / `darwin-arm64.node` |
| Windows | x64 | MSVC runner（或 zig 交叉） | `mirri-node.win32-x64-msvc.node` |

- 发布：主包 `@mirri-ai/mirri-node`（`index.js` 按 `process.platform + arch` 解析）+ 7 个 `optionalDependencies` 平台包（`@mirri-ai/mirri-node-<platform>-<arch>`），由 `napi` CLI 的 `npm/*` 目录生成。
- Node 版本要求：`>= 20`（N-API ≥ 8；对齐 doc 05 §7.2）。N-API 的 ABI 稳定性保证引擎升级不强制重建 `.node`。
- 版本策略：`mirri-node` 版本与引擎协议版本绑定（每次协议方法目录变更即升 minor），平台包与主包同版本。

### 2.9 SDK 集成：SDKRpcClientV2 → SDKRpcClientRust

新建 `packages/node-sdk/src/sdk-rpc-client-rust.ts`：

```ts
export class SDKRpcClientRust extends SDKRpcClientBase {
  private readonly engine: MirriRustEngine;

  constructor(options: RustEngineOptions, extra?: ...) {
    super();
    this.engine = createEngine(options);
    // 事件桥：引擎事件 → base class registries（与 v2 的 session-wiring 同构）
    this.engine.onEvent(({ method, params }) => this.receiveEvent(method, params));
    // 反向 RPC 桥：approval.ask / question.ask → base class handler 注册表
    this.engine.onReverseRpc('approval.ask', (p) => this.resolveApproval(p));
    this.engine.onReverseRpc('question.ask', (p) => this.resolveQuestion(p));
    this.engine.onReverseRpc('fs.*', ...); this.engine.onReverseRpc('terminal.*', ...);
  }

  // 方法 override 面与 SDKRpcClientV2 一一对应，仅实现改为:
  override async createSession(payload: CreateSessionOptions): Promise<...> {
    return this.engine.callMethod('session.create', payload) as ...;
  }
  override async prompt(payload: PromptInput): Promise<...> {
    return this.engine.callMethod('message.send', payload) as ...; // doc 01 规范名（v2 面 prompt → message.send）
  }
  // ... 其余 ~80 个方法同构
}
```

- **复用映射层**：Rust 引擎暴露与 klient facade 相同的 v2 RPC 方法面（wire-compatible），因此 `v2/session-mapper.ts`、`v2/config-mapper.ts`、`v2/event-mapper.ts` 原样复用——`SDKRpcClientRust` 的每个 override 与 `SDKRpcClientV2` 的对应 override 只有「klient facade 调用」→「`callMethod`」一处差异。
- **MirriHarness 接口不变**：`MirriHarness` 构造只依赖 `SDKRpcClientBase`（见 `packages/node-sdk/src/mirri-harness.ts`），TUI 层（`apps/mirri-code`）无需感知底层传输变化；新增 factory `createMirriHarnessRust(options)`（`packages/node-sdk/src/index.ts` 导出）。

### 2.10 TUI 终端（PTY）风险

- TUI 终端功能当前依赖 TS 引擎内的 `node-pty`（terminal 面板 / `terminal.*` 反向 RPC）。迁移到 Rust 引擎（NAPI）后，PTY 能力需由 `mirri-os` 的 pty 抽象提供（Unix `openpty`、Windows ConPTY）——与 doc 02 §4.4 的 Terminal 领域是同一依赖。
- **风险**：若 Rust NAPI binding 无法提供 PTY（`mirri-os` pty 实现未就绪、Windows ConPTY 接入延迟等），TUI 终端功能会**静默破坏**——`terminal.create` 成功但无输出，或直接失败，用户感知为终端不可用而引擎其余功能正常。
- **降级选项**：
  - (a) TUI 终端继续直连 `node-pty`（现有 TS 侧实现保留，终端路径绕过 Rust 引擎）——**过渡期建议**；
  - (b) 终端能力位门控：握手 `capabilities` 如实不声明 `terminal`，TUI 按 capabilities 隐藏终端面板（对齐 doc 02 §6.2 的降级语义）；
  - (c) `mirri-os` PTY 实现作为 **Phase 3 前置条件**（与 ACP 本地终端共用同一实现）——**目标方案**。
- **建议**：(a) 过渡 + (c) 目标；(b) 作为周期不够时的兜底。

## 3. ACP Rust Binary 设计（ACP 路径）

### 3.1 Crate 与产物

新建 binary crate `rust/crates/mirri-acp/`：

```
rust/crates/mirri-acp/
├── Cargo.toml          # [[bin]] name = "mirri-acp"; deps: mirri-core, mirri-auth, mirri-os, mirri-proto, serde_json, tokio
└── src/
    ├── lib.rs          # 【修订 2026-09-04 已交付】库面入口（规划未列出）
    ├── main.rs         # 参数解析（--login / --version / 默认 serve）→ 入口
    ├── stdio_transport.rs  # 逐行 JSON-RPC 2.0 读写（newline-delimited），stderr 日志（已交付，文件名与规划一致）
    ├── acp_handler.rs  # ACP 方法 ↔ EngineAdapter 方法（含 request/notification 生命周期）【修订：交付文件名；规划的 acp_mapper.rs 未按此命名】
    └── auth.rs         # terminal-auth（--login）：mirri-auth device-code 流程【修订：规划名；未按此命名，落点以现状为准】
```

发布形态：随 `mirri` CLI 一起分发（Homebrew / npm tarball / 独立下载），也可单独发布，便于 IDE 直接配置 `command: "mirri-acp"`。

### 3.2 架构

```
┌─── IDE (Zed / JetBrains) ───────────────────────────────┐
│  ACP client（@agentclientprotocol/sdk 1.x）             │
└──────────────────────┬──────────────────────────────────┘
                       │ spawn 子进程
                       │ stdin/stdout: JSON-RPC 2.0（逐行帧）
┌──────────────────────▼──────────────────────────────────┐
│  mirri-acp (Rust binary，自包含，无需 Node.js)          │
│   ├─ StdioTransport       逐行读写，stderr 只写日志      │
│   ├─ AcpHandler           ACP 方法 ⇄ 引擎方法（§3.4；交付文件名 acp_handler.rs） │
│   ├─ EmbeddedEngine       EngineAdapter 实现            │
│   │   └─ mirri-core 领域服务（直接调用，无网络/无 IPC） │
│   └─ EventBridge          引擎事件 → ACP session/update │
└──────────────────────────────────────────────────────────┘
```

### 3.3 stdio 帧格式

- 帧体与 WS 协议**同一套 JSON-RPC 2.0 结构**：`{"jsonrpc":"2.0","id":1,"method":"...","params":{...}}`；响应 `result`/`error`；通知无 `id`。区别仅在分帧：stdio 上 **newline-delimited**（LF；解析器容忍尾部 CR），与现有 TS `acp-server` 行为一致。
- stdout 只承载协议帧；日志、告警一律走 stderr（防止污染协议流、防止 IDE 侧解析错位）。
- 大 payload（工具结果等）字段直接内嵌 JSON 字符串；超限结果沿用引擎已有的 `tool_result_truncation` 截断策略，需要全文走文件路径引用（ACP `ToolCallContent` 的 URI 形式）。
- 行长度上限（默认 16 MB）防畸形输入；超长行 → JSON-RPC `ParseError` 响应而非崩溃。

### 3.4 方法映射：ACP 方法 → EngineAdapter 方法

| ACP 方法（1.x） | 引擎方法（统一协议） | 说明 |
|----------------|---------------------|------|
| `initialize` | `handshake`（引擎内部） | 返回 `protocolVersion: "1.x"`、`implementation`、`capabilities`、`authMethods`（含 terminal-auth，见 §3.8）；本地引擎不做网络握手，只回 implementation 元数据 |
| `session/new` | `session.create` | workspace + cwd → 创建会话；`mcpServers` 按现状 warn-and-drop（引擎 MCP 是 workspace 级）【修订 2026-09-04：MCP 域模型以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 三池连接模型为准，warn-and-drop 行为需在该模型下重新裁决】 |
| `session/load` | `session.resume` | wire.jsonl replay（mirri-core 原生能力；ACP 1.x 无独立 load 语义，与 `session/resume` 同落点） |
| `session/resume` | `session.resume` | 同上 |
| `session/list` | `session.list` | 按 workDir 过滤，Shape 对齐 TS 版 `SessionInfo` |
| `session/close` | `session.close` | |
| `session/delete` | `session.close` | 关闭并归档（doc 01：`session.close` 保留可恢复语义并触发 `session.deleted` 事件；协议无独立 delete 方法） |
| `session/fork` | `session.fork` | |
| `session/prompt` | `message.send` | 主交互入口；事件经 §3.6 流回 |
| `session/cancel` | `message.abort` | 支持 `CancelNotification`（notification 形式） |
| `session/set_mode` | `plan.enter` + `config.set` | plan mode → `plan.enter`；其余 v2 mode 枚举 → `config.set`（经 config mapper 语义，合并 patch） |
| `session/set_session_config_option` | `config.set` | 经 config mapper 语义（合并 patch，而非整文档替换） |
| `session/request_permission` | （引擎 `AgentPermissionGate` 触发的反向确认，见 §3.5） | ACP 中 permission 请求由引擎发起（`session/update` 的 `permission_request`），客户端用本方法回 outcome |
| `auth` | `auth.login` / `auth.status` / `auth.logout` | §3.8 |
| `ext/*` | 透传 | 保留扩展点（如 `ext/just_do_it`），未实现返回 `MethodNotFound` |

> **命名说明**：ACP 方法名映射到统一协议方法时使用不同名字——协议按关注点分离组织方法（消息发送 vs 会话生命周期、plan mode vs 会话配置），因此 `session/prompt` → `message.send`、`session/cancel` → `message.abort`、`session/load` / `session/delete` → `session.resume` / `session.close`、`session/set_mode` → `plan.enter` + `config.set`。

映射策略：`AcpHandler`（规划名 `acp_mapper.rs`，实际交付以 `acp_handler.rs` 命名）只做**名称与载荷形状转换**（camelCase 字段名与引擎保持一致，见 `mirri-proto` 的 serde rename 约定），不做业务逻辑；业务全部在 `mirri-core`。

### 3.5 反向 RPC：全部走同一条 stdio 连接

引擎需要宿主能力时，在**同一连接**上发起 JSON-RPC request（方向：engine → IDE），IDE 回 response：

| 引擎侧触发 | ACP 侧承载 | 说明 |
|-----------|-----------|------|
| `approval.ask`（权限确认） | `session/update`（`permission_request`）+ 客户端 `session/request_permission` 返回 | option ids 与 TS 版完全一致：`approve_once` / `approve_always` / `reject` / `plan_opt_<i>` / `plan_approve` / `plan_revise` / `plan_reject_and_exit`（对齐 `packages/acp-server/src/approval.ts`） |
| `question.ask`（自由提问） | `session/update`（question/prompt）+ 客户端回复 | 对齐 TS 版 `question.ts` 的 `q{n}_*` namespace 约定 |
| `fs.readTextFile` / `fs.writeTextFile` | 直接反向 request | 仅当客户端 `clientCapabilities.fs` 声明时启用；否则引擎用本地 fs（`mirri-os::path_access` 白名单校验） |
| `terminal.*`（Bash 执行/信号） | 直接反向 request（`terminal/*`） | 仅当 `clientCapabilities.terminal` 声明时启用（对齐 `acp-server/src/acp-terminal`）；否则引擎用 `mirri-os::pty` 本地执行 |

- 反向 request 使用与正向相同的 `id` 空间（引擎侧独立自增），stdio 连接半双工读写的并发由 `StdioTransport` 内部 dispatch 表管理。
- 客户端未声明的能力：静默降级到引擎本地实现，行为与 TS 版一致（`warnIgnoredMcpServers` 式 warning 走 stderr）。

### 3.6 会话生命周期与事件流

- 生命周期方法全量支持：`new` / `load` / `resume` / `list` / `close` / `delete` / `fork`；`load`/`resume` 依赖 wire.jsonl 的 `metadata` envelope + replay（`mirri-proto::WireOp` 已覆盖）。
- 事件流：引擎 event bus → `EventBridge` → ACP `session/update` notification：

| 引擎事件 | ACP `session/update` 载荷 | 说明 |
|---------|--------------------------|------|
| `assistant.delta` | `agent_message_chunk`（`content: {type:'text', text}`） | 对齐 `events-map.ts` 的 `assistantDeltaToSessionUpdate` |
| `thinking.delta` | `agent_thinking_chunk`（若 SDK 1.x 支持）否则并入 message chunk | 以 SDK 1.3.0 类型为准 |
| `tool.call.started` | `tool_call_started` + `ToolCallContent` | 对齐 `events-map.ts` |
| `tool.call.delta` | `tool_call_delta`（SDK 支持时） | **填补 TS v2 缺口**：Rust 引擎原生产出 delta |
| `tool.progress` | `tool_call_progress`（SDK 支持时） | **填补 TS v2 缺口** |
| `tool.result` | `tool_call_finished` | 对齐 `toolResultToAcpContent` |
| `turn.ended` | `session/update`（`stopReason`） | 对齐 `AcpStopReason` 映射 |

- **TS v2 已知缺口（无 `tool.call.delta`、无 `tool.progress`、无 compaction 事件）在 Rust 路径上补齐**：`mirri-core` 的事件模型原生包含这些事件，`EventBridge` 逐项映射；SDK 1.x 类型面不支持的部分先经 `customSessionUpdateEventTypes` 透传，随 SDK 升级收紧。

### 3.7 ACP SDK 兼容性

- 协议面：ACP **1.x**（`initialize` 返回 `protocolVersion`，协商式向前兼容；不实现 0.x 遗留字段）。
- `session/update` 载荷遵循 `@agentclientprotocol/sdk` 1.3.0 的 `SessionNotification` 类型；新增字段只在 `sessionUpdate` 判别式命名空间内扩展。
- `initialize` 的 `capabilities` 如实声明（`fs`、`terminal` 按客户端支持）；`authMethods` 广告 `AuthMethodTerminal`（`args: ['--login']`），与现有 `auth-methods.ts` 一致，同时保留 `_meta['terminal-auth']` fallback 语义（`process.argv[1]` → 实际为 binary 自身路径）。

### 3.8 认证：terminal-auth

- `mirri-acp --login`：进入 terminal-auth 分支——复用 `mirri-auth` crate 的 device-code 流程（与 `mirri-cli` 的 `run_auth_login` 同逻辑），按 ACP terminal-auth 契约把 token JSON 写到 stdout 后退出。
- `MIRRICODE_HOME` 透传（IDE 侧 `agent_servers.*.env.MIRRICODE_HOME` 注入的沙箱根目录）：token 落在与 ACP server 相同的数据根下（对齐现 `acp.ts` 的注释语义）。
- 非 `--login` 启动：`initialize` 时若发现未认证，返回带 `authMethods` 的响应，客户端据此触发 terminal-auth 流程。

## 4. EngineAdapter Trait（共享抽象）

### 4.1 定义

Trait 与帧类型承载位置：帧类型（`RpcRequest` / `RpcResponse` / `RpcNotification` / `RpcError`）进 `mirri-proto::rpc`（在现有 `WireOp` 之外新增 JSON-RPC 2.0 帧模型，与 WS 协议共用）；trait 进 `mirri-core::adapter`（`mirri-node`、`mirri-acp`、`mirri-server` 共同依赖，无循环）。

```rust
// rust/crates/mirri-core/src/adapter.rs（示意）
use mirri_proto::rpc::{RpcError, RpcNotification};

pub type ProcedureResult = Result<serde_json::Value, RpcError>;

/// 引擎表面：一个可调用的方法 + 双向事件/反向 RPC 通道。
/// 帧语义 = 01 文档的统一协议；实现只负责「帧怎么到达引擎」。
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    /// 正向 RPC 分发。method 未注册 → Err(RpcError::method_not_found)。
    async fn call_method(&self, method: &str, params: serde_json::Value) -> ProcedureResult;

    /// 订阅引擎事件流（pull 式，天然背压）。
    async fn subscribe_events(&self) -> mpsc::Receiver<RpcNotification>;

    /// 注册反向 RPC 处理器：宿主（FFI 宿主 / stdio 客户端）实现的
    /// approval.ask / question.ask / fs.* / terminal.* 都从这里进出。
    async fn on_reverse_rpc(&self, handler: ReverseRpcHandler)
        -> ReverseRpcSubscription;
}

pub type ReverseRpcHandler = Arc<
    dyn Fn(&str, serde_json::Value) -> Pin<Box<dyn Future<Output = ProcedureResult> + Send>>
        + Send + Sync,
>;
```

设计取舍：事件用 **stream（pull）** 而非回调注册（push）——调用方（`mirri-node` / `mirri-acp`）各自 `spawn` 一个转发任务把 stream 桥到自己的传输（TSFN / stdio），背压天然落在 channel 上；避免 push 式回调在三个实现里重复处理慢消费者。

### 4.2 三个实现

| 实现 | 位置 | 传输 | 消费者 | 特点 |
|------|------|------|--------|------|
| `EmbeddedEngine` | `mirri-embedded/src/embedded_server/`（【修订 2026-09-04】独立 crate 承载） | 同进程直接调用领域服务 | `mirri-node`（FFI）、`mirri-acp`（stdio） | 无序列化、无网络；事件 = 订阅 engine event bus；反向 RPC = 回调宿主 |
| `RemoteEngine` | `mirri-remote/src/frames.rs`（【修订 2026-09-04】独立 crate 承载） | WS client（帧序列化进出） | Web 前端（浏览器 WS 客户端）、未来 TUI 连远端 server 的形态 | 与 Embedded 唯一差异在传输；同一方法目录 |
| `InProcessEngine` | `mirri-test-harness` | 同进程 channel + serde 往返 | 协议层/映射层测试 | 帧语义（parse/错误码/顺序）完整，但不做真实 IO |

> **修订注记（2026-09-04）**：EngineAdapter 的 embedded / remote 实现分别由 `mirri-embedded`（`src/embedded_server/`）与 `mirri-remote`（`src/frames.rs`）各自成 crate 承载，**非由 `mirri-transport` 提供**；`mirri-transport` 职责按现状收敛为共享传输层工具，不承载 EngineAdapter 实现。InProcessEngine 仍按上表（`mirri-test-harness`）。

### 4.3 不变式

1. **三个实现的方法目录、事件目录、错误码完全一致**——由共享的协议测试（§9.3）强制。
2. `call_method` 的契约：`method` 未知 → `method_not_found`；参数形状错 → `invalid_params`；领域错误 → 引擎领域错误码（`MirriErrorPayload` 结构，与 TS 版 `fromMirriErrorPayload` 双向往还兼容）。
3. 反向 RPC 无宿主响应超时（默认 30s）→ 返回 `reverse_rpc_timeout`，引擎侧按各自语义降级（approval 超时 = reject；fs 超时 = 工具失败）。
4. 反向 RPC handler 不得同步调用 `callMethod` / 同步阻塞等待引擎结果（死锁风险，见 §2.5.1）；引擎侧超时/取消必须传播到宿主侧 pending Promise（取消帧清理机制，见 §2.5.1）。

## 5. TUI 迁移步骤

| 步骤 | 内容 | 完成标志 |
|------|------|---------|
| **Step 1** | 创建 `rust/crates/mirri-node`：NAPI bindings（`createEngine` + `RustEngine` class）+ npm 包骨架 + CI 构建矩阵 + prebuilt 发布流水线 | `createEngine()` 在 Node 里跑通 `session.create` + `message.send`（mock LLM），事件能流回 JS |
| **Step 2** | 创建 `packages/node-sdk/src/sdk-rpc-client-rust.ts`：`SDKRpcClientRust extends SDKRpcClientBase`，override 面与 `SDKRpcClientV2` 对齐；`createMirriHarnessRust(options)` factory | node-sdk 的 `test/v2/*` 套件在该 client 上跑绿（复用 v2 测试） |
| **Step 3** | `createHarnessForEngine()`（`apps/mirri-code/src/cli/experimental-v2.ts`）增加 Rust 分支：`MIRRICODE_RUST_ENGINE=rust` → `createMirriHarnessRust`；沿用现有 truthy-env 判断模式（对齐 `MIRRICODE_LEGACY_FLAG`） | `MIRRICODE_RUST_ENGINE=rust mirri` 可启动 TUI 并完成一轮对话（【修订 2026-09-04】已交付：开关实名落地） |
| **Step 4** | Feature flag 灰度：`MIRRICODE_RUST_ENGINE=rust` 默认 off（TS v2 仍默认）；内部 dogfood → CI 全量 e2e 双跑 → 逐步开放；**回滚** = 环境变量切回（瞬时） | dogfood 两周无阻塞性回归；e2e 双跑绿（【修订 2026-09-04】已交付：开关按 `MIRRICODE_RUST_ENGINE` > v2 默认 > v1 优先级实现；dogfood/灰度未放开，v2 仍默认） |
| **Step 5** | 默认切换：`MIRRICODE_RUST_ENGINE` 未设置 → Rust；TS v2 通过 `MIRRICODE_RUST_ENGINE=ts` 保留为 fallback。`.node` 加载失败（平台无 prebuilt）时自动 warning + fallback 到 TS v2 | 默认路径为 Rust；fallback 逻辑有测试覆盖（【修订 2026-09-04】**未执行**：默认引擎仍为 TS v2，「默认切换为 Rust」是真实剩余工作） |
| **Step 6** | 移除 TS engine 路径（TUI 侧）：删 `SDKRpcClientV2` 及其 klient wiring、`agent-core-v2` 依赖（TUI 进程内）；v1 legacy 关联代码一并清理 | TUI 代码中不再 import `agent-core-v2`；`MIRRICODE_LEGACY_FLAG` 与 v1 路径移除按 Phase 4 计划执行 |

依赖关系：Step 1 依赖 Phase 2 的协议帧（`mirri-proto::rpc`）与 `EmbeddedEngine`；Step 2–3 纯 JS 侧改造，可并行开两条线。

> **修订注记（2026-09-04）**：Step 1–3 交付物均已存在（`rust/crates/mirri-node`、`rust/crates/mirri-acp`、NAPI 包 `packages/mirri-node`、`createMirriHarnessRust` / `SDKRpcClientRust`）；Step 4 的开关已按 `MIRRICODE_RUST_ENGINE` > v2 默认 > v1 优先级实现；**Step 5「默认切换为 Rust」未执行——TS v2 仍是默认引擎**。

## 6. ACP 迁移步骤

| 步骤 | 内容 | 完成标志 |
|------|------|---------|
| **Step 1** | 创建 `rust/crates/mirri-acp`：`StdioTransport`（newline-delimited JSON-RPC）+ `AcpHandler`（先通 `initialize` + `session/new` + `session/prompt` + 事件流；交付文件名 `stdio_transport.rs` / `acp_handler.rs`，规划名 `acp_mapper.rs` 未采用） | 用 ACP SDK test client 可完成一次「new → prompt → 收到 delta/result/turn.ended」 |
| **Step 2** | 全方法面：生命周期/删除/fork、`set_mode`、`set_session_config_option`、`request_permission`（approval/question 桥）、fs/terminal 反向 RPC、compaction/tool delta 事件；`--login` terminal-auth | `packages/acp-server/test/**` 映射套件（改造后）全绿 |
| **Step 3** | 真实客户端验证：Zed（`agent_servers` 指向 `mirri-acp`）+ JetBrains 插件手测清单（new/load/resume/fork、approval、terminal、auth 全过）；修复帧/事件差异 | 手测清单 100% 通过；发现的问题全部落到回归测试 |
| **Step 4** | `apps/mirri-code/src/cli/sub/acp.ts` 的 `registerNativeAcpCommand` 改为 spawn `mirri-acp` binary（替代进程内 TS server）；`MIRRICODE_LEGACY_FLAG` 仍指向 v1 `acp-adapter` 路径 | `mirri acp` 命令的集成测试（`test/cli/acp.test.ts`）改造后对 spawn 路径绿 |
| **Step 5** | 移除 TS `acp-server` 包：删 `packages/acp-server`（`--login`/`MIRRICODE_HOME` 语义已由 binary 承担）；文档与用户配置迁移说明 | `packages/acp-server` 从 workspace 移除；无残留引用 |

## 7. 内存安全（NAPI 边界）

### 7.1 Rust 所有权 → Node.js GC

- **实例生命周期**：`RustEngine` JS 对象 ↔ Rust `&mut self` 由 napi-rs 的 class 机制绑定；引擎状态（`Arc<dyn EngineAdapter>` + runtime）寄存在 instance data，JS 侧 GC 回收对象时 napi 自动 drop Rust 侧——但**引擎状态不依赖 GC**：`dispose()` 显式回收，防止「JS 对象还活着但引擎已无用」或「进程退出前未 flush」。
- **external Buffer**：`JsBuffer::from_external(vec)` 把 `Vec<u8>` 所有权移交给 JS，finalizer（`External` 析构）里释放；finalizer 只做 `drop`，不执行任何 IO/锁。
- 反向：JS → Rust 的大参数用 `JsBuffer::into_raw` 借用（不拷贝），Rust 侧在调用作用域内消费完即归还，禁止跨 await 持有借用。

### 7.2 线程安全

- JS 对象永不跨线程使用：Rust worker 线程与 Node 主线程之间只经 `ThreadsafeFunction`（napi-rs 内部队列 + 锁）和 `serde_json::Value`（Send）。
- 共享状态最小化：`pending: Mutex<HashMap<u64, PendingCall>>` + `next_id: AtomicU64`；`PendingCall` 只含 `oneshot::Sender`，不含任何 napi 类型。
- TSFN 生命周期：`dispose()` 先 `abort()` 再 drop，保证回调队列清空后才释放引擎；`ReverseRpcCall` 结构体在 TSFN 队列中的存活期由 napi 管理，Rust 侧只持有 `Send` 载荷。
- Tokio runtime 与 Node 隔离：无共享 heap、无跨线程 `Env` 使用（每个线程的 `Env` 只在该线程内用）。

### 7.3 Buffer 策略

- 小 payload：serde 直转，无 `to_vec` / 无 `JSON.stringify` 往返（对比当前 `createRPC` 的 stringify+parse，见 §8）。
- 大 payload：一次 `serde_json::to_vec` 写入 worker 复用缓冲 → `from_external` 移交；hot path 禁止重复分配。
- 可选的 `SharedArrayBuffer` slab（大流）：引擎预分配 N×chunk 环形区，JS 侧通过 `Atomics` 读长度；仅在流式传输场景（未来 session export 流）启用。

### 7.4 Panic 处理

- FFI 边界 `catch_unwind`（§2.7）；`#[napi]` 宏 + 任务包装器双保险。
- `panic = "unwind"` 保持默认（便于 catch_unwind 恢复）；release profile 不开 `panic = "abort"`。
- 用户可控输入任何路径不得 unwind：参数校验先行（`InvalidParams`），`serde_json` 解析失败一律转 JSON-RPC 错误。

## 8. 性能预期（NAPI vs 当前 in-process RPC）

### 8.1 现状基线（TS v2）

- 每条 RPC 经 `createRPC()` 的 `simulateNetwork`（`packages/agent-core/src/rpc/client.ts`）：`setTimeout(0)` 至少一轮 event loop + `JSON.stringify` + `JSON.parse` → 单次调用指令往返 **~0.5–2 ms**（随载荷增大劣化），事件流每事件同样一轮 timer。
- 引擎侧分发链：klient facade → DI scope → 服务 → mapper，中间对象大量分配，GC 压力大。
- 实测基线（Phase 1 审计）：TUI 冷启动（进程启动 → 首轮 prompt 可交互）约 **2s**；Node 进程 RSS 约 **300–500 MB**。

### 8.2 NAPI 预期

| 指标 | TS v2（现状） | Rust（预期） | 提升 |
|------|--------------|-------------|------|
| RPC 延迟（小 payload） | ~0.5–2 ms（timer + stringify/parse） | ~1–10 µs（serde 直转）；异步路径 ~10–50 µs | 预期亚毫秒（sub-ms）；具体提升倍数以 §8.3 基准确认为准 |
| 事件端到端延迟 | ~1 ms 级（每事件一轮 setTimeout） | ~10–50 µs（TSFN 直投） | 预期亚毫秒（sub-ms）；倍数待 §8.3 基准确认 |
| 进程 RSS | 300–500 MB | 80–150 MB（Node baseline ~50 MB + 引擎） | **2–5×** 降低 |
| 冷启动（→ 可交互） | ~2 s | **< 500 ms**（Node boot ~100 ms + require(.node) ~10–30 ms + 引擎 init ~100–300 ms） | ~4× |

说明：收益主要来自三处——消除 timer 轮次、消除 JS 侧分发链（DI/klient/mapper）的 CPU 与中间对象、Rust 引擎内存布局更紧凑。`createRPC` 的模拟网络本身不是瓶颈上限，真实差距以 §8.3 测量为准。

### 8.3 测量方法（进 CI 门禁）

- RPC 延迟分布：`node:perf_hooks` 包一层 client 打点，输出 p50/p95/p99（`scripts/bench/rpc-latency.mjs`，双引擎同脚本）。
- RSS：`process.memoryUsage` 快照 + `--trace-gc` 采样峰值。
- 冷启动：`time mirri ...` + 引擎内打点（`engine.ready` 事件时间戳）。
- 门禁：Rust 路径 p50 RPC ≤ 50 µs、RSS ≤ 200 MB、冷启动 ≤ 500 ms（CI 用固定规格 runner，机器抖动留 30% 余量）。

## 9. 测试策略

### 9.1 NAPI binding

- Rust 侧单测（`cargo test` in `mirri-node`）：参数装箱/拆箱、错误码转换、TSFN 队列背压、dispose 竞态。
- Node 侧集成（`node:test`，`packages/node-sdk/test/rust/**`）：
  - 生命周期：createEngine → 事件订阅 → dispose 幂等；dispose 后调用返回 `engine_shutdown`。
  - 反向 RPC：approval/question/fs/terminal handler 的 resolve/reject 双向。
  - 并发与背压：1024 并发 RPC、事件洪峰（delta 可丢、关键事件不丢）。
  - 内存：`--expose-gc` 下 external Buffer 泄漏检测（GC 后 RSS 回落断言）。
- 大 payload 路径：`session.export` 全量 transcript 走 Buffer 分支，内容逐字节比对。

### 9.2 ACP binary

- **ACP SDK conformance**：跑 `@agentclientprotocol/sdk` 自带 test client / conformance 子集（initialize 协商、notification 形状、error 对象、生命周期状态机）。
- **套件复用**：`packages/acp-server/test/**`（150+ 用例）改造出「stdio transport shim」——测试进程内不再 `startServer`，而是 spawn `mirri-acp` 并走 stdin/stdout；同一套文件双跑 TS server 与 Rust binary，断言完全一致。
- 帧级 fuzz：`proptest` 生成畸形帧（超长行、截断 JSON、CRLF 混入、UTF-8 BOM、并发交错），断言「不崩溃 + 正确错误码」。
- 真机清单：Zed + JetBrains 手测 checklist（§6 Step 3）。

### 9.3 Cross-engine（差分测试）

- `packages/e2e-harness/src/engine-fixture.ts` 增加 `'rust'` 档位 → node-sdk 全套 e2e（含 v1/v2 兼容测试）三引擎（v1 / ts-v2 / rust）同跑。
- **golden 事件序列**：扩展 `agent-core-v2/test/_mirri-harness/golden.test.ts` 模式——同一 prompt 输入，ts-v2 与 rust 各录事件 log（`replay-v2-frames.mjs` 风格），断言逐帧等值（时间戳字段做归一化）。
- 错误面差分：`MirriErrorPayload` 双引擎往返（`error-mapping.test.ts` 模式扩展到 rust）。
- mock LLM：`mirri-test-harness` 的 `mock_llm.rs` 供 Rust 引擎（配置指向 mock provider），保证差分测试确定性。

## 10. 验收标准

| 类别 | 标准 | 验证方式 |
|------|------|---------|
| **TUI 功能** | `MIRRICODE_RUST_ENGINE=rust` 下 `apps/mirri-code/test/**` 全绿；~80+ RPC 方法清单逐项通过（prompt/steer/cancel/swarm/goals/cron/skills/plugins/MCP/shell/export/…）；approval/question/fs/terminal 反向 RPC 交互与 TS 版行为一致 | CI e2e + 方法清单核对表 + 手测 |
| **TUI 兼容** | `MirriHarness` 接口无改动；v1 legacy（`MIRRICODE_LEGACY_FLAG=1`）与 ts-v2 fallback 路径不受影响 | 三引擎 e2e 双跑 |
| **ACP 协议** | ACP SDK conformance 全过；`packages/acp-server/test/**`（stdio shim）对 `mirri-acp` 全绿；Zed/JetBrains 手测清单全过；terminal-auth（`--login`）端到端可用 | §9.2 |
| **性能** | TUI 冷启动 < 500 ms（对照 ~2s）；RPC p50 ≤ 50 µs（对照 ~0.5–2 ms）；RSS ≤ 200 MB（对照 300–500 MB） | §8.3 基准脚本，CI 门禁 |
| **工程** | `cargo test --workspace` 全绿；`cargo clippy --workspace --all-targets -- -D warnings` 无警告；六平台 prebuilt 产物可用（Linux x64/arm64、macOS x64/arm64、Windows x64） | CI + 发布流水线 |
| **收尾** | TS 路径在 TUI/ACP 侧全部移除（`SDKRpcClientV2`、`acp-server` 无引用）；文档更新（README、IDE 配置指南） | grep 审计 + 文档 review |

> **修订注记（2026-09-04）**：
> - 「~80+ RPC 方法清单」口径以 mirri-proto 注册表为准（pin 计数：embedded 165 / serve 154 / both 140 / 注册表 179 行，`rust/crates/mirri-proto/src/rpc/registry.rs`）；
> - 上表 MCP 相关验收以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 三池连接模型为准，`session/new` 的 `mcpServers` warn-and-drop 行为需在该模型下重新裁决（见 §3.4）。

## 11. 风险与对策

| 风险 | 影响 | 对策 |
|------|------|------|
| NAPI 构建复杂度 / 平台二进制覆盖率不足 | 发布延迟；某平台无 prebuilt 导致 TUI 不可用 | napi-rs 标准构建矩阵 + CI 缓存；`optionalDependencies` 按平台解析；加载失败自动 fallback 到 TS v2（Step 5 双保险）；release 前三 OS 真机冒烟 |
| ACP 协议边界情况（畸形帧、半关闭、并发交错、超大行） | IDE 集成稳定性 | conformance 套件 + 帧级 fuzz（§9.2）；与 TS server 差分；超限/畸形一律 JSON-RPC 错误码而非崩溃 |
| TS 特有行为差异（事件顺序、错误码语义、时间戳、slash/marker、截断策略） | 用户可见回归、测试误判 | 差分测试 + golden 事件序列；behavior parity matrix 作为显式契约文档；差异逐条 triage（可接受差异走文档 + 测试豁免，不接受差异必须修复） |
| 内存/泄漏（TSFN 未释放、external Buffer 泄漏、dispose 竞态） | 长会话内存膨胀 | `--expose-gc` 泄漏测试 + RSS 门禁；dispose 幂等 + abort 顺序单测（§7.2） |
| TUI 终端 PTY 依赖（TS `node-pty` → `mirri-os` pty 迁移缺口） | TUI 终端功能静默破坏（`terminal.create` 成功但无输出 / 直接失败） | §2.10：过渡期 (a) TUI 终端直连 `node-pty`（绕过 Rust 引擎）；目标 (c) `mirri-os` PTY 作为 Phase 3 前置；兜底 (b) 能力位门控，UI 按 capabilities 隐藏终端面板 |
| ACP binary 体积/启动时间 | IDE 集成体验（每次新会话 spawn） | release profile：`strip` + `lto` + `opt-level=3`，目标 < 40 MB、启动 < 100 ms；懒初始化重模块 |
| 双引擎维护成本（迁移窗口内两套都要跑） | 修复成本翻倍、漂移 | 迁移窗口锁死（Phase 3 结束即删 TS v2 路径，Phase 4 收尾）；feature flag 数量最小化（只留 `MIRRICODE_RUST_ENGINE` / `MIRRICODE_LEGACY_FLAG` 两个开关）；差分测试防漂移 |
| 反向 RPC 超时/宿主能力差异（IDE 声明 fs/terminal 但实现不一致） | 工具执行失败 | 超时默认 30s + 降级语义（§4.3）；能力声明如实上报；IDE 侧问题记录到兼容性清单 |
| `mirri-node` 与引擎协议版本耦合 | 升级 break | minor 版本随协议方法目录变更；N-API ABI 稳定保证不强制重建；主包与平台包同版本发布 |

---

## 参考实现位置

- TUI 引擎路由：`apps/mirri-code/src/cli/experimental-v2.ts`（`createHarnessForEngine`、`MIRRICODE_LEGACY_FLAG`）、`apps/mirri-code/src/main.ts`
- node-sdk：`packages/node-sdk/src/mirri-harness.ts`（`MirriHarness`）、`packages/node-sdk/src/sdk-rpc-client-v2.ts`（`SDKRpcClientV2`，方法迁移清单注释）、`packages/node-sdk/src/types.ts`、`src/v2/{session,config,event}-mapper.ts`
- 现 RPC 模拟传输：`packages/agent-core/src/rpc/client.ts`（`createRPC` / `simulateNetwork`）
- ACP TS 版：`packages/acp-server/src/server.ts`、`approval.ts`、`events-map.ts`、`convert.ts`、`auth-methods.ts`、`interaction-bridge.ts`、`acp-terminal/`、`acp-fs/`；TUI 侧 `apps/mirri-code/src/cli/sub/acp.ts` / `acp-native.ts` / `acp-legacy.ts`
- Rust 侧：`rust/crates/mirri-core`（领域服务）、`mirri-proto/src/wire.rs`（wire.jsonl op，RPC 帧模型待新增 `rpc` 模块）、`mirri-server/src/{ws,rpc,ipc}.rs`（Phase 2 WS 帧）、`mirri-auth`（device-code）、`mirri-os`（pty / path_access）、`mirri-test-harness`（mock_llm / http_client）、`mirri-cli/src/commands.rs`（`run_auth_login`）
