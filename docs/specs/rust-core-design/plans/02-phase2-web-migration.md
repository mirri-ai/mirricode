# Phase 2: Web 前端迁移到 Rust server — 设计文档

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——可随执行推进更新状态；设计主体见 [../00-overview.md](../00-overview.md) 与 [../01-protocol-unification.md](../01-protocol-unification.md)，跨阶段状态汇总见 [../ROADMAP.md](../ROADMAP.md)。
> **路线裁决（持久）**：本文规划的「重写前端 API client 为单 WS」路线未被采用——实际执行转为 **B1-B5 RPC parity 批次**：rust 侧契约全域做实（embedded/serve 契约，Unimplemented 0）、contract-e2e 双后端对拍；web 端保持 v2 双通道直至切换。方法目录以 mirri-proto 注册表为准（`rust/crates/mirri-proto/src/rpc/registry.rs`），MCP 域以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 为准。

> 状态: Draft
> 日期: 2026-08-29
> 作者: Architecture Team
> 依赖: [01-protocol-unification.md](../01-protocol-unification.md)（统一 WS JSON-RPC 2.0 协议设计，本文档的协议依据）
> 前置: [00-overview.md](../00-overview.md)（总体架构与迁移路径）

## 1. 迁移目标

### 1.1 目标陈述

**Web 前端（`apps/mirri-web`）从 TS `kap-server` 切换到 Rust `mirri-server`，功能零回归。**

当前 Web 前端通过双通道与 `kap-server` 通信：

- **REST 通道**（`apps/mirri-web/src/api/daemon/http.ts`）：~60 个端点，覆盖 Session / Message / Prompt / Approval / Question / Task / Terminal / Skill / FS / Workspace / Model / Provider / Config / Agent / Tool / MCP / OAuth / File 全部领域；
- **WS 通道**（`apps/mirri-web/src/api/daemon/ws.ts`）：`server_hello` / `client_hello` 握手、订阅 + cursor（`seq` / `epoch`）同步、`resync_required`、`terminal_*` 帧、`abort` 控制帧。

`mirri-server`（`rust/crates/mirri-server`）**现状（2026-09-04 修订）**：早期为 14 个 REST 端点、16 个 RPC 方法；B1-B5 parity 批次后 **serve 面 154 个方法**（mirri-proto 注册表驱动），统一 WS `/api/v1/rpc`（`ws_rpc.rs`，JSON-RPC 2.0）已落地，WS 已有完整握手与 subprotocol 鉴权；REST 收敛为运维与浏览器能力例外端点。

本 Phase 的目标不是给 Rust server 逐个补齐 60 个 REST 端点，而是**按 `01-protocol-unification.md` 定义的统一 WS JSON-RPC 2.0 协议重写通信层**，让 Web 前端从"REST + WS 双通道"演进为"单 WS JSON-RPC 连接"，同时保证每个现有 UI 功能的行为不变（见 §8 验收标准）。

### 1.2 迁移范围

| | 内容 |
|---|---|
| **In scope** | Rust server 补齐统一 WS JSON-RPC 协议与全部 RPC method；Web 前端 API client 层重写；auth 迁移；wire 格式对齐；事件投影对齐；与 kap-server 并行运行与回滚 |
| **Out of scope** | TUI / ACP 迁移（Phase 3）；TS engine 退役（Phase 4）；Rust core 领域功能本身（其能力已完备，本 Phase 只补齐 server 层的 RPC 表面） |

### 1.3 成功标志（概要，详见 §8）

- 所有 Web UI 功能在 Rust server 后端下可用，与 kap-server 无行为差异；
- `mirri-test-harness` 对两个 server 跑同一组 conformance 测试，事件序列一致；
- 任一时刻可通过配置/开关在双后端间切换，且可随时回滚。

---

## 2. 差距分析

### 2.1 现状对照总览

| 维度 | kap-server（现状，Web 目标） | mirri-server（现状） | 差距 |
|---|---|---|---|
| REST 端点 | ~60 | ~14（历史现状）；serve 面经统一 WS `/api/v1/rpc`（`ws_rpc.rs`）交付 **154 个方法**（mirri-proto 注册表驱动），REST 收敛为运维/浏览器例外端点 | 已闭合（B1-B5 parity 批次，2026-09-04） |
| RPC 方法 | — | **154**（serve 面事实行，2026-09-04；mirri-proto 注册表 pin：embedded 165 / serve 154 / both 140 / 注册表 179 行） | 已闭合（B1-B5 parity 批次） |
| WS 协议 | 握手携带 `heartbeat_ms` / `max_event_buffer_size` / `capabilities`；`client_hello` 带订阅 + cursors；`session_event` 信封带 `seq` / `epoch` / `volatile` / `offset`；`resync_required`；`terminal_*` 帧；`abort` 帧 | 统一 WS `/api/v1/rpc` 已落地（`ws_rpc.rs`，JSON-RPC 2.0，serve 面由 mirri-proto 注册表驱动），完整握手与 subprotocol 鉴权已实现（2026-09-04） | 已闭合（B1-B5 parity 批次；信封/terminal 等差异见 §2.3 历史记录） |
| Wire 格式 | 响应视图 camelCase；请求参数 camelCase；envelope `{code, msg, data, request_id}` | 响应视图 camelCase（`SessionView` 等 `rename_all = "camelCase"`）；RPC 请求参数 snake_case（`session_id`，本 Phase 统一为 camelCase）；envelope 与 TS 一致 | 中（见 §2.4） |
| Auth | REST: `Authorization: Bearer`；WS: `Sec-WebSocket-Protocol: mirri-code.bearer.{token}` | REST: `Authorization: Bearer`（`auth.rs`，constant-time 比较）；WS: subprotocol 鉴权已落地（`ws_rpc.rs`，2026-09-04） | 已闭合（B1-B5 parity 批次） |
| 事件格式 | 原始 agent-core 事件 + 服务端投影 `event.*` 类型 + 信封 | 原始 `SessionEvent`（internally-tagged，`message_received` 等 snake_case 变体），无投影、无信封 | 中（见 §2.6） |

### 2.2 REST 端点差距矩阵

按领域列出 Web 实际使用的每个端点与 Rust server 的对应关系。列含义：**现状状态**（✅ 已有 / ◐ 语义近似 / ❌ 缺失）→ **统一协议中的 RPC method**（命名遵循 `01-protocol-unification.md` 的方法目录，`<domain>.<verb>`）。

> **修订注记（2026-09-04）**：以下缺口矩阵中的大多数条目已被 **B1-B5 RPC parity 批次**闭合——serve 面方法数（事实行）为 embedded 165 / serve 154 / both 140 / 注册表 179 行，方法目录以 mirri-proto 注册表为准（`rust/crates/mirri-proto/src/rpc/registry.rs`）；矩阵本身保留作历史记录。

#### Health / Meta

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /healthz` | ✅ | 保留 REST（运维探活，不需要 WS 握手） |
| `GET /meta` | ◐ | 保留 REST，但需补齐 `capabilities`、`backend: 'rust'`、`open_in_apps`、`dangerous_bypass_auth` 字段（kap-server 已有同款字段，见 `packages/kap-server/src/routes/meta.ts`） |

#### Sessions

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /sessions`、`POST /sessions` | ✅ | `session.list` / `session.create`（已实现） |
| `GET /sessions/:id` | ✅ | `session.get`（已实现） |
| `DELETE /sessions/:id` | ✅ | `session.close`（已实现） |
| `POST /sessions/:id/profile` | ❌ | `agent.get` / `agent.update`（session 的 profile 即其 agent profile） |
| `GET /sessions/:id/status` | ❌ | `session.status`（运行状态、当前 turn、active agents） |
| `GET /sessions/:id/goal` | ❌ | `session.goal`（mirri-core 有 Goal Eng） |
| `GET /sessions/:id/warnings` | ❌ | `session.warnings` |
| `POST /sessions/:id:archive` / `:restore` | ❌ | `session.archive` / `session.restore` |
| `POST /sessions/:id:abort` | ❌ | `message.abort`（WS 控制帧 `abort` 之外的主路径） |
| `POST /sessions/:id:compact` | ❌ | `session.compact`（mirri-core 有 compaction 能力） |
| `POST /sessions/:id:undo` | ❌ | `session.undo` |
| `POST /sessions/:id:fork` | ✅ | `session.fork`（已实现） |
| `POST /sessions/:id/children` | ❌ | `session.children_list`（新增 child session） |
| `GET /sessions/:id/children` | ❌ | `session.children` |
| `POST /sessions/:id:btw` | ❌ | `session.start_btw`（BTW = break-the-wall 子代理） |

#### Messages / Snapshot

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /sessions/:id/messages` | ❌ | `message.list`（分页，Rust core 有持久化上下文） |
| `GET /sessions/:id/snapshot` | ❌ | `session.snapshot`（配合 cursor 协议：`resync_required` → 请求 snapshot 重建，见 §4.1） |

#### Prompts

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `POST /sessions/:id/prompts` | ◐ | `message.send`（已实现，语义等价） |
| `GET /sessions/:id/prompts` | ❌ | `message.list`（prompts 为 legacy 端点，与 `/messages` 收敛同一 RPC） |
| `POST /sessions/:id/prompts:steer` | ❌ | `message.steer` |
| `POST /sessions/:id/prompts/:pid:abort` | ❌ | `message.abort`（WS `abort` 控制帧） |

#### Approvals / Questions

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `POST /sessions/:id/approvals/:aid` | ✅ | `approval.resolve`（已实现） |
| `POST /sessions/:id/questions/:qid` | ✅ | `question.answer`（已实现） |
| `POST /sessions/:id/questions/:qid:dismiss` | ❌ | `question.dismiss` |

#### Tasks

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /sessions/:id/tasks` | ❌ | `task.list`（mirri-core 有 Background Tasks 能力） |
| `GET /sessions/:id/tasks/:tid` | ❌ | `task.get` |
| `POST /sessions/:id/tasks/:tid:cancel` | ❌ | `task.cancel` |

#### Terminals（最大缺口之一）

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET/POST /sessions/:id/terminals` | ❌ | `terminal.list` / `terminal.create` |
| `GET /sessions/:id/terminals/:tid` | ❌ | `terminal.get` |
| `POST /sessions/:id/terminals/:tid:close` | ❌ | `terminal.close` |
| WS `terminal_attach/input/resize/detach/close`、`terminal_output/exit` | ❌ | `terminal.*` 反向 RPC + 事件流（Rust WS 完全没有） |

#### Skills

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /sessions/:id/skills` | ❌ | `skill.list` |
| `GET /workspaces/:id/skills` | ❌ | `skill.list_for_workspace` |
| `POST /sessions/:id/skills/:name:activate` | ❌ | `skill.activate`（mirri-core 有 Skill Eng） |

#### File System（最大缺口之一）

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `POST /sessions/:id/fs:list` | ❌ | `file.list` |
| `POST /sessions/:id/fs:read` | ❌ | `file.read`（反向 RPC 中也可能作为 permission 询问出现） |
| `POST /sessions/:id/fs:search` | ❌ | `file.search` |
| `POST /sessions/:id/fs:grep` | ❌ | `file.grep` |
| `POST /sessions/:id/fs:git_status` | ❌ | `file.git_status` |
| `POST /sessions/:id/fs:diff` | ❌ | `file.diff` |
| `POST /sessions/:id/fs:open` / `:reveal` / `:open-in` | ❌ | `file.open` / `file.reveal` / `file.open_in` |
| `GET /sessions/:id/fs/:path:download` | ❌ | 保留 REST `GET /files/:id` 兜底（二进制下载不适合 JSON-RPC，见 §4.7） |

#### Workspaces

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /workspaces` | ✅ | `workspace.list`（已实现） |
| `POST /workspaces` | ❌ | `workspace.create` |
| `DELETE /workspaces/:id` | ❌ | `workspace.delete` |
| `POST /workspaces/:id/refresh` | ❌ | `workspace.refresh` |
| `PATCH /workspaces/:id` | ❌ | `workspace.update` |

#### Models / Providers

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /models`、`GET /providers` | ❌ | `model.list` / `provider.list`（rust 侧有 `mirri-providers` crate 与 `state.rs` 的 `LlmFactory` 抽象） |
| `POST /providers` | ❌ | `provider.create` |
| `POST /providers:import_catalog` | ❌ | `provider.import_catalog` |
| `DELETE /providers/:id` | ❌ | `provider.delete` |
| `POST /providers/:id:refresh` / `POST /providers:refresh` | ❌ | `provider.refresh` / `provider.refresh_all` |
| `POST /models/:id:delete` | ❌ | `model.delete` |
| `GET /catalog/providers` | ❌ | `provider.catalog` |

#### Config

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /config` | ✅ | `config.get`（已实现） |
| `POST /config` | ❌ | `config.set` |

#### Agents

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET/POST/PUT/DELETE /agents` | ◐ | `agent.list` / `agent.create` / `agent.update` / `agent.delete`（`agent.list` / `agent.get` 已实现，需确认与前端 agent profile 语义一致） |
| `POST /agents/:name:enable` / `:disable` / `:reset` | ❌ | `agent.enable` / `agent.disable` / `agent.reset` |

#### Tools

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /tools` | ✅ | `tool.list`（已实现） |
| `GET /tools/catalog` | ❌ | `tool.catalog` |

#### MCP

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET/POST/PUT/DELETE /mcp/global/*`（历史路由） | — | **MCP 域方法面以新 spec 为准**（[docs/specs/mcp-architecture.md](../../mcp-architecture.md)，三池连接模型）：`mcp.config.list/set/remove/test/setToolFilter` + `mcp.status` + `mcp.auth.start/complete` + `event.mcp.server.status`；本行规划的 `mcp.server_list/toggle_state/mcp.global.*` 等方法提案一律以新 spec 为准，不再作为方法目录依据 |

#### Auth / OAuth

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `GET /auth` | ❌ | `auth.get`（当前凭证、登录状态、`dangerous_bypass_auth`） |
| `POST/GET /oauth/login`、`DELETE /oauth/login`、`POST /oauth/logout` | ❌ | **保留 REST**：OAuth 是浏览器 redirect 流程，`login` 需要 302/新窗口，不适合 JSON-RPC；Rust 侧需要实现 server-side token exchange（`mirri-auth` crate + `packages/kimi-code-oauth` 协议） |

> **修订注记（2026-10-13）**：`packages/kimi-code-oauth` 已随 managed login 退役删除；本行「server-side token exchange」的参照实现现位于 `packages/kosong`（`src/host-identity.ts` 主机身份/设备头、`src/open-platform.ts` + `src/model-alias-merge.ts` 开放平台 API-key 提供商模型合并、`src/custom-registry.ts` + `src/refresh-providers.ts` 私有注册表与模型刷新）。同时 `GET /auth` 与 `/oauth/*` 端点亦已退役移除（见 `packages/kap-server/src/routes/registerApiV1Routes.ts` 头注）。

#### Files

| Endpoint | 状态 | 对应 RPC / 说明 |
|---|---|---|
| `POST /files`、`GET /files/:id` | ❌ | **保留 REST**：multipart 上传与二进制下载（见 §4.7） |

**缺口统计**：~60 端点中 ✅ 8、◐ 2、❌ ~50；❌ 中约 45 个收敛为新的 RPC method，5 个（OAuth 登录/登出、文件上传/下载、`/fs/:path:download` 下载）保留 REST。

### 2.3 WS 协议差距

逐项对比 kap-server WS（`packages/kap-server`，Web 前端当前对接的协议）与 mirri-server WS（`rust/crates/mirri-server/src/ws.rs`）：

| 维度 | kap-server WS（目标） | mirri-server WS（现状） | 差距说明 |
|---|---|---|---|
| `server_hello` | `{heartbeat_ms, max_event_buffer_size, capabilities}` | `{serverId, version}` | 缺 `heartbeat_ms`（前端 fallback 30s）、`max_event_buffer_size`、`capabilities`（能力协商，见 §6） |
| `client_hello` | `{client_id, subscriptions, cursors}` | `{clientId}` | 订阅与 cursor 只能握手后逐个 `subscribe`；断线重连无法恢复订阅水位 |
| 事件信封 | `session_event` 带顶层 `seq` / `epoch` / `volatile` / `offset` | 原始 `SessionEvent`，无任何信封 | 无 seq/epoch → 无法游标续传与去重；无 volatile/offset → 前端 `trackCursor` 逻辑无处生效 |
| 重同步 | `resync_required {session_id, current_seq, epoch}` | 无 | 前端 `ws.ts` 已完整实现 `onResync` 处理（收 sequel 后重新拉 snapshot），server 侧缺失 |
| 控制帧 | `subscribe` / `unsubscribe` / `abort` / `pong` / `terminal_attach|input|resize|detach|close` | `subscribe` / `unsubscribe` / `interrupt` / `ping` | `abort` 缺失（前端会发，Rust 端现在只能打日志）；terminal 帧全缺 |
| 心跳 | server 主动 `ping` | 仅响应 client `ping` | 前端有半开连接 staleness 检测兜底（2× heartbeat），可兼容，但需补齐 server push `ping` 以获得一致行为 |
| RPC over WS | 无 | 无 | 两者都无——统一协议（doc 01）将 RPC 调度引入 WS，这是本 Phase 的核心新增 |
| 反向 RPC | 无（approval/question 靠 REST + 事件轮询） | 无 | 统一协议新增（`approval.ask` / `question.ask` / `terminal.*`），消除轮询 |
| Auth | `Sec-WebSocket-Protocol: mirri-code.bearer.{token}` | **无** | 浏览器 WebSocket API 不能设自定义 header，必须走 subprotocol（前端已用同款 prefix，见 §2.5） |

### 2.4 Wire 格式差异

1. **Envelope**：REST 两侧一致（`{code, msg, data, request_id}`，HTTP 恒为 200）。
   但统一协议将引入 JSON-RPC 2.0 帧（`{jsonrpc, id, result|error}`）——`mirri-server` 的 IPC 路径已用此格式（`rpc.rs` 文档注释），WS 复用同一套 `RpcRegistry` 即可。
2. **请求参数命名**：kap-server REST 用 camelCase（`sessionId`、`promptId`）；Rust RPC params 现状用 snake_case（路由处显式插入 `session_id`，见 `http.rs::session_scoped_params`）。统一协议决策（doc 01）：**wire 全用 camelCase**（与现有 Rust wire 视图及 TS server 一致——params 与 results 均为 camelCase）；前端 `wire.ts` 负责 snake_case↔camelCase 转换（如有需要）。
3. **响应视图**：Rust 已用 camelCase 视图对齐 TS（`SessionView` / `AgentView` / `turn_end_to_json` 等），但覆盖面小。**新增的每个 RPC 必须延续 camelCase 响应视图约定**，并配 `mappers.test.ts` 之类契约测试。
4. **错误码**：Rust `RpcError::json_rpc_code()` 已按 JSON-RPC 2.0 约定（`-326xx` 协议错误、`-32xxx` 应用错误）；kap-server 的错误码语义在 `packages/protocol/error-codes.ts`（前端 `serverAuth.ts` 监听 40101 触发认证弹窗）。统一协议必须对齐错误码枚举，特别是 40101 → `-32002 unauthorized` 的映射，否则登录弹窗逻辑失效。

### 2.5 Auth 机制差异

| | kap-server | mirri-server |
|---|---|---|
| REST | `Authorization: Bearer <token>` | ✅ 一致（`auth.rs`，constant-time 比较、public routes 豁免） |
| WS | `Sec-WebSocket-Protocol: mirri-code.bearer.{token}`（浏览器无法设 header，故走 subprotocol；前端 `ws.ts` 已实现） | ✅ 已落地（`ws_rpc.rs`，subprotocol 鉴权，2026-09-04） |

Rust server 已实现 subprotocol 鉴权（修订 2026-09-04）：握手时解析 `Sec-WebSocket-Protocol` 中 `mirri-code.bearer.` 前缀后的 token，复用 `auth.rs::token_matches` 做 constant-time 比较；失败时以 401 close code 拒绝升级。token 校验失败不得回退为"开放连接"。

### 2.6 事件格式差异

| | kap-server | mirri-server |
|---|---|---|
| 事件类型 | 原始 agent-core 事件（`type` 不以 `event.` 开头）+ 服务端投影的 `event.*` 类型（projected events）混合下发 | 仅原始 `SessionEvent`（internally-tagged，snake_case 变体如 `message_received`、`session_closed`） |
| 信封 | `seq` / `epoch` / `volatile` / `offset` | 无 |
| 前端处理 | `ws.ts::classifyFrame` 三路分拣（protocol / agent / ignore）→ 投影事件走 `onWireEvent`，原始事件走 `onRawAgentEvent` → `agentEventProjector.ts` 客户端投影 → `eventReducer.ts` 归约 | 同一套前端代码，但接收端是 Rust 的原始事件 |

**决策**（与 doc 01 一致）：统一协议固定为**服务端投影 + 信封**——Rust server 新增事件投影层（wire event view），把领域 `SessionEvent` 投影为 `event.*` 类型并附 `seq`/`epoch`/`volatile`/`offset`；前端 `agentEventProjector` 在迁移过渡期保留（兼容分类器），协议稳定后退役。

---

## 3. 迁移策略

### 3.1 Option A（推荐）：统一 WS JSON-RPC 单通道

Rust server 按 doc 01 实现完整 WS JSON-RPC 2.0 协议（正向 RPC + 事件流 + 反向 RPC + cursor 同步）；Web 前端把 API client 层从"REST + WS 双通道"重写为"单 WS JSON-RPC client"；REST 仅保留 `healthz` / `meta` / `shutdown` 3 个运维端点 + 浏览器能力例外端点（OAuth redirect、文件上传/下载）。

```
Web UI (composables / components)
        │  MirriWebApi interface（签名不变）
        ▼
DaemonMirriWebApi ── 重构为单一客户端
        │
        ▼
WS JSON-RPC 2.0（`/api/v1/rpc` 单连接：RPC + events + reverse RPC + cursor）
        │
        ▼
mirri-server ── RpcRegistry（serve 面 154 方法，mirri-proto 注册表驱动；2026-09-04）
```

**优点**
- **总工作量更小**：Option B 需要复刻 ~60 个 URL 路由 + 参数校验 + 错误语义样板；而 RPC method 直接复用/扩展 `RpcRegistry`，与 IPC、未来的 TUI/ACP 共享同一张方法表，新增 method 的边际成本远低于新增 route；
- **消除双通道一致性问题**：现在前端需要 REST（提交/查询）与 WS（事件）两侧对齐（cursor 竞态、`eventBatcher` 调序、`useTaskPoller` 轮询补漏）；单通道后事件与响应同序抵达，轮询逻辑可删除；
- **反向 RPC 成为可能**：`approval.ask`、`question.ask`、`terminal.*` 需要 server → client 方向；REST 架构下 kap-server 只能靠"事件 + 客户端再调 REST"模拟，阶段 2 后 approval/question/terminal 全部改为 server 主动推送，UI 延迟和代码复杂度同时下降；
- **长期架构统一**：Phase 3 的 TUI（NAPI）与 ACP（stdio）将复用同一协议与同一张方法表，Web 先行落地为后续铺路。

**代价**
- 前端 API client 层一次性重写（§5）；
- Rust server 需要新增 WS 协议栈（帧调度、cursor、反向 RPC），比"再加 40 个 handler"复杂；
- 过渡期需要双协议并存（REST 兜底端点 + WS JSON-RPC）。

### 3.2 Option B：REST 端点 1:1 补齐

Rust server 把 ~60 个端点逐个补齐，wire 与 kap-server 完全一致，前端只改 server URL。

**优点**
- 前端改动最小（仅 `config.ts` 的 base URL）；
- 可按端点逐个交付、逐个验证。

**缺点**
- **工作量更大**：45+ 个新增端点 ×（路由 + zod 等价校验 + envelope 错误语义 + 契约测试），且这些样板代码在 Phase 4（TS 退役）后全部作废；
- **无法表达反向 RPC**：approval/question/terminal 的 server → client 推送在 REST 架构下仍要靠事件 + 轮询模拟，架构问题原样保留；
- **双栈长期并存**：REST 路由与 WS 调度各自维护一份方法表，违背 00-overview 的"单协议"目标；Phase 3 TUI/ACP 仍要新建一套客户端，Web 的投资无法复用。

### 3.3 决策：Option A

理由：总工作量更低（RPC 方法表复用 + 前端单客户端替换双客户端）、架构长期一致（与 Phase 3/4 目标同向）、并为反向 RPC 与单连接因果顺序奠定基础。OAuth redirect、multipart 上传、二进制下载三处保留 REST 端点作为**有意的协议例外**（浏览器能力限制，非架构妥协）。

**过渡形态**：迁移期间 mirri-server 保留现有 14 个 REST 端点不删除（灰度期兜底 + 便于 A/B 对拍），新端点一律以 RPC method 形式落在 WS 上。

---

## 4. Rust server 补齐计划（`rust/crates/mirri-server`）

### 4.0 前置：协议落地（doc 01）

按 `01-protocol-unification.md` 实现帧格式、方法目录、事件目录、反向 RPC 目录、握手与 cursor 语义。本小节各项均以该文档为唯一协议真源（source of truth），`packages/protocol` 的 schema 与之对齐（§5.4）。

### 4.1 WS JSON-RPC 2.0 协议实现（重写 `src/ws.rs`）

统一 WS 端点为 `GET /api/v1/rpc`（doc 01 §4.1；旧 `/api/v1/ws` 在迁移期保留旧握手协议）。

| 子项 | 内容 | 落点 |
|---|---|---|
| 端点 | `GET /api/v1/rpc`（WS，统一协议；旧 `/api/v1/ws` 迁移期保留旧协议） | `http.rs` 路由 + `ws.rs` |
| 帧模型 | 三种帧：request（client→server，需响应）、notification（server→client 事件流、client→server 控制）、response（含 `id` 关联）；`jsonrpc: "2.0"` 字段 | `ws.rs` 新增帧解析/序列化；错误码复用 `RpcError::json_rpc_code()` |
| 握手 | `server_hello` 补 `heartbeat_ms` / `max_event_buffer_size` / `capabilities`；`client_hello` 接受 `client_id` / `subscriptions` / `cursors`，按 cursor 回放事件 | `ws.rs::send_server_hello` / `await_client_hello` 重写 |
| cursor 同步 | 每 session 维护 per-connection watermark：事件入队时分配 `seq`（持久化 epoch 由 core 侧提供）；`volatile` 帧（如 `assistant.delta`）不推进持久位点；事件帧附 `epoch` / `offset` | 新增 `src/wscursor.rs`（或并入 `ws.rs`）；core 的 `SessionEvent` 总线前置 seq 分配器 |
| 重同步 | client cursor 落后或未知 → `resync_required {session_id, current_seq, epoch}`；client 随后调 `session.snapshot` 重建 | 订阅入口校验 cursor |
| RPC 调度 | request 帧按 method 走现有 `RpcRegistry`（`rpc.rs`，与 HTTP 路径共享），响应带 `id` 回传 | `ws.rs` 增加 dispatch 分支 |
| 事件分发 | 订阅的 session 事件 → 投影（§4.8）→ notification 帧（带 `session_id` / `seq` / `epoch`） | 复用现有 forward-task 模型，替换帧格式 |
| 反向 RPC | server → client request（`approval.ask`、`question.ask`、`terminal.*`、`file.read` 权限询问）→ client response 回 `ResolveApprovalHandler` / `AnswerQuestionHandler`（已存在） | 新增反向帧通道；approval/question gate 已有，仅需接线 |
| 控制帧 | `abort`（对齐前端已发帧）、`pong`（沿用）、`interrupt`（保留）、`terminal_*` | 并入帧解析 |

### 4.2 RPC 方法补齐（接入现有 `RpcRegistry`）

按 §2.2 映射表补齐。优先级：

- **P0 — 聊天主链路**（Web 第一屏）：`message.list`、`message.steer`、`message.abort`、`session.snapshot`、`session.status`、`session.goal`、`session.warnings`、`session.undo`、`session.compact`、`question.dismiss`、`task.list/get/cancel`
- **P1 — 工作区/文件/设置**：`file.list/read/search/grep/git_status/diff/open/reveal/open_in`、`workspace.create/update/delete/refresh`、`config.set`、`model.list/delete`、`provider.list/create/delete/refresh/refresh_all/import_catalog/catalog`、`agent.create/update/delete/enable/disable/reset`（profile）、`tool.catalog`、`skill.list/list_for_workspace/activate`、`file.upload/download`（REST 兜底）
- **P2 — 扩展能力**：`session.archive/restore/children/children_list/start_btw`、`terminal.list/create/get/close`（依赖 §4.4）、MCP 域（方法面以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 为准：`mcp.config.list/set/remove/test/setToolFilter` + `mcp.status` + `mcp.auth.start/complete` + `event.mcp.server.status`，非本蓝图规划的 `mcp.*` 全套）、`auth.get`、OAuth（REST）

**实现要点**
- 每个新 handler 落在 `rpc.rs`（或按领域拆分 `rpc/` 模块），复用 `mirri-core` 已有领域能力（SessionManager、AgentLifecycle、Skill Eng、Goal Eng、Background Tasks、MCP、providers、auth）；
- **`message.send` 同步 → 异步迁移任务**（§9 风险 10）：现状实现为 300s `spawn_blocking` + `block_on` 同步阻塞直到 turn 结束；协议要求 fire-and-forget——立即返回 `{ok, turnId}`、进度以事件流推送（doc 01 `message.send` 语义）。改造涉及 TurnFlow 状态机、`turn_flows` mutex（同会话并发上限 1）与事件总线接线（遵守"先应答后发布"），列为独立迁移任务、单独验收；
- 响应视图一律 camelCase（§2.4.3），并配序列化契约测试；
- 错误语义对齐 `packages/protocol/error-codes.ts`（§2.4.4）。

### 4.3 WS 鉴权（subprotocol）

- 握手时解析 `Sec-WebSocket-Protocol`，接受值形如 `mirri-code.bearer.<token>`（与前端 `ws.ts` 常量 `WS_BEARER_PROTOCOL_PREFIX` 一致）；
- 复用 `auth.rs::token_matches`（constant-time）；无 token 配置时为开放模式（与 REST 行为一致）；
- 鉴权失败：以 close code 4001 + reason 拒绝升级，不建立连接；token 不得出现在日志中。

### 4.4 Terminal 复用（跨平台 PTY）

- `terminal.create/list/get/close` RPC + WS 数据面（`terminal_output` / `terminal_exit` 事件帧、`terminal_input` / `terminal_resize` 反向帧、attach 时按 `since_seq` 回放）；
- **核心依赖**：Rust 侧缺少 PTY 核心能力（`mirri-core` 尚无任何 PTY 实现）——需要新增 terminal 领域模块与 `PtyProvider` trait，对齐 [06-cross-platform.md](../06-cross-platform.md) 的 trait 化平台抽象模式（如其 `ShellProvider` §3）：trait 定义在业务侧，由 `mirri-os` 提供 Unix `openpty`、Windows ConPTY 实现并注入；进程生命周期与 session 绑定、退出时 `terminal_exit`；
- 这是本 Phase 风险最高的领域（§9），若周期不允许，可先交付 RPC + 帧协议并以"功能未启用"能力位降级（§6.2），UI 按 capabilities 隐藏终端面板。

### 4.5 File System 操作

- 实现 `file.*` 系列 RPC，路径安全复用 `mirri_core::validate_workspace_path`（已存在）与 workspace 根目录约束；
- `file.read` 同时可作为反向 RPC 出现在权限询问中（与 `tool.call` 的 permission chain 一致）；
- `file.open/reveal/open_in` 依赖宿主桌面能力，经 `mirri-os` 抽象；kap-server 用 `getAvailableOpenInApps()` 在 `/meta` 暴露可用性，Rust `/meta` 补同字段（§6.1）。

### 4.6 Model / Provider 管理

- 接入 `mirri-providers` crate 与 `state.rs::LlmFactory`：`provider.create/import_catalog` 写入配置，`provider.refresh/refresh_all/test` 走 provider 探测，`model.list/delete` 别名管理；
- `config.set` 需要 core 的配置变更通知，使运行中 agent 重读配置（与 `Config Watcher` 能力对齐）。

### 4.7 OAuth 与文件上传下载（REST 兜底）

- **OAuth**：`GET /oauth/login`（返回/执行 redirect）、`GET /oauth/callback`（服务端 token exchange）、`POST /oauth/logout`、`DELETE /oauth/login`；凭证落入配置存储；`auth.get`（RPC）供前端读取登录态；
- **Files**：`POST /files`（multipart，大小/类型限制）与 `GET /files/:id`（流式下载 + `Content-Disposition`）；上传后返回 `file_id`，消息引用沿用现有格式。

### 4.8 事件投影层

- 新增 wire projection：`SessionEvent → event.*` 投影 + `seq/epoch/volatile/offset` 信封（§2.6 决策）；
- 投影器与 `packages/protocol/events.ts` 的 schema 对齐（该文件是 TS 侧事件契约，Rust 需有等价定义——建议 Rust 侧生成或双维护 + 契约测试防漂移）；
- 过渡期保留原始事件兼容开关（`capabilities.raw_events: true`），前端 `classifyFrame` 双路解析，灰度完成后关闭。

---

## 5. Web 前端改造计划（`apps/mirri-web`）

### 5.1 核心约束

**`MirriWebApi` interface（`src/api/types.ts`）与 `DaemonMirriWebApi`（`src/api/daemon/client.ts`）的全部公开方法签名保持不变**——composables 与组件层零改动，只动 transport 层。这是零回归的工程前提，也是验收时可以"同一套 UI 代码双后端对跑"的基础。

### 5.2 API client 层：双通道 → 单 WS JSON-RPC client

| 现值 | 改后 |
|---|---|
| `http.ts`（REST 调用，401 处理）+ `ws.ts`（事件订阅）+ 双通道对齐逻辑 | 单一 `JsonRpcClient`：request/response 以 `id` 关联（Promise map）、notification 分发给事件订阅者、反向 RPC 注册回调 handler；HTTP 仅保留 `healthz/meta`、文件上/下载、OAuth redirect |
| `client.ts` 的 ~100 个方法内部各自拼 REST URL | 每个方法改为 `rpc.call(method, params)`，params 组装下沉到 `wire.ts`（camelCase；必要时做 snake_case↔camelCase 转换） |
| `ws.ts::DaemonEventSocket` 手写帧协议（`server_hello`/`subscribe`/`resync_required`/`terminal_*` 分支） | 事件帧统一走 JSON-RPC notification；订阅改用 `client_hello` + `subscribe` request（cursor 语义保留）；`seq/epoch/volatile/offset` 解析逻辑保留并简化（信封由协议固定） |
| `interrupt` / `abort` 控制帧 | `message.abort` RPC request（旧 `interrupt` 帧同样映射 `message.abort`；保留 WS 控制帧兼容分支直到 kap-server 下线） |
| `eventBatcher` + `useTaskPoller` 等轮询/对齐机制 | 单通道因果顺序保证后，轮询逻辑移除；`eventBatcher` 保留（UI 帧率合并） |
| 断线重连：`onclose` → backoff → `client_hello` 带 cursors 恢复 | 语义不变，帧格式换新；`resync_required` → `session.snapshot` rebuild 流程不变（§5.5） |

### 5.3 事件投影适配

- 服务端投影上线后，`agentEventProjector.ts` 的 `classifyFrame` 降级为纯粹的协议类型守卫；客户端投影逻辑（`eventReducer.ts` 的 raw 分支）在灰度期保留、之后删除；
- `mappers.ts`（wire ↔ app 类型）继续作为唯一映射层，只调整输入源。

### 5.4 Auth 与 Wire 对齐

- **Auth**：`serverAuth.ts` 凭证存储逻辑完全复用（fragment token / 输入弹窗 / 7 天 TTL / `onAuthRequired`）；传输层从"REST header + WS subprotocol"统一为"全部走 WS subprotocol（RPC 请求）+ REST 兜底端点仍带 header"；40101 错误码映射到 JSON-RPC `-32002`（§2.4.4）；
- **Wire 格式**：`wire.ts` 负责 snake_case↔camelCase 转换（params 组装与响应解包；wire 全用 camelCase）；`packages/protocol` 演进——新增统一协议 schema（建议 `src/rpc/` 目录 + `ws-control.ts` 扩展），保留旧 schema 直到 kap-server 下线；`mappers.test.ts` / `eventReducer.test.ts` 双后端各跑一遍。

### 5.5 Snapshot / rebuild 适配

- `lib/snapshotSync.ts`（resync 后重建）与 `lib/snapshotMessages.ts`：REST `GET /sessions/:id/snapshot` → `session.snapshot` RPC，返回结构对齐现有 `AppSessionSnapshot`；
- `session.status` / `goal` / `warnings` 等查询从轮询（`useTaskPoller` 类 composable）改为订阅事件 + 按需 RPC。

### 5.6 兼容开关（灰度期）

- `config.ts` 增加 backend 选择（`VITE_MIRRICODE_BACKEND=ts|rust`，§6.3）；
- 客户端按 `/meta` 的 `backend` 字段决定初始化哪套 transport（§6.1），并支持运行中通过 dev toggle 切换（§6.4）。

---

## 6. 兼容性（双后端并行）

Phase 2 期间 kap-server 与 mirri-server 并存，同一份数据目录（session 持久化 `wire.jsonl` 双向可读——00-overview §5 已确认单向兼容策略），同一份配置。

### 6.1 `/meta` backend 标识（客户端检测）

- **mirri-server `/meta` 新增 `backend: "rust"`**，并补齐 `capabilities`（`websocket/file_upload/fs_query/mcp/background_tasks/tasks/terminal`，与 `packages/kap-server/src/protocol/rest-meta.ts` 的 schema 一致）、`open_in_apps`、`dangerous_bypass_auth`；
- **kap-server 继续返回 `backend: "v1"|"v2"`**（现有字段，schema 为 `z.enum(['v1','v2']).optional()`）；
- **Web 客户端判定规则**：`backend === 'rust'` → 统一 WS JSON-RPC client；`'v1'|'v2'|undefined` → 现有双通道 client。两个值域互不重叠，无需改 kap-server。

### 6.2 capabilities 能力协商（灰度基础）

`server_hello.capabilities`（WS）与 `/meta.capabilities`（REST）同源：每个功能域一个能力位（terminal、fs、mcp、oauth、file_upload…）。Rust server 按 §4 的交付进度逐步点亮能力位，Web UI 按能力位隐藏/禁用未就绪功能（kap-server 已是该模式：capabilities 只允许字面量 `true`，未实现即不声明）。**零回归由此保证：未就绪功能在 Rust 后端下以"能力未启用"呈现，而不是报错。**

### 6.3 Feature flags

| Flag | 位置 | 取值 | 用途 |
|---|---|---|---|
| `VITE_MIRRICODE_BACKEND` | `apps/mirri-web` build env | `ts`（默认）/ `rust` / `auto` | 编译期默认后端；`auto` 按 `/meta` 检测 |
| `MIRRICODE_BACKEND` | server env（`mirri-cli` 启动参数） | `ts` / `rust` | 灰度期同一端口由哪个 server 进程接管（配合反向代理） |
| capabilities 能力位 | server 配置 | 每域 on/off | 功能域级灰度（§6.5） |

### 6.4 Dev toggle（开发期）

`src/api/devBackend.ts` + `vite.config.ts` 的 `backendSwitcherPlugin` 已有 `v1`/`v2` 两个 preset（`/__mirri-dev/backend` 切换 dev proxy 上游）。扩展为 `v1` / `v2` / `rust` 三个 preset，Vite dev 下随时一键切换，无需重启前端。

### 6.5 灰度顺序（按功能域，由低风险到高风险）

1. **会话列表 / 聊天主链路**（session、message（含 prompts 端点）、approval、question、task、snapshot、cursor）——P0，灰度面最大；
2. **工作区 / 文件**（workspace、fs）——P1；
3. **设置页**（config、model、provider、agent profile、tool catalog、skill）——P1；
4. **终端**（terminal）——P2，能力位闸门；
5. **MCP / OAuth / 附件**（mcp、oauth、file）——P2（MCP 域方法面以 [docs/specs/mcp-architecture.md](../../mcp-architecture.md) 为准）。

每个域切换 = 对应能力位点亮 + 该域 UI checklist 全绿（§8）。

---

## 7. 测试策略

### 7.1 `mirri-test-harness` 扩展（cross-server conformance）

现有 `rust/crates/mirri-test-harness`（`http_client.rs` / `mock_llm.rs` / `types.rs`）目前只针对一类 server。扩展为**双后端对拍框架**：

- 同一套 scenario 脚本同时跑 kap-server 与 mirri-server（client 抽象为 trait：`HttpClient` / `WsJsonRpcClient` 各一实现）；
- `mock_llm.rs` 已是 deterministic LLM——保证**同一 prompt 在两个 server 产生同一事件序列**（golden 对拍：TS server 事件流录为 golden fixture，Rust server 重放比对 event type / 顺序 / 关键字段）；
- 对比维度：事件序列、snapshot 结构、approval/question 交互序列、terminal 帧序列、错误码。

### 7.2 WS 协议 conformance

独立于功能对拍，按 doc 01 逐项验证 Rust server：

- **握手**：`server_hello` 字段齐全（heartbeat_ms / max_event_buffer_size / capabilities）；错误 token → 拒绝升级；超时断连；
- **cursor**：订阅携带 cursor → 只回放 `> seq` 事件；断线重连带 cursor 无重复无丢失；`volatile` 帧不推进位点；
- **resync**：cursor 落后超阈值 → `resync_required` → `session.snapshot` → 对齐；
- **反向 RPC**：`approval.ask` → `approval.resolve` 闭环；`question.ask` → `question.answer` / `dismiss`；
- **terminal**：attach（含 `since_seq` 回放）→ input → resize → output 流 → exit。

### 7.3 前端测试

- 现有 `mappers.test.ts` / `eventReducer.test.ts` / `modelThinking.test.ts` 等单元测试在"新 wire 格式 + 旧格式"两套 fixture 下双跑；
- WS client 单测：mock `WebSocket`，验证 request/response id 关联、notification 分发、反向 RPC 回调、重连 cursor 恢复；
- e2e（`packages/e2e-harness` / Playwright）：冒烟用例双后端各跑一遍（建 session → 发消息 → approval → 工具调用 → 快照恢复）。

### 7.4 性能回归门槛

- 事件端到端延迟（消息发出 → UI 渲染）：Rust 后端不劣于 kap-server；
- 冷启动：`mirri server` 进程就绪时间计入 CI 对比。

---

## 8. 验收标准

1. **功能零回归**：§6.5 的 5 个功能域 checklist 全绿——Web UI 每个功能对 Rust server 的行为与 kap-server 一致（含错误场景、空态、loading 态）。
2. **cross-server conformance 通过**：§7.1 golden 对拍全部通过；WS 协议 conformance（§7.2）全部通过。
3. **单通道落地**：REST 仅剩 `healthz/meta/shutdown` + OAuth redirect + 文件上/下载；`useTaskPoller` 类轮询逻辑已移除；`MirriWebApi` interface 未变，composables 零改动（git diff 可核验）。
4. **能力协商生效**：任一能力位关闭时对应 UI 以"未启用"呈现而非报错（capabilities 降级冒烟）。
5. **可回滚**：`MIRRICODE_BACKEND=ts` / dev toggle 一键切回 kap-server，全功能正常（§10）。
6. **质量门槛**：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 全绿；前端 CI（lint + unit + e2e）全绿。
7. **性能**：§7.4 门槛通过。

---

## 9. 风险与对策

| # | 风险 | 等级 | 影响 | 对策 |
|---|---|---|---|---|
| 1 | **REST API 表面差距大**（~60 vs ~14），逐个补齐周期长 | 高 | 迁移周期拉长、灰度窗口增加 | Option A 单协议收敛（§3.3）；P0/P1/P2 优先级排序；能力位掩盖未就绪域（§6.2），不做全量补齐即可上线核心链路 |
| 2 | **事件格式差异**（projected vs raw、snake_case vs camelCase、信封字段）导致前端投影/归约行为漂移 | 高 | 重放不一致、UI 状态错乱 | 双后端 golden 对拍（§7.1）；服务端投影 + 兼容期 `classifyFrame` 双路解析（§2.6、§5.3）；`packages/protocol/events.ts` 与 Rust 投影器契约测试防漂移 |
| 3 | **Terminal 实现**（Rust 侧缺 PTY 核心能力，需新增 `PtyProvider` trait（[06-cross-platform.md](../06-cross-platform.md)）；Windows ConPTY vs Unix openpty 行为差异） | 高 | 终端面板不可用/行为不一致 | §4.4 列为独立工作流，`PtyProvider` 经 `mirri-os` 平台抽象注入；交付顺序靠后（P2）；能力位闸门，未就绪时 UI 隐藏终端面板（§6.2）；不阻塞其它域上线 |
| 4 | **OAuth 流程**（浏览器 redirect 语义、token exchange 在 Rust 侧的库/协议支持、登录态与 `auth.get` 一致性） | 中 | 登录/登出失效 | 保持 REST 端点形态（§4.7）；`mirri-auth` crate + `packages/kimi-code-oauth` 参照实现；登录态由 `auth.get` 单一真源，前端逻辑复用 |
| 5 | **反向 RPC 复杂度**（server → client 请求的生命周期、超时、重连后 pending 请求处理、请求与事件并发顺序） | 中 | 卡死/悬挂 approval | 协议层定义 timeout 与取消语义（doc 01）；conformance 覆盖超时与断线场景（§7.2）；复用现有 approval/question gate 的 request-id 机制 |
| 6 | **二进制传输不适合 JSON-RPC**（附件/下载大文件、进度） | 低 | 附件功能退化 | 有意保留 REST 兜底端点（§4.7），文件元数据走 RPC、字节走 HTTP；文档化为协议例外（§3.3） |
| 7 | **双后端并行数据一致性**（同一数据目录被两个 server 交替写） | 中 | session 文件损坏/事件重复 | 运行约束：同一时刻同一数据目录只被一个 server 进程持有（`MIRRICODE_BACKEND` 且反向代理接管端口）；切换前由 `mirri-cli` 做目录锁/优雅停服检查 |
| 8 | **WS 鉴权细节**（subprotocol token 出现在握手日志/代理 AccessLog、长度超限被代理截断） | 中 | 凭证泄露/握手失败 | token 一律脱敏打日志；文档化代理（nginx/caddy）的 header size 配置；复用 `auth.rs` constant-time 比较 |
| 9 | **错误码语义不一致**（40101 vs -32002 等）破坏前端错误处理 | 中 | 登录弹窗不触发、错误提示错位 | §2.4.4 错误码映射表 + `packages/protocol/error-codes.ts` 对齐测试；前端按语义（unauthorized/not_found/…）而非数字判断 |
| 10 | **`message.send` 异步迁移**（现状同步阻塞：300s `spawn_blocking` + `block_on` 直到 turn 结束才返回；协议要求 fire-and-forget——立即返回 `{ok, turnId}`，进度以事件流推送） | 高 | 聊天主链路卡顿/超时语义漂移，双后端行为不一致 | 独立 Phase 2 迁移任务（§4.2）：改造 TurnFlow 状态机、`turn_flows` mutex 与事件总线接线，遵守"先应答后发布"（doc 01）；conformance 覆盖 `{ok, turnId}` 即时返回与事件顺序（§7.2）；双后端 golden 对拍（§7.1） |

> **修订注记（2026-10-13）**：本行「`mirri-auth` crate + `packages/kimi-code-oauth` 参照实现」中 `packages/kimi-code-oauth` 已随 managed login 退役删除；OAuth/token 相关参考实现现位于 `packages/kosong`——`src/host-identity.ts`（主机身份/设备头）、`src/open-platform.ts`（开放平台 API-key 提供商）、`src/custom-registry.ts`（私有注册表）、`src/refresh-providers.ts`（模型刷新）。

---

## 10. 回滚方案

### 10.1 回滚判定条件

任一情形触发回滚（任一功能域）：
- §7.1 golden 对拍失败率超过阈值（或出现事件顺序/结构类不一致）；
- §8 checklist 某功能域未全绿且无能力位掩盖；
- 线上出现 cap-server 不存在的行为差异（性能退化 > 30%、事件丢失、连接悬挂）。

### 10.2 回滚步骤（分钟级）

1. **改配置**：`mirri-cli` 停止 Rust server（优雅停服，持锁释放）；启动 kap-server 进程接管同一端口/数据目录；
2. **改前端**：`VITE_MIRRICODE_BACKEND=ts` 或 `/meta` 检测（Rust server 下线后自动回落到双通道 client）——`auto` 模式无需重新构建；
3. **dev 环境**：`/__mirri-dev/backend` toggle 切回 `v1`/`v2`，秒级生效。

### 10.3 数据兼容保证

- 两个 server 读写同一 `wire.jsonl` 持久化格式（00-overview §5：新格式不与旧格式向后兼容，但迁移策略为一次性迁移 + 回滚窗口内不产生新格式数据）；
- **约束**：灰度期间 Rust server 沿用现有持久化格式（不引入新格式），回滚窗口内数据双向可读；新格式随 Phase 4 统一切换。

### 10.4 灰度回滚粒度

回滚按功能域执行：某一域能力位点亮后若出现回归，仅关闭该功能域的能力位（该域 UI 回到"未启用"），或整域回切 `ts`——避免全量回滚打断已稳定的聊天链路。

---

## 附录 A：术语对照

| 术语 | 含义 |
|---|---|
| kap-server | TS 引擎的 HTTP/WS server（`packages/kap-server`） |
| mirri-server | Rust server（`rust/crates/mirri-server`），本 Phase 的目标后端 |
| unified protocol | doc 01 定义的统一 WS JSON-RPC 2.0 协议 |
| cursor / seq / epoch | 事件流游标：`seq` 会话内单调序号，`epoch` 持久化分代；`resync_required` 在游标落后时触发 |
| projected event | 服务端投影后的 `event.*` 业务事件（相对原始 agent-core 事件） |
| reverse RPC | server → client 方向的 JSON-RPC request（approval/question/terminal） |
| capabilities | `/meta` 与 `server_hello` 中的能力位地图，UI 据此呈现功能 |

## 附录 B：相关代码路径索引

| 关注点 | 路径 |
|---|---|
| Rust server REST 路由 | `rust/crates/mirri-server/src/http.rs` |
| Rust server RPC 方法表 | `rust/crates/mirri-server/src/rpc.rs` + `rust/crates/mirri-proto/src/rpc/registry.rs`（方法目录以注册表为准：serve 面 154 / 注册表 179 行） |
| Rust server WS 协议 | `rust/crates/mirri-server/src/ws_rpc.rs`（统一 WS `/api/v1/rpc`） |
| Rust server 鉴权 | `rust/crates/mirri-server/src/auth.rs` |
| Rust server 测试 | `rust/crates/mirri-server/tests/http_tests.rs`、`tests/ws_tests.rs` |
| Web API 入口/配置 | `apps/mirri-web/src/api/index.ts`、`api/config.ts` |
| Web 双通道 client | `apps/mirri-web/src/api/daemon/client.ts`、`http.ts`、`ws.ts` |
| Web wire/映射/投影 | `apps/mirri-web/src/api/daemon/wire.ts`、`mappers.ts`、`eventReducer.ts`、`agentEventProjector.ts` |
| Web 凭证存储 | `apps/mirri-web/src/api/daemon/serverAuth.ts` |
| Web dev 后端切换 | `apps/mirri-web/src/api/devBackend.ts` + `vite.config.ts` 的 `backendSwitcherPlugin` |
| 共享协议 schema | `packages/protocol/src/`（`rest/*`、`ws-control.ts`、`events.ts`、`envelope.ts`、`snapshot.ts`、`error-codes.ts`） |
| kap-server `/meta` | `packages/kap-server/src/routes/meta.ts`、`src/protocol/rest-meta.ts` |
| cross-server 测试框架 | `rust/crates/mirri-test-harness/`（`http_client.rs`、`mock_llm.rs`、`types.rs`） |

---

## 修订补记（2026-09-04）：本蓝图下的实际剩余工作

- **Web 前端默认启用 Rust 后端**：「重写前端 API client 为单 WS」未被采用；`apps/mirri-web` 已新增 `json-rpc-client.ts` / `ws-rpc-api.ts`（`src/api/daemon/`）但**未默认启用**——默认启用为真实剩余工作；
- **灰度与回滚执行**：§6.5 灰度顺序与 §10 回滚方案尚未执行；
- 一致性保障已由 B1-B5 批次的 contract-e2e 双后端对拍承担（本蓝图 §7.1 的对拍思路以 contract-e2e 形式落地）。
