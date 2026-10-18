# Rust Core 架构迁移 — 总体设计文档

> 状态: Draft · 原稿日期: 2026-08-29 · 2026-09-04 重绘终态架构图 ·
> 作者: Architecture Team
> 方法目录事实源: `rust/crates/mirri-proto/src/rpc/registry.rs`
> MCP 域事实源: [../mcp-architecture.md](../mcp-architecture.md)
> **执行状态不在本文维护**：阶段进度、开放项、基线数字统一见 [ROADMAP.md](ROADMAP.md)

## 1. 背景与动机

产品在从"TS 引擎为运行时核心、三端各接各的"走向"Rust 内核为唯一引擎"。动机（现状的详细盘见 [context.md](context.md)）：

1. **Rust core 是孤岛** — 功能完备但消费者不足，投资无法变现
2. **协议碎片化** — 多种传输 × 多种消息格式 × 不对等的能力分布
3. **TS 引擎性能瓶颈** — 冷启动慢、内存占用高、GC 停顿影响交互延迟
4. **跨平台一致性** — TS 引擎在 Windows 上 shell/路径行为不一致
5. **部署复杂度** — 三端各自需要 Node.js 运行时

### 1.1 目标

- **统一协议**：单一 JSON-RPC 2.0 双向连接，覆盖全部交互（正向 RPC + 事件流 + 反向 RPC）
- **Rust core 成为唯一引擎**：三端全部切换到 Rust 引擎
- **Node.js 依赖消除**：引擎、CLI、server 全部为原生 Rust 二进制，产品运行面零 Node.js 依赖（ADR-001 扩大范围：TS CLI 整体退役）
- **跨平台一致**：路径、Shell、文件监听通过 trait 抽象，行为一致
- **性能极致**：零拷贝 FFI、零网络开销的 in-process 模式

> 现状与目标的结构差距（当前架构、协议现状、crate 差异、跨平台模块状态）统一盘在 [context.md](context.md)（参考类，带快照日期）。

## 2. 目标架构

```
┌──────────────────────────────────────────────────────────────────────┐
│  前端层   Web (WS·localhost) │ ACP (stdio) │ Desktop (Electron 壳)      │
│           终端 UI：Rust 原生 TUI（ratatui，进程内直连引擎；ADR-002）    │
└──────────────────────────────┬───────────────────────────────────────┘
                               │ 三端同一线格式：JSON-RPC 2.0
┌──────────────────────────────▼───────────────────────────────────────┐
│  契约层   mirri-proto 注册表（唯一事实源）                             │
│           正向 RPC │ 事件流 │ 反向 RPC │ capabilities │ cursor{seq,epoch}│
│           ⇄ contract-e2e fixture 双后端对拍（klient schemas ⇄ EMB）   │
└──────────────────────────────┬───────────────────────────────────────┘
┌──────────────────────────────▼───────────────────────────────────────┐
│  App 层（宿主进程单例组合根——desktop 常驻 / TUI / server 进程）       │
│   全局配置发现(user级) │ UserPool[MCP 用户级连接] │ Plugin Manager     │
│   (+PluginPool) │ OAuth Store │ Flags │ Telemetry                     │
│   ↕ 全局管理面（无 workspace 上下文：mcp.status / 设置面板）          │
└──────────────────────────────┬───────────────────────────────────────┘
┌──────────────────────────────▼───────────────────────────────────────┐
│  Workspace 层（一等并发单元）                                         │
│   workspace 生命周期（open/close/资源归属/信任门）                     │
│   WorkspacePool[project 层 MCP] │ 会话树 │ Agent Teams[协作spec]     │
└──────────────────────────────┬───────────────────────────────────────┐
┌──────────────────────────────▼───────────────────────────────────────┐
│  适配层   EngineAdapter                                              │
│           EmbeddedEngine (mirri-embedded·组合根+EMB路由)              │
│           RemoteEngine (mirri-remote)                                │
└──────────────────────────────┬───────────────────────────────────────┘
┌──────────────────────────────▼───────────────────────────────────────┐
│  内核层   mirri-core —— 零 IO、trait-only（平台/时钟/文件全经trait）  │
│   Session │ Agent Loop │ Tools │ Permission │ Skills │ Hooks         │
│   Plan/Goal │ Swarm/Subagents │ Collaboration[团队协作·见spec]       │
│   ├ LLM 抽象：mirri-kosong（trait LlmFactory，多 provider）           │
│   ├ 执行环境：mirri-os（fs/glob/process/pty/search/path_access/      │
│   │              path_codec/shell/fs_watcher，平台差异唯一入口）       │
│   └ 持久化：mirri-persistence（EventLog：seq/epoch/ring buffer）      │
└──────────────────────────────────────────────────────────────────────┘
```

分层职责要点：

- **App 层**是"桌面产品化"的架构落点：无 workspace 上下文的全局管理面（MCP 用户级配置/状态、插件管理、OAuth）住在这里，生命周期是宿主进程而非 session/workspace。
- **Workspace 层**承载 multi-workspace 并发（desktop 同时开多项目）：隔离（project 层凭据不跨 workspace）、复用（App 层 UserPool 共享）、生命周期（workspace 关闭 → WorkspacePool 断连）。
- **内核层**的零 IO 原则是可测试性与可移植性的脊柱：mirri-core 不碰文件系统/网络/时钟，一切经 trait 注入（见 [05-crate-structure.md](05-crate-structure.md) §1.2）。
- MCP 三池跨 App/Workspace/Plugin 三个生命周期，详见 [../mcp-architecture.md](../mcp-architecture.md)；Agent Teams 详见 [../collaboration-requirements.md](../collaboration-requirements.md)。

### 2.1 三条路径，一套协议

| 前端 | 传输 | 引擎接入 | 延迟 | 适用场景 |
|------|------|---------|------|---------|
| **终端 UI** | 无（进程内直连） | `EmbeddedEngine` — Rust 原生 TUI（ratatui）进程内直连引擎 | 零网络开销 | 终端场景（ADR-001：TS TUI 随 TS CLI 退役；ADR-002：未来终端 UI 以 ratatui 原生重构，为 TUI 废掉 Node 运行时、降低资源占用） |
| **Web** | WebSocket | `RemoteEngine` — 浏览器 WS 连接到 Rust server | localhost RTT | 主交互界面（SPA 内嵌于 Rust 二进制），远程访问 |
| **ACP** | stdio | `EmbeddedEngine` — Rust binary 直接嵌入引擎 | 零网络开销 | IDE 集成（Zed/JetBrains），无需 Node.js |

三条路径的线格式完全一致：JSON-RPC 2.0 帧。区别只在传输方式。桌面应用（Electron）复用 Web 路径：壳进程 spawn Rust 二进制作为本地 server，窗口加载同一套 Web UI。

### 2.2 协议帧统一模型

单一连接承载三个方向：

- **正向 RPC**：Client → Server（`session.create` / `message.send` / `tool.call` …）
- **事件流**：Server → Client notification（`assistant.delta` / `tool.result` / `turn.ended` …）
- **反向 RPC**：Server → Client request → Client → Server response（`approval.ask` / `question.ask` / `fs.read` / `terminal.*`）

保留 REST 端点：3 个运维端点（`GET /healthz`、`GET /meta`、`POST /shutdown`）+ 浏览器能力例外端点（OAuth redirect callback、文件上传/下载 — 浏览器 WS 不便处理的场景）。

## 3. 迁移路径

> 阶段划分与各阶段目标如下（阶段化本身是设计约束：每阶段独立可交付可回滚）；**执行状态统一见 [ROADMAP.md](ROADMAP.md)**，本文不维护进度标记。

```
Phase 1              Phase 2                Phase 3              Phase 4
─────────────       ────────              ────────              ────────
Rust core 功能完备 → Web 切到 Rust server → ACP 切到 Rust binary → TS 面整体退役
+ 契约 parity         + 分发接管              （ADR-001：TUI 面      （CLI/TUI + 引擎
  （注册表为事实源）    （install.sh）           随 Phase 4 直接删除，   + NAPI 桥 +
                                               不做 NAPI 迁移）        node-sdk）
                                              后续（ADR-002）：终端 UI
                                              以 ratatui 原生重构
```

| Phase | 目标 | 完成标志 | 风险 |
|-------|------|---------|------|
| **1** | Rust core 功能完备 + 协议统一落地 + 契约 parity | 统一 WS 协议落地；注册表驱动 serve 面；contract-e2e 对拍绿 | — |
| **2** | Web 前端切到 Rust server | Web 默认启用 rust 后端，灰度 + 回滚，REST 业务端点 deprecated | 前端双通道关联逻辑的迁移回归 |
| **3** | ~~TUI + ACP 切到 Rust engine~~（ADR-001 修订：TUI 部分作废，随 TS CLI 直接退役；ACP 切 Rust binary 仍有效） | ACP Rust binary 成为默认（见 [03](plans/03-phase3-tui-acp-migration.md)，NAPI 部分被 ADR-001 取代） | ACP SDK 版本兼容 |
| **4** | TS 面整体退役（ADR-001 扩大范围） | 分发接管完成后，删除 TS CLI/TUI + 引擎包 + NAPI 桥 + SDK 包装；**klient 先迁契约真源再删**（见 [04](plans/04-phase4-ts-retirement.md)） | 回归测试不充分；分发未接管即删除会留下安装空白 |

## 4. 文档索引

| 文档 | 内容 |
|------|------|
| [01-protocol-unification.md](01-protocol-unification.md) | 统一 JSON-RPC 2.0 协议设计：帧格式、方法目录（事实源为 mirri-proto 注册表）、事件目录、反向 RPC、握手、cursor 同步 |
| [05-crate-structure.md](05-crate-structure.md) | crate 结构与依赖关系 |
| [06-cross-platform.md](06-cross-platform.md) | 跨平台抽象：路径编码、Shell、文件监听、IPC 传输 |
| [08-multi-workspace.md](08-multi-workspace.md) | 多工作区架构：WorkspaceRegistry（一进程多工作区）、工作区生命周期与身份、信任门、workspace.*/session.* 契约终态语义 |
| [09-session-title-generation.md](09-session-title-generation.md) | 会话标题生成：轮次内注入 + 标题工具 + 命名守卫 + 兜底链 |
| [10-agent-context-assembly.md](10-agent-context-assembly.md) | 会话代理 system prompt 组装：语言指令 / AGENTS.md / 工作目录披露 |
| [context.md](context.md) | 现状参考（参考类）：当前架构、协议现状、crate 差异、跨平台模块状态 |
| [plans/02-phase2-web-migration.md](plans/02-phase2-web-migration.md) | 迁移计划（计划类）：Web 前端切到 Rust server 的路线与灰度回滚 |
| [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md) | 迁移计划（计划类）：TUI NAPI binding + ACP Rust binary 的 FFI 设计与桥接 |
| [plans/04-phase4-ts-retirement.md](plans/04-phase4-ts-retirement.md) | 迁移计划（计划类）：TS engine 退役的删除清单、klient 契约迁移、回滚方案 |
| [ROADMAP.md](ROADMAP.md) | 执行状态唯一源：契约治理、B6/TS 退役、分发打包、卫生项、真实遗留清单 |

域级 spec（同仓库 docs/specs/）：

| 文档 | 内容 |
|------|------|
| [../mcp-architecture.md](../mcp-architecture.md) | MCP 域架构 RFC：三池连接模型（UserPool/WorkspacePool/PluginPool）、配置分层读写、热更新、OAuth，A1-A5 裁决 |
| [../mcp-requirements.md](../mcp-requirements.md) | MCP 域需求：21 个 user story（GWT 验收）与 NFR |
| [../collaboration-requirements.md](../collaboration-requirements.md) | 多 agent 团队协作生态需求：7 Epics、19 US（角色/任务板/通信交接/仲裁/观测/生命周期/模型统一）与开放裁决点 |

## 5. 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 统一协议 | JSON-RPC 2.0 over WS | 成熟标准，双向，文本帧跨平台 |
| ~~TUI 传输：NAPI FFI~~ | ADR-001 取代 | TS TUI 整体退役；未来终端 UI 以 Rust 原生实现——ratatui 重构、进程内直连、零 Node 运行时（ADR-002） |
| ACP 传输 | stdio (嵌入式 Rust binary) | IDE 无需 Node.js 运行时 |
| REST 保留 | 仅 healthz/meta/shutdown | 运维探活不需要 WS 握手开销 |
| Session 持久化 | wire.jsonl (单向兼容) | 旧 session 一次性迁移，新格式不向后兼容 |
| 路径编码 | POSIX 风格 wire 格式 | 统一约定，OS 边界转换 |

## 6. 验收标准

- Rust core 与 TS v2 引擎通过 contract-e2e fixture 双后端对拍（klient `KLIENT_RESPONSE_SCHEMAS` ⇄ mirri-proto 注册表；pin 数字以注册表内置断言为准）
- ~~TUI 通过 NAPI binding 使用 Rust 引擎时，所有现有功能不回归~~（ADR-001 作废：TUI 面随 TS CLI 退役，不再有 NAPI 消费端）
- Web 前端连接 Rust server 时，所有现有功能不回归
- ACP Rust binary 通过 ACP SDK conformance 测试
- `cargo test --workspace` 全量通过
- `cargo clippy --workspace --all-targets -- -D warnings` 无 warning
- 跨平台：Linux / macOS / Windows 三平台均通过测试

## 7. 决策记录（ADR）

### ADR-001 · 2026-10-12 · TS 前端面整体退役，收敛为单一 Rust 二进制交付面

**背景与动机**（现状盘见 [context.md](context.md)）：

- 引擎单一化后，双语言前端面（TS CLI + 终端 UI）仍要求长期维护：原生绑定桥、事件词表多端翻译同步、按平台的绑定分发、引擎路由开关族——这些在「引擎只此一家」的终态下是纯过渡成本
- 产品的主交互界面已实际收敛为 Rust 二进制内嵌的 Web UI（`mirri web`），终端交互的使用已被 Web 取代
- 长期目标是语言单一性与运行时零 Node.js 依赖

**决策**：

1. **退役范围从「TypeScript 引擎」扩大为「整个 TypeScript CLI 面」**：终端 UI 所在的 CLI 应用、其 TS SDK 包装、以及 NAPI 绑定桥随引擎一并退役，不做「TS 前端长期保留在 NAPI 上」的过渡形态
2. **终端交互面短期空缺**：非交互单次命令兜底；未来终端 UI 以 Rust 原生实现，届时进程内直连引擎、无翻译层（技术选型与动机由 [ADR-002](#adr-002--2026-10-12--终端-ui-以-ratatui-原生重构为-tui-移除-node-运行时) 细化裁决）
3. **交付面收敛为单一 Rust 二进制**：CLI / server / Web UI（内嵌 SPA）一体分发；桌面应用作为 Web 的窗口壳，后端切换为该 Rust 二进制（不再捆绑 Node SEA 后端）；IDE 集成经 ACP
4. **分发接管是退役的硬前置**：Rust 二进制先完成安装渠道接管（Release + manifest + 安装脚本链路），再执行删除——否则删除后无可安装产物
5. **运行时零 Node.js 依赖**覆盖引擎 / CLI / server / 桌面后端；Web 前端源码与构建链属浏览器资产，不在本 ADR 裁决范围

**被否决的备选**：

- **原 Phase 4 范围（保留 TS 终端 UI，经 NAPI 接 Rust 引擎）**：保住终端交互的连续性，但 NAPI 桥、事件翻译层、按平台绑定分发、多端事件词表手工同步成为长期税，与语言单一性目标冲突；且终端面的实际使用已被 Web 取代
- **立即以 Rust 重写终端 UI（约 5 万行量级）**：在 Web 已是主界面的前提下，回归风险与收益不成比例；终端面需求延后到真实需要时再实现

**影响范围**：

- 本文档 §1.1、§2 架构图、§2.1 三路径表（终端 UI 行）、§3 阶段表（Phase 3/4）、§5 决策表（TUI 传输行）、§6 验收（TUI NAPI 条）
- [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md) 的 NAPI 交付物部分（作废）
- [plans/04-phase4-ts-retirement.md](plans/04-phase4-ts-retirement.md) 删除清单（扩大至 CLI 应用、TS SDK 包装、NAPI 桥）
- [ROADMAP.md](ROADMAP.md) 1.3 / 1.4 / 1.5 行与分发线 2.5 行
- klient 契约真源先行迁移的前置不变（contract-e2e 对拍依赖）

### ADR-002 · 2026-10-12 · 终端 UI 以 ratatui 原生重构，为 TUI 移除 Node 运行时

**背景与动机**（细化 ADR-001 第 2 条；现状盘见 [context.md](context.md)）：

- ADR-001 裁决 TS CLI 面整体退役后，终端交互面短期空缺；「未来终端 UI 以 Rust 实现」当时未定技术选型
- 终端场景下 Node 运行时的资源占用（进程内存、启动时间）与单一 Rust 二进制的交付形态不匹配——只要 TUI 还跑在 Node 上，「运行面零 Node.js 依赖」（ADR-001 决策 5）就存在终端侧例外
- 产品需要终端原生交互体验：细粒度渲染、流式输出、事件驱动刷新，这些是 TUI 专用能力，Web 承接不了

**决策**：

1. **技术选型：ratatui**。终端 UI 以 Rust 的 ratatui 库原生重构，继承并重建现 TS TUI 的终端交互面（终端 UI 在本架构中的终态职责不变）
2. **引擎接入：进程内直连（`EmbeddedEngine`）**。TUI 二进制与引擎同进程，无 NAPI 桥、无事件翻译层——与 Web/ACP 三端同源 JSON-RPC 契约的「三路径」模型保持一致，终端路径复用统一协议的帧语义
3. **为 TUI 废掉 Node 运行时**：终端交互路径产品运行面零 Node.js 依赖，达成 ADR-001「引擎 / CLI / server / 桌面后端零 Node」对终端侧的完整覆盖
4. **核心收益 = 减少资源使用量**：去除 Node 进程基线内存与启动开销，终端场景的内存占用与冷启动收敛到与引擎同体的 Rust 进程水位

**被否决的备选**：

- **保留 TS TUI 走 NAPI 桥**：ADR-001 已裁决整体退役；即便保留，Node 运行时基线（进程内存 + 启动）仍在，与资源目标冲突，且 NAPI 桥/事件翻译层为长期维护税
- **终端交互仅由 Web/非交互命令长期承接（不建原生 TUI）**：排除 Node 依赖问题，但终端原生体验（全屏交互、细粒度流式渲染）无法交付，终端用户群失去主交互面
- **crossterm 直写（不用 TUI 框架）**：控制力最强，但现 TUI 的组件密度（多面板、选择器、流式渲染）从零手写成本与长期维护成本过高，收益不成立

**影响范围**：

- 本文档 §2 架构图（终端 UI 行）、§2.1 三路径表（终端 UI 行）、§3 阶段图（Phase 4 后续）、§5 决策表（TUI 传输行）、ADR-001 决策 2（加指针）
- [ROADMAP.md](ROADMAP.md) 1.5 行（状态与说明刷新为 ADR-002 选型）与新增后续项（ratatui TUI 重构执行线，排在 TS 面退役之后）
- 与 [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md) 的 NAPI 设计无冲突：该文件 NAPI 部分已被 ADR-001 作废，ratatui 路径不经 NAPI
