# 统一 JSON-RPC 2.0 协议设计（Rust Core）

> 状态: Draft · 原稿日期: 2026-08-30 ·
> 作者: Architecture Team
> 上游文档: [00-overview.md](00-overview.md)
> 关联文档: [plans/02-phase2-web-migration.md](plans/02-phase2-web-migration.md) · [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md) · [plans/04-phase4-ts-retirement.md](plans/04-phase4-ts-retirement.md) ·
> **执行状态不在本文维护**：阶段进度统一见 [ROADMAP.md](ROADMAP.md)

本文档是 Mirri Code Rust core 引擎的统一协议规范（Protocol v1.0）。它定义**唯一的**客户端 ↔ 引擎交互契约：

- 帧格式（JSON-RPC 2.0，含 JSON Schema 定义）
- 握手与能力协商
- 正向 RPC 方法目录（按领域组织；**§6.23 目录为设计期提案快照，方法面的落地事实源是 `rust/crates/mirri-proto/src/rpc/registry.rs` 注册表**——命名与计数差异以注册表为准）
- 事件目录（含 `volatile` 标记）
- 反向 RPC（服务端发起、客户端应答）
- Cursor 同步与 resync 协议
- 错误码体系
- REST 保留端点
- 迁移策略与安全考虑
- MCP 域以 [../../mcp-architecture.md](../mcp-architecture.md) 为准（本文 §6.8/§7.10 为其协议视角摘要）

所有协议定义使用 **JSON Schema（draft-07）**，不使用代码。Schema 中出现的 `x-volatile`、`x-role` 等 `x-` 前缀关键字是本文档定义的**标注（annotation）**，不影响校验语义。

---

## 1. 设计目标

### 1.1 现状与痛点（动机摘要）

统一协议设计启动前，Rust core 对外暴露 3 种互不对等的传输（REST 信封 / 旧 WS 帧 / IPC JSON-RPC），产生协议碎片化、双连接依赖、`message.send` 同步阻塞、审批通道割裂、IPC 格式孤立五类问题（三传输现状表与问题详盘见 [context.md](context.md) §2）。本文档定义取代它们的单协议终态。

### 1.2 目标

- **单连接、三方向**：所有交互（正向 RPC、事件流、反向 RPC）跑在同一条 WS JSON-RPC 2.0 连接上，客户端不再维护多通道关联。
- **一套方法目录**：Web / TUI / ACP 三种前端看到的是同一份协议目录，能力分布天然对齐。
- **消息异步化**：`message.send` 立即返回 `turnId`，进度以事件流推送；`wait` 模式仅作为简单客户端的兼容选项。
- **审批/提问同连接化**：approval 与 question 通过反向 RPC 在同一连接上请求与应答，请求 id 直接关联来源 turn。
- **可恢复**：`{seq, epoch}` cursor 同步 + `resync_required` + `session.snapshot`，断线重连后状态可重建。
- **REST 收缩到 3 个运维端点 + 浏览器能力例外端点**（OAuth redirect、文件上传/下载）：`GET /healthz`、`GET /meta`、`POST /shutdown`。

### 1.3 非目标（本期明确不做）

- 不做二进制帧编码（保持 JSON 文本帧，便于调试与跨语言）。
- 不做 pub/sub 服务发现；会话订阅是连接级语义，不引入 broker。
- 不做多服务端集群同步；cursor 同步仅覆盖单服务端实例内的重连 / 缓冲溢出场景。

---

## 2. 协议总览

### 2.1 一条连接，三个方向

```
┌─────────────────────── 客户端 (Web / TUI / ACP Host) ───────────────────────┐
│                                                                             │
│   ① 正向 RPC (Request/Response)         ③ 反向 RPC (Server-initiated)      │
│      client ──request──▶ server            server ──request──▶ client      │
│      client ◀─response── server            client ◀─response── server      │
│                                                                             │
│   ② 事件流 (Notification, server → client)                                  │
│      server ──notification──▶ client   （assistant.delta / turn.started …） │
│                                                                             │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │  同一条传输（WS / stdio / FFI）
┌──────────────────────────────────▼──────────────────────────────────────────┐
│                        Rust Engine Core（统一协议层）                        │
│       正向 RPC 分发 │ 事件发布 │ 反向 RPC 调度 │ cursor 记账 │ resync       │
└──────────────────────────────────────────────────────────────────────────────┘
```

三种帧在 JSON-RPC 2.0 中的表达方式：

| 方向 | JSON-RPC 表达 | 是否带 `id` | 示例 |
|------|--------------|------------|------|
| 正向 RPC | Request | 必带（string UUID 或 integer） | `{"jsonrpc":"2.0","id":"r-1","method":"message.send","params":{...}}` |
| 正向 RPC 应答 | Response | 回显请求 id | `{"jsonrpc":"2.0","id":"r-1","result":{...}}` 或 `{"error":{...}}` |
| 事件流 | Notification | 不带 | `{"jsonrpc":"2.0","method":"turn.started","params":{...}}` |
| 反向 RPC | Request（服务端发出） | 必带 | `{"jsonrpc":"2.0","id":"rr-7","method":"approval.ask","params":{...}}` |
| 反向 RPC 应答 | Response（客户端发出） | 回显请求 id | `{"jsonrpc":"2.0","id":"rr-7","result":{"approved":true}}` |

**判定规则只有一条**：带 `id` 的是 RPC（必须应答），不带 `id` 的是通知（永不应答，单向）。事件与反向 RPC 由此自然区分，不需要额外字段。

### 2.2 术语约定

| 术语 | 含义 |
|------|------|
| 帧 (frame) | 一条 WS 消息 / stdio 一行 / 一次 FFI 调用的完整 JSON 值 |
| 正向 RPC (forward RPC) | 客户端发起的请求-应答 |
| 反向 RPC (reverse RPC) | 服务端发起的请求-应答（客户端必须应答） |
| 事件 (event) | 服务端发出的通知；`method` 即事件名（`turn.started` 等） |
| 通知 (notification) | 不带 `id` 的任意消息（事件、`ping`、`session.subscribe` 等） |
| cursor | `{seq, epoch}`，会话事件日志的单调游标 |
| volatile 事件 | 不落日志、不推进 seq、重连后不可重放的事件（如 `assistant.delta`） |
| durable 事件 | 落日志、带 `seq`、可重放的事件 |

### 2.3 与会话模型的关系

- 所有事件与反向 RPC 都携带 `sessionId`（连接级控制帧除外，如 `ping`）。
- 一条连接可同时挂载多个会话：`session.subscribe {sessionId}` 订阅后，该会话的 durable 事件按 cursor 顺序推送；未订阅的会话不推送（但反向 RPC 不受订阅限制——审批属于全局连接语义）。
- **全局扇出例外**：少数会话级 durable 事件（目录中以"全局扇出"标注，如 `session.work_changed`）绕过订阅门控，推送到每条连接——它们承载列表级状态（不打开会话也需要感知），逐会话订阅模型对这类信息天然欠覆盖。全局扇出事件仍按 cursor 顺序落日志与重放。
- cursor 是**按会话**的，不是全局的（多会话并发时各自独立推进，避免全局互锁）。

---

## 3. 传输层

三条传输承载**完全相同的 JSON-RPC 2.0 帧**，区别仅在传输方式与成帧规则。

### 3.1 WebSocket（主要传输，浏览器与远程访问）

- 端点：`/api/v1/rpc`（新端点；旧 `/api/v1/ws` 在迁移期保留旧协议，见 §12）。
- 帧：每条 WS 消息 = 一个 JSON 值（对象或数组）。默认一消息一帧；协商 `event_batching` 能力后，服务端可发送**数组帧**（JSON-RPC 2.0 batch），把多条事件合并为一条 WS 消息。
- 子协议：浏览器客户端必须用 `Sec-WebSocket-Protocol` 携带 token（`mirri.bearer.<token>`），详见 §13。
- 心跳：应用层 `ping`/`pong` 通知（§5.4），不依赖 WS 协议层 ping/pong。
- 帧大小上限：默认 16 MiB（`maxMessageSize`，握手时协商），超限即断开并视为协议违规。

### 3.2 stdio（ACP 传输）

- 形态：Rust core 以子进程形态被 ACP 宿主（IDE 集成）拉起，`stdin`/`stdout` 为帧通道，`stderr` 只写日志（**严禁**混入协议帧）。
- 成帧：行分隔 JSON（NDJSON）——与现有 Unix socket IPC 一致；每条协议帧序列化为单行（不含换行符），以 `\n` 结尾。二进制内容（如 `file.download`）一律 base64 内嵌，避免混合成帧。
- 方向：与 WS 完全相同——宿主即客户端，core 即服务端，三方向全支持（ACP 的 `fs.readTextFile` 等文件代理即反向 RPC，见 §8.3）。
- 生命周期：宿主拉起 → core 打 `stdout` 写 `server_hello` → 宿主回 `client_hello` → 进入正常协议循环 → 任一方关闭 stdin / 退出即断连。`POST /shutdown` 的语义在 stdio 下变为「收到反向请求 `process.shutdown` 后优雅退出」（内核实现细节见 [06-cross-platform.md](06-cross-platform.md)）。

### 3.3 FFI（TUI 传输）

- 形态：Rust core 编译为 cdylib（或 NAPI binding），TUI 进程内直接调用，零网络开销（见 [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md)）。
- 接口面（3 个函数，均为 C ABI）：
  - `mirri_rpc_call(request_json: *const c_char) -> *mut c_char` — 同步调用，入参为**请求帧 JSON**，返回**响应帧 JSON**（含 `error` 形态）；调用方必须 `mirri_rpc_free` 释放。
  - `mirri_set_event_callback(cb: extern "C" fn(*const u8, usize))` — 注册事件回调；服务端（引擎）产生事件/反向 RPC 帧时，以**帧 JSON 的 UTF-8 字节**回调。
  - `mirri_call_reverse_response(response_json: *const c_char)` — 客户端应答反向 RPC（即 §8 中的 Response 帧）。
- 语义等价性：FFI 下没有「断连」，但协议层（握手、cursor、订阅）**照常执行**，保证三种传输行为一致——TUI 在握手后收到 `server_hello`，同样需要回 `client_hello`。
- 事件回调重入规则：回调在引擎线程触发，禁止在回调内同步调用 `mirri_rpc_call`（会死锁）；需应答反向 RPC 时，把响应帧交给 `mirri_call_reverse_response` 异步处理。
- 内存约定：所有跨 FFI 边界的字符串均为 UTF-8、`\0` 结尾的 C 字符串（JSON 序列化自带转义，不存在换行/二进制歧义）。

### 3.4 传输对比

| 属性 | WebSocket | stdio (ACP) | FFI (TUI) |
|------|-----------|-------------|-----------|
| 帧边界 | 每条 WS 消息 | 每行 | 每次函数调用 / 每次回调 |
| 批处理 | 数组帧（协商后） | 不支持（一行一帧） | 不支持 |
| 反向 RPC | ✅ | ✅ | ✅（回调 + `mirri_call_reverse_response`） |
| 断线语义 | 重连 + cursor 恢复 | 进程退出即重建 | 无断线 |
| 认证 | Bearer token（§13） | 父进程令牌（环境变量/argv） | 同进程，无认证 |
| 压缩 | 协商 `permessage-deflate` 或帧级 gzip | 无 | 无 |

关键约束：**任何传输都不得改变帧的内容与语义**。同一份 `message.send` 请求帧，在三条传输上含义完全一致。

---

## 4. 帧格式

### 4.1 基础帧

#### 4.1.1 Request（请求帧）

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://mirri.dev/protocol/v1/frame-request",
  "title": "JSON-RPC 2.0 request frame",
  "type": "object",
  "required": ["jsonrpc", "id", "method"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "id": {
      "description": "请求标识。推荐 string（UUID）以避免整数溢出；服务端响应原样回显。",
      "anyOf": [
        { "type": "string", "maxLength": 64 },
        { "type": "integer" }
      ]
    },
    "method": {
      "description": "点分小写方法名或事件名，如 session.create、turn.started。",
      "type": "string",
      "pattern": "^[a-z][a-z0-9]*(\\.[a-z][a-z0-9]*)+$",
      "maxLength": 128
    },
    "params": {
      "description": "结构化参数对象。无参数时可不传或传 null（服务端按空对象处理）。",
      "type": ["object", "null"],
      "additionalProperties": true
    }
  },
  "additionalProperties": false
}
```

#### 4.1.2 Response（响应帧）

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://mirri.dev/protocol/v1/frame-response",
  "title": "JSON-RPC 2.0 response frame",
  "type": "object",
  "required": ["jsonrpc", "id"],
  "oneOf": [
    {
      "title": "成功响应",
      "required": ["result"],
      "properties": {
        "jsonrpc": { "const": "2.0" },
        "id": {},
        "result": { "type": ["object", "array", "string", "number", "boolean", "null"] }
      },
      "additionalProperties": false
    },
    {
      "title": "错误响应",
      "required": ["error"],
      "properties": {
        "jsonrpc": { "const": "2.0" },
        "id": {},
        "error": { "$ref": "https://mirri.dev/protocol/v1/error-object" }
      },
      "additionalProperties": false
    }
  ]
}
```

#### 4.1.3 Notification（通知帧）

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://mirri.dev/protocol/v1/frame-notification",
  "title": "JSON-RPC 2.0 notification frame (events, control)",
  "type": "object",
  "required": ["jsonrpc", "method"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "method": {
      "description": "事件名或控制方法名；不带 id，永不收到应答。",
      "type": "string",
      "pattern": "^[a-z][a-z0-9]*(\\.[a-z][a-z0-9]*)+$"
    },
    "params": { "type": ["object", "null"] }
  },
  "additionalProperties": false
}
```

#### 4.1.4 Batch（数组帧，仅 WS，协商后启用）

```json
{
  "$id": "https://mirri.dev/protocol/v1/frame-batch",
  "title": "JSON-RPC 2.0 batch frame (event batching)",
  "type": "array",
  "minItems": 1,
  "maxItems": 256,
  "items": {
    "anyOf": [
      { "$ref": "https://mirri.dev/protocol/v1/frame-notification" },
      { "$ref": "https://mirri.dev/protocol/v1/frame-request" }
    ]
  },
  "description": "仅服务端 → 客户端方向支持事件批处理；数组内不带 id 的成员即事件。客户端 → 服务端禁止发送数组帧（保持简单）。"
}
```

### 4.2 Error 对象

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://mirri.dev/protocol/v1/error-object",
  "title": "JSON-RPC 2.0 error object",
  "type": "object",
  "required": ["code", "message"],
  "properties": {
    "code": {
      "description": "整数错误码：-327xx 协议错误，-326xx JSON-RPC 保留，-32000..-32099 应用错误。详见 §10。",
      "type": "integer"
    },
    "message": { "type": "string", "maxLength": 512 },
    "data": {
      "description": "可选详情。约定字段（camelCase）：errorId（内部错误关联 id）、domainCode（引擎领域错误码，如 provider_error）、retryAfterMs（限流）、details（附加结构化信息）。",
      "type": "object"
    }
  },
  "additionalProperties": false
}
```

### 4.3 解析与处理规则

1. **解析失败**（非合法 JSON）→ 回 `-32700`，`id: null`。
2. **结构非法**（缺 `method`、缺 `id` 或 `id` 类型非法、带 `id` 但缺 `method`）→ 回 `-32600`；**不带 `id` 的非法帧直接丢弃**（通知无应答，符合 JSON-RPC 2.0）。
3. **未知方法** → `-32601`。
4. **参数校验失败** → `-32602`。
5. 请求处理顺序：**先应答后发布**——`message.send` 等写操作必须先落响应再发事件，保证客户端不会在收到 `turnId` 之前收到 `turn.started`（同一条连接天然保证发送顺序，但事件可能由其他驱动源并发产生，故显式要求 handler 内先响应后 publish）。
6. 服务端内部错误强制携带 `data.errorId`（UUID），客户端报障时回传该 id 即可关联服务端日志（沿用现有 `RpcError::internal` 约定）。

---

## 5. 握手协议

握手沿用现有 `server_hello` / `client_hello` 名称（兼容心智模型），但升级为 JSON-RPC 帧并加入**能力协商**。

### 5.1 时序

```
客户端                          服务端
  │                               │
  │◀──── server_hello (通知) ──────┤  升级/建连后立即（服务器身份 + 能力）
  │                               │
  │──── client_hello (请求) ─────▶│  10s 内必须发出，否则服务端断开
  │◀──── client_hello 响应 ───────┤  协商结果 + 客户端初始 cursor
  │                               │
  │──── session.subscribe ───────▶│  订阅会话，开始接收事件
  │◀──── subscribed 确认 ─────────┤
```

- `server_hello` 是**通知**（服务端无需应答）。
- `client_hello` 是**请求**（带 `id`），服务端必须应答，应答中带回协商结果——这样握手成功的信号是「收到了 client_hello 的响应」，客户端无需再等第三个消息。

### 5.2 server_hello（服务端 → 客户端，通知）

```json
{
  "$id": "https://mirri.dev/protocol/v1/server-hello",
  "title": "server_hello notification",
  "type": "object",
  "required": ["jsonrpc", "method", "params"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "method": { "const": "server_hello" },
    "params": {
      "type": "object",
      "required": ["serverId", "version", "protocolVersion", "capabilities"],
      "properties": {
        "serverId": { "type": "string", "description": "服务端实例 id，重连后可据此识别 epoch 是否变化" },
        "version": { "type": "string", "description": "引擎版本，如 0.28.0" },
        "protocolVersion": { "const": 1, "description": "协议版本。不匹配时客户端应拒绝连接或降级" },
        "sessionId": { "type": "string", "description": "本连接的服务端会话 id（连接级，非业务 session），用于日志关联" },
        "capabilities": { "$ref": "https://mirri.dev/protocol/v1/capabilities" },
        "maxMessageSize": { "type": "integer", "minimum": 65536, "description": "帧大小上限（字节），默认 16777216" },
        "heartbeatIntervalMs": { "type": "integer", "minimum": 5000, "description": "建议心跳间隔，默认 15000" }
      },
      "additionalProperties": false
    }
  }
}
```

### 5.3 client_hello（客户端 → 服务端，请求）与应答

```json
{
  "$id": "https://mirri.dev/protocol/v1/client-hello",
  "title": "client_hello request",
  "type": "object",
  "required": ["jsonrpc", "id", "method", "params"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "id": { "type": ["string", "integer"] },
    "method": { "const": "client_hello" },
    "params": {
      "type": "object",
      "required": ["clientId", "clientType"],
      "properties": {
        "clientId": { "type": "string", "description": "客户端实例 id（UUID）。同一客户端重连应复用，服务端据此判断断线重连" },
        "clientType": {
          "type": "string",
          "enum": ["web", "desktop", "tui", "acp", "cli"],
          "description": "客户端形态，影响能力默认值与审计日志"
        },
        "capabilities": { "$ref": "https://mirri.dev/protocol/v1/capabilities" },
        "cursors": {
          "description": "客户端已消费的会话游标。服务端据此重放未消费的 durable 事件（§9）。",
          "type": "object",
          "additionalProperties": { "$ref": "https://mirri.dev/protocol/v1/cursor" }
        },
        "resumeConnectionId": {
          "type": "string",
          "description": "断线重连时携带旧连接的 ws_connection_id，服务端可复用订阅集合；服务端可忽略"
        }
      },
      "additionalProperties": false
    }
  }
}
```

**应答（成功）**：

```json
{
  "$id": "https://mirri.dev/protocol/v1/client-hello-response",
  "title": "client_hello response (success)",
  "type": "object",
  "required": ["jsonrpc", "id", "result"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "id": {},
    "result": {
      "type": "object",
      "required": ["connectionId", "heartbeatIntervalMs"],
      "properties": {
        "connectionId": { "type": "string", "description": "连接 id，服务端日志与此连接关联" },
        "heartbeatIntervalMs": { "type": "integer", "description": "协商后的心跳间隔；客户端应按此节奏发 ping" },
        "acceptedCapabilities": { "$ref": "https://mirri.dev/protocol/v1/capabilities", "description": "∩ 后的能力协商结果" },
        "replayedEvents": {
          "type": "array",
          "description": "按 cursors 重放的 durable 事件（可能为空；superset 大时服务端可改为直接发 resync_required）",
          "items": { "$ref": "https://mirri.dev/protocol/v1/frame-notification" }
        },
        "resyncedSessions": {
          "type": "array",
          "description": "游标已失效、需要客户端重建的会话列表",
          "items": { "type": "string" }
        }
      },
      "additionalProperties": false
    }
  }
}
```

**错误**：`-32002` 未授权（token 无效）、`-32602` 参数非法、`-32004` 限流（握手频率超限）、`-32601` 协议版本不兼容（此时 `data.protocolVersion` 携带服务端版本）。

### 5.4 能力协商

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://mirri.dev/protocol/v1/capabilities",
  "title": "capabilities negotiation object",
  "type": "object",
  "properties": {
    "eventBatching": {
      "type": "boolean",
      "description": "服务端是否可将多条事件合并为数组帧发送（§4.1.4）",
      "x-role": "server-offered"
    },
    "compression": {
      "type": "boolean",
      "description": "是否启用帧级 gzip（当前仅限 WS；标识位由服务端声明、客户端可按需启用）",
      "x-role": "server-offered"
    },
    "reverseRpc": {
      "type": "array",
      "items": { "type": "string", "pattern": "^[a-z]+$", "description": "单字段域前缀（如 \"approval\"、\"question\"、\"fs\"、\"terminal\"）" },
      "description": "客户端支持应答的反向 RPC 方法前缀白名单，如 [\"approval\", \"question\", \"fs\", \"terminal\"]。服务端不得发起不在白名单内的反向 RPC",
      "x-role": "client-declared"
    },
    "terminalMode": {
      "type": "string",
      "enum": ["engine", "host"],
      "description": "PTY 所有权：engine = 引擎创建并管理终端（正向 terminal.create）；host = 宿主提供终端（反向 terminal.create，ACP 场景）。协商规则见下",
      "x-role": "negotiated"
    },
    "maxEventBufferSize": {
      "type": "integer",
      "minimum": 64,
      "description": "服务端为每条连接保留的事件缓冲上限；客户端落后超过此值即收到 resync_required(buffer_overflow)",
      "x-role": "server-offered"
    }
  },
  "additionalProperties": false
}
```

**协商规则**：

1. 服务端在 `server_hello` 中声明能力（`x-role: server-offered` 与 `negotiated` 字段），客户端在 `client_hello` 中声明需求（`client-declared`）。
2. 结果取**交集**：`server.capabilities.X && client.capabilities.X`（boolean 类）；`terminalMode` 冲突时服务端优先使用自身的 `engine` 模式并以应答告知（ACP 宿主声明 `host` 时采用 `host`）。
3. 未协商成功的能力默认关闭；客户端不得假设服务端接受了未声明的能力。
4. 能力变化（如服务端热更新）→ 服务端发 `config.changed` + `capabilities_changed` 事件（§7.11），客户端应重新评估，必要时重连。

### 5.5 心跳

- 客户端以协商的 `heartbeatIntervalMs`（默认 15s）为周期发送 `ping` **通知**：

```json
{ "jsonrpc": "2.0", "method": "ping", "params": { "nonce": "abc123" } }
```

- 服务端必须立即回 `pong` **通知**（回显 nonce）：

```json
{ "jsonrpc": "2.0", "method": "pong", "params": { "nonce": "abc123" } }
```

- 超时判定：任一方超过 `3 × heartbeatIntervalMs` 未收到任何帧（不只是 pong），即判定对端失联并断开。
- 客户端 UI 指示：连续 `2 × heartbeatIntervalMs` 未收到任何帧时，UI 应显示「reconnecting...」指示；`3 × heartbeatIntervalMs` 为硬断开阈值（与上条超时判定一致）。
- 服务端也主动发 `ping`（带自己的 nonce），客户端必须回 `pong`——双方向心跳保证 FFI/stdio 下宿主进程「假死」可被检测。---

## 6. 正向 RPC 方法目录

### 6.0 公共定义与约定

以下类型在本章所有 schema 中复用（引用 `#/$defs` 时指各方法 schema 内的内联副本，文档层面为共享语义）：

```json
{
  "$id": "https://mirri.dev/protocol/v1/defs",
  "title": "shared type definitions",
  "$defs": {
    "SessionId":   { "type": "string", "minLength": 8, "maxLength": 64, "description": "会话 id" },
    "AgentId":     { "type": "string", "maxLength": 64, "description": "agent id" },
    "TurnId":      { "type": "string", "description": "回合 id（UUID）" },
    "RequestId":   { "type": "string", "description": "审批/提问请求 id（UUID）" },
    "TaskId":      { "type": "string", "description": "任务 id（UUID）" },
    "Timestamp":   { "type": "string", "format": "date-time", "description": "RFC 3339" },
    "PosixPath":   { "type": "string", "description": "POSIX 风格路径（wire 统一格式，跨平台转换见 06-cross-platform.md）" },
    "Ok":          { "type": "object", "required": ["ok"], "properties": { "ok": { "const": true } }, "additionalProperties": false },
    "Cursor":      { "type": "object", "required": ["seq"], "properties": { "seq": { "type": "integer", "minimum": 0 }, "epoch": { "type": "string" } }, "additionalProperties": false },
    "Pagination":  { "type": "object", "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 50 }, "cursor": { "type": "string", "description": "不透明分页游标" } }, "additionalProperties": false },
    "ToolInputDisplay": {
      "oneOf": [
        { "type": "object", "required": ["kind", "command"], "additionalProperties": false,
          "properties": { "kind": { "const": "command" }, "command": { "type": "string" } } },
        { "type": "object", "required": ["kind", "path", "operation"], "additionalProperties": false,
          "properties": { "kind": { "const": "file_io" }, "path": { "type": "string" },
            "operation": { "type": "string", "enum": ["read", "write", "edit", "glob", "grep"] } } },
        { "type": "object", "required": ["kind", "path", "before", "after"], "additionalProperties": false,
          "properties": { "kind": { "const": "diff" }, "path": { "type": "string" },
            "before": { "type": "string", "description": "替换前文本（Edit 的 old_string）" },
            "after": { "type": "string", "description": "替换后文本（Edit 的 new_string）" } } },
        { "type": "object", "required": ["kind", "query"], "additionalProperties": false,
          "properties": { "kind": { "const": "search" }, "query": { "type": "string" },
            "scope": { "type": "string", "description": "搜索范围路径，缺省省略" } } },
        { "type": "object", "required": ["kind", "url"], "additionalProperties": false,
          "properties": { "kind": { "const": "url_fetch" }, "url": { "type": "string" } } },
        { "type": "object", "required": ["kind", "agent_name", "prompt"], "additionalProperties": false,
          "properties": { "kind": { "const": "agent_call" }, "agent_name": { "type": "string" },
            "prompt": { "type": "string" }, "background": { "type": "boolean" } } },
        { "type": "object", "required": ["kind", "skill_name"], "additionalProperties": false,
          "properties": { "kind": { "const": "skill_call" }, "skill_name": { "type": "string" },
            "args": { "type": "string" } } },
        { "type": "object", "required": ["kind"], "additionalProperties": false,
          "properties": { "kind": { "const": "todo_list" } } },
        { "type": "object", "required": ["kind"], "additionalProperties": false,
          "properties": { "kind": { "const": "background_task" } } },
        { "type": "object", "required": ["kind", "task_id"], "additionalProperties": false,
          "properties": { "kind": { "const": "task_stop" }, "task_id": { "type": "string" },
            "task_description": { "type": "string" } } },
        { "type": "object", "required": ["kind", "plan"], "additionalProperties": false,
          "properties": { "kind": { "const": "plan_review" }, "plan": { "type": "string" },
            "path": { "type": "string" },
            "options": { "type": "array", "items": { "type": "object", "required": ["label"],
              "additionalProperties": false, "properties": { "label": { "type": "string" },
                "description": { "type": "string" } } } } } },
        { "type": "object", "required": ["kind", "objective"], "additionalProperties": false,
          "properties": { "kind": { "const": "goal_start" }, "objective": { "type": "string" },
            "completion_criterion": { "type": "string" } } },
        { "type": "object", "required": ["kind", "summary"], "additionalProperties": false,
          "properties": { "kind": { "const": "generic" }, "summary": { "type": "string" } } }
      ],
      "description": "审批载荷的结构化预览提示。语义蓝本为 agent-core-v2 的 ToolInputDisplay（approval-architecture.md 13 形态）。可选字段缺省时整个键省略（非 null）。"
    }
  }
}
```

统一约定：

- **参数**：所有方法参数为对象；可空参数用 `?` 标注并允许缺省；多余字段一律拒绝（`additionalProperties: false`）。
- **命名**：wire 字段名全用 camelCase（**参数与结果字段均如此**，与现有 Rust wire 视图及 TS server 一致）；方法名全小写点分。
- **错误**：每个方法给出**本方法可产生的应用错误码**；协议错误（`-32700/-32600/-32601/-32602/-32603`）对所有方法通用，不再逐个列出。错误码全集与语义见 §10。
- **结果**：无特别说明时成功结果即所列 schema；`{ok: true}` 用于纯确认操作。

**连接级控制方法**（不属于任何领域，先列出）：

| 方法 | 方向 | 说明 |
|------|------|------|
| `ping` / `pong` | 通知 | 应用层心跳（§5.5） |
| `session.subscribe` | 请求 | `{sessionId, cursor?}` → `{ok}`；订阅后该会话 durable 事件按序推送；幂等（重复订阅返回 `{ok}` 并重放自当前 cursor 起的事件）。可选 `cursor: {seq, epoch}`：服务端**原子地**以该游标为订阅起点（自 `cursor.seq + 1` 开始重放），消除「快照 + 订阅」竞态（§9.4） |
| `session.unsubscribe` | 请求 | `{sessionId}` → `{ok}`；幂等 |
| `session.snapshot` | 请求 | 见 §6.1（全量状态重建，§9.4） |

```json
{
  "session.subscribe": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "cursor": {
          "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Cursor",
          "description": "订阅起点游标（可选）：服务端在同一锁/事务内原子地以 cursor.seq + 1 为起点重放 durable 事件；缺省从当前日志尾部开始。用于「快照 → 订阅」的原子衔接（§9.4）"
        }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

### 6.1 Session 域

#### `session.create`

创建新会话。返回会话摘要。

```json
{
  "session.create": {
    "params": {
      "type": "object",
      "properties": {
        "title": { "type": "string", "maxLength": 200, "default": "Untitled Session" },
        "tags": { "type": "array", "items": { "type": "string", "maxLength": 50 } },
        "workspaceId": { "type": "string", "description": "绑定工作区；缺省绑定服务端默认工作区" },
        "agentTemplate": { "type": "string", "description": "初始 agent 模板 id（缺省用默认模板）" }
      }
    },
    "result": { "$ref": "#/$defs/SessionView" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/session-view",
  "title": "SessionView",
  "type": "object",
  "required": ["id", "title", "createdAt", "updatedAt", "agentCount", "tags"],
  "properties": {
    "id": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
    "title": { "type": "string" },
    "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" },
    "updatedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" },
    "agentCount": { "type": "integer", "minimum": 0 },
    "tags": { "type": "array", "items": { "type": "string" } },
    "workspaceId": { "type": "string" },
    "status": { "type": "string", "enum": ["live", "closed", "archived"] },
    "busy": { "type": "boolean", "description": "聚合忙碌：任一成员回合在途或后台工作存活（会话管理域 SDR-013）" },
    "mainTurnActive": { "type": "boolean", "description": "主对话成员的回合未收尾——列表'运行中'标记的判定输入" },
    "pendingInteraction": { "type": "string", "enum": ["none", "approval", "question"] },
    "lastTurnReason": { "type": "string", "enum": ["completed", "cancelled", "failed"] }
  }
}
```

错误：`-32602` 参数非法 · `-32003` 工作区无权限 · `-32004` 限流。

#### `session.list`

```json
{
  "session.list": {
    "params": {
      "type": "object",
      "properties": {
        "workspaceId": { "type": "string" },
        "status": { "type": "string", "enum": ["live", "closed", "archived"] },
        "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 50 },
        "cursor": { "type": "string" }
      }
    },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/session-view" } },
        "nextCursor": { "type": "string" }
      }
    }
  }
}
```

错误：`-32602` 参数非法。

#### `session.get`

```json
{
  "session.get": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } }
    },
    "result": {
      "type": "object",
      "allOf": [{ "$ref": "https://mirri.dev/protocol/v1/session-view" }],
      "properties": {
        "agents": {
          "type": "array",
          "items": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" }
        },
        "cursors": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Cursor" }
      }
    }
  }
}
```

错误：`-32000` 会话不存在。

#### `session.close`

```json
{
  "session.close": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "reason": { "type": "string", "enum": ["user_initiated", "idle_timeout", "shutdown"] }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在。成功后必发 `session.deleted` 事件。

#### `session.resume`

从持久化存储恢复会话（重放 wire log）。

```json
{
  "session.resume": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 会话损坏/恢复失败（`data.details` 带原因）。成功后必发 `session.updated` 事件。

#### `session.fork`

复制某会话的一个 agent 上下文到新会话。沿用现有 `session.fork` 语义（`sessionId` + `agentId`）。

```json
{
  "session.fork": {
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" },
        "title": { "type": "string", "maxLength": 200 },
        "checkpointId": { "type": "string", "description": "从指定检查点 fork；缺省 fork 最新状态" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` agent 不存在 · `-32024` 检查点不存在。

#### `session.export`

导出会话为 zip 归档。沿用现有 `session.export` 语义与路径安全约束（输出路径必须落在导出根目录内，§13.5）。

```json
{
  "session.export": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "outputPath": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath", "description": "缺省 <home>/.mirri-code/exports/<sessionId>.zip" },
        "includeGlobalLog": { "type": "boolean", "default": false }
      }
    },
    "result": {
      "type": "object",
      "required": ["zipPath", "entryCount", "sessionDir"],
      "properties": {
        "zipPath": { "type": "string" },
        "entryCount": { "type": "integer" },
        "sessionDir": { "type": "string" }
      }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32602` outputPath 越界/非法 · `-32024` 导出失败。

#### `session.archive`

关闭会话并归档持久化状态（区别于 `session.close` 的保留可恢复语义：archive 后从 `session.list` 的 live 视图移除，只能 `session.restore`）。

```json
{
  "session.archive": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "title": { "type": "string", "description": "归档显示名" }
      }
    },
    "result": {
      "type": "object",
      "required": ["ok", "archiveId"],
      "properties": { "ok": { "const": true }, "archiveId": { "type": "string" } }
    }
  }
}
```

错误：`-32000` 会话不存在。成功后发 `session.deleted`，随后 `session.updated`（archiveId 关联）事件。

#### `session.restore`

```json
{
  "session.restore": {
    "params": {
      "type": "object",
      "required": ["archiveId"],
      "properties": {
        "archiveId": { "type": "string" },
        "title": { "type": "string" },
        "workspaceId": { "type": "string" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32024` 归档不存在/损坏。成功后发 `session.created`。**注意**：恢复后的会话获得**新的 `sessionId`**；原 `archiveId` 记录在新会话元数据中（`metadata.archiveId`，可经 `session.updateMeta` 查询/修改），据此追溯来源。

#### `session.snapshot`

全量状态重建（resync 的恢复手段，§9.4）。

```json
{
  "session.snapshot": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "includeMessages": { "type": "boolean", "default": true },
        "includeTurns": { "type": "boolean", "default": true },
        "maxMessages": { "type": "integer", "minimum": 1, "maximum": 10000, "default": 2000 }
      }
    },
    "result": {
      "type": "object",
      "required": ["session", "cursor"],
      "properties": {
        "session": { "$ref": "https://mirri.dev/protocol/v1/session-view" },
        "cursor": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Cursor" },
        "messages": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/message" } },
        "turns": { "type": "array", "description": "最近 turn 摘要（含工具调用与 step 列表）" },
        "agents": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/agent-summary" } },
        "pendingApprovals": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/approval-ask-params" } },
        "pendingQuestions": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/question-ask-params" } },
        "goals": { "type": "array" },
        "plan": { "type": ["object", "null"] },
        "tasks": { "type": "array" },
        "backgroundTasks": { "type": "array" }
      }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32023` 快照过大（按 `data.maxMessages` 提示客户端缩小范围）。**snapshot 的 `cursor` 是客户端后续事件追平的基准**。

#### `session.reload`

从磁盘重新加载会话状态（丢弃内存中未持久化的变更）。

```json
{
  "session.reload": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 会话损坏/磁盘不可读（`data.details` 带原因）。成功后发 `session.updated`。

#### `session.rename`

```json
{
  "session.rename": {
    "params": {
      "type": "object",
      "required": ["sessionId", "title"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "title": { "type": "string", "minLength": 1, "maxLength": 200 }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32000` 会话不存在 · `-32602` 标题非法。成功后发 `session.meta.updated`。

#### `session.updateMeta`

```json
{
  "session.updateMeta": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "tags": { "type": "array", "items": { "type": "string", "maxLength": 50 } },
        "metadata": { "type": "object", "description": "任意元数据（浅合并；含 archiveId 追溯等）" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/session-view" }
  }
}
```

错误：`-32000` 会话不存在 · `-32602` 参数非法。成功后发 `session.meta.updated`（`changedKeys` 列出变更字段）。

#### `session.clearContext`

清空会话上下文（保留会话与历史日志；后续 turn 从空上下文开始）。

```json
{
  "session.clearContext": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省清空会话内全部 agent" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` agent 不存在 · `-32024` turn 进行中（先 `message.abort`）。成功后发 `session.context_cleared`。

#### `session.compact`

```json
{
  "session.compact": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "instruction": { "type": "string", "maxLength": 2000, "description": "压缩指令（如保留重点、摘要风格）" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["ok", "compactedTokens"],
      "properties": {
        "ok": { "const": true },
        "compactedTokens": { "type": "integer", "description": "本次压缩节省的 token 数；异步压缩进行中时为 0" }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 已有压缩进行中/上下文过小无需压缩。成功后发 `compaction.started`，完成发 `compaction.completed`。

#### `session.cancelCompaction`

```json
{
  "session.cancelCompaction": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 无进行中的压缩。成功后发 `compaction.cancelled`。

#### `session.undo`

回退历史步骤（撤销 agent 已执行的 turn/工具调用）。

```json
{
  "session.undo": {
    "params": {
      "type": "object",
      "required": ["sessionId", "count"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "count": { "type": "integer", "minimum": 1, "maximum": 100, "default": 1 }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["ok", "removedSteps"],
      "properties": {
        "ok": { "const": true },
        "removedSteps": { "type": "integer", "description": "实际回退的步骤数" }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 无可撤销步骤/turn 进行中。成功后发 `session.undone`。

#### `session.status`

运行状态快照（running 状态、当前 turn、活跃 agents）。

```json
{
  "session.status": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["state"],
      "properties": {
        "state": { "type": "string", "enum": ["idle", "running", "paused", "compacting", "closed", "archived"] },
        "currentTurnId": { "type": ["string", "null"] },
        "activeAgents": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" } },
        "queuedTurns": { "type": "integer", "default": 0 }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在。

#### `session.warnings`

会话级警告列表（对应 §7.12 `warning` 事件的持久化视图）。

```json
{
  "session.warnings": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["warnings"],
      "properties": {
        "warnings": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["code", "message"],
            "properties": {
              "code": { "type": "string", "description": "警告类别码（同 §7.12 warning 事件）" },
              "message": { "type": "string" },
              "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
            }
          }
        }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在。

#### `session.startBtw`

启动 break-the-wall 子 agent（隔离会话内上下文、独立探索的并行工作流）。

```json
{
  "session.startBtw": {
    "params": {
      "type": "object",
      "required": ["sessionId", "input"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "input": { "type": "string", "minLength": 1, "maxLength": 20000 }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["agentId"],
      "properties": { "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" } },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 超出子 agent 并发上限。成功后发 `subagent.spawned`。

#### `session.children`

```json
{
  "session.children": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } },
      "additionalProperties": false
    },
    "result": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/session-view" } }
  }
}
```

错误：`-32000` 会话不存在。

### 6.2 Message 域

#### `message.send`

**核心异步语义**：立即返回 `turnId`，turn 进度全部走事件流（`turn.started` → `turn.step.*` → `assistant.delta` → `turn.ended`）。`mode: "wait"` 仅为简单客户端（CLI 脚本）保留：阻塞至 turn 结束，返回完整 `TurnEnd`。**wait 模式的事件保证**：volatile 事件（`assistant.delta`、`thinking.delta`、`tool.call.delta`、`tool.progress`）服务端**可以跳过**（省带宽）；durable 事件（`turn.started`、`turn.step.*`、`tool.result`、`turn.ended`）**始终发送**——其中 `turn.ended` 在 wait 模式下同样发送，保证多客户端（如同时挂着的 UI 端）观察一致。

```json
{
  "message.send": {
    "params": {
      "type": "object",
      "required": ["sessionId", "prompt"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" },
        "prompt": {
          "description": "用户输入。字符串或结构化块（多模态：text / image / file）",
          "anyOf": [
            { "type": "string" },
            {
              "type": "array",
              "items": {
                "type": "object",
                "required": ["type"],
                "properties": {
                  "type": { "type": "string", "enum": ["text", "image", "file"] },
                  "text": { "type": "string" },
                  "dataBase64": { "type": "string" },
                  "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
                  "mimeType": { "type": "string" }
                }
              }
            }
          ]
        },
        "mode": { "type": "string", "enum": ["fire", "wait"], "default": "fire" },
        "attachments": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["name"],
            "properties": {
              "name": { "type": "string" },
              "contentType": { "type": "string" },
              "dataBase64": { "type": "string" }
            }
          }
        },
        "model": { "type": "string", "description": "本条消息临时覆盖模型；结束后还原" },
        "overrideConfig": { "type": "object", "description": "本条消息临时覆盖 agent 配置（如 temperature），浅合并" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["ok", "turnId"],
      "properties": {
        "ok": { "const": true },
        "turnId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TurnId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" },
        "mode": { "type": "string", "enum": ["fire", "wait"] },
        "turn": {
          "description": "仅 mode=wait 时返回",
          "$ref": "https://mirri.dev/protocol/v1/turn-end"
        }
      },
      "additionalProperties": false
    }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/turn-end",
  "title": "TurnEnd result",
  "type": "object",
  "required": ["turnId", "stopReason", "steps", "usage", "durationMs"],
  "properties": {
    "turnId": { "type": "string" },
    "stopReason": {
      "type": "string",
      "enum": ["end_turn", "max_tokens", "filtered", "paused", "unknown", "aborted", "blocked_by_hook"]
    },
    "steps": { "type": "integer" },
    "usage": {
      "type": "object",
      "required": ["inputTokens", "outputTokens"],
      "properties": {
        "inputTokens": { "type": "integer" },
        "outputTokens": { "type": "integer" }
      }
    },
    "durationMs": { "type": "integer" },
    "blockedByPromptHook": { "type": "boolean", "default": false }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` agent 不存在 · `-32024` agent 无 LLM provider（消息仍记录） · `-32005` turn 超时（仅 `wait` 模式） · `-32003` 输入被 hook/策略拒绝。

#### `message.abort`

中断进行中的 turn（对应现有 WS `interrupt` 控制帧，统一为 RPC）。

```json
{
  "message.abort": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" },
        "turnId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TurnId", "description": "缺省中断该 agent 当前 turn" }
      }
    },
    "result": {
      "type": "object",
      "required": ["ok", "interrupted"],
      "properties": {
        "ok": { "const": true },
        "interrupted": { "type": "boolean", "description": "是否确有 turn 被中断" }
      }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` 指定 turnId 不存在。中断成功必发 `turn.step.interrupted` + `turn.ended(stopReason=aborted)`。

#### `message.steer`

turn 进行中注入额外指令（多模态中断式引导）。

```json
{
  "message.steer": {
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "text"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "turnId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TurnId" },
        "text": { "type": "string", "maxLength": 20000 },
        "mode": { "type": "string", "enum": ["interrupt", "queue"], "default": "interrupt", "description": "interrupt = 立即打断当前 step 处理新指令；queue = 加入待处理队列" }
      }
    },
    "result": {
      "type": "object",
      "required": ["ok", "received"],
      "properties": { "ok": { "const": true }, "received": { "type": "boolean" } }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32024` turn 不存在/已结束。

#### `message.list`

分页获取消息历史（与 `session.snapshot` 互补：面向长历史增量加载）。

```json
{
  "message.list": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "beforeId": { "type": "string", "description": "取该消息之前（更早）的消息；与 afterId 二选一" },
        "afterId": { "type": "string", "description": "取该消息之后（更新）的消息；与 beforeId 二选一" },
        "pageSize": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 },
        "role": { "type": "string", "enum": ["user", "assistant", "system", "tool"], "description": "按角色过滤（可选）" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["messages", "hasMore"],
      "properties": {
        "messages": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["id", "role", "createdAt"],
            "properties": {
              "id": { "type": "string" },
              "role": { "type": "string", "enum": ["user", "assistant", "system", "tool"] },
              "content": { "anyOf": [ { "type": "string" }, { "type": "array", "description": "结构化 parts（多模态）" } ] },
              "agentId": { "type": ["string", "null"] },
              "turnId": { "type": ["string", "null"] },
              "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
            }
          }
        },
        "hasMore": { "type": "boolean" }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32602` 参数非法（beforeId/afterId 同传）。

### 6.3 Tool 域

#### `tool.list`

列出会话内所有 agent 的可用工具（含 MCP/插件注入的工具）。沿用现有语义。

```json
{
  "tool.list": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" }
      }
    },
    "result": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["name", "description"],
        "properties": {
          "name": { "type": "string" },
          "description": { "type": "string" },
          "parameters": { "type": "object", "description": "JSON Schema（可空）" },
          "deferred": { "type": "boolean", "default": false },
          "source": { "type": "string", "enum": ["builtin", "mcp", "plugin"] },
          "mcpServerId": { "type": "string" },
          "enabled": { "type": "boolean", "default": true }
        }
      }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` agent 不存在。

#### `tool.call`

直接调用工具（不经 LLM）。沿用现有语义：策略链要求 `Ask` 的调用在无审批面时**直接拒绝**（返回 `isError: true` 的 ToolResult，绝不绕过权限）。

```json
{
  "tool.call": {
    "params": {
      "type": "object",
      "required": ["sessionId", "name"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" },
        "name": { "type": "string" },
        "arguments": { "type": "object", "default": {} },
        "turnId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TurnId", "description": "关联到某 turn（审计用）；缺省 direct-call" }
      }
    },
    "result": {
      "type": "object",
      "required": ["output", "isError"],
      "properties": {
        "output": { "anyOf": [ { "type": "string" }, { "type": "array", "description": "结构化 parts" } ] },
        "isError": { "type": "boolean" },
        "note": { "type": ["string", "null"] },
        "stopTurn": { "type": "boolean" },
        "truncated": { "type": "boolean" }
      }
    }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` agent 不存在 · `-32006` 工具不存在。策略拒绝**不是**错误响应，而是 `result.isError: true`（与引擎内一致）。

#### `tool.catalog`

工作区级工具目录（不依赖 session；含 MCP/插件注入的工具）。与 `tool.list` 的区别：按工作区聚合而非按 agent 聚合。

```json
{
  "tool.catalog": {
    "params": {
      "type": "object",
      "properties": {
        "workspaceId": { "type": "string", "description": "缺省取服务端默认工作区" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["tools"],
      "properties": {
        "tools": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["name", "description"],
            "properties": {
              "name": { "type": "string" },
              "description": { "type": "string" },
              "parameters": { "type": "object", "description": "JSON Schema（可空）" },
              "source": { "type": "string", "enum": ["builtin", "mcp", "plugin"] },
              "mcpServerId": { "type": "string" },
              "enabled": { "type": "boolean", "default": true }
            }
          }
        }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`-32009` 工作区不存在。

### 6.4 Agent 域

#### `agent.list` / `agent.get`

沿用现有语义（`agent.list` 返回会话内全部 agent 摘要；`agent.get` 返回单 agent 详情，错误 `-32001`）。

```json
{
  "agent.list": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/agent-summary" } }
  },
  "agent.get": {
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/agent-detail" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/agent-summary",
  "type": "object",
  "required": ["id", "name", "status"],
  "properties": {
    "id": { "type": "string" },
    "name": { "type": "string" },
    "status": { "$ref": "https://mirri.dev/protocol/v1/agent-status" },
    "model": { "type": "string" },
    "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/agent-status",
  "type": "string",
  "enum": ["idle", "running", "paused", "suspended", "error", "stopped"]
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/agent-detail",
  "type": "object",
  "allOf": [{ "$ref": "https://mirri.dev/protocol/v1/agent-summary" }],
  "properties": {
    "systemPrompt": { "type": ["string", "null"] },
    "config": { "type": "object", "description": "模型/温度/上下文参数等" },
    "tools": { "type": "array", "items": { "type": "string" } }
  }
}
```

#### `agent.create`

会话内创建新 agent（多 agent 场景）。

```json
{
  "agent.create": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "name": { "type": "string", "maxLength": 100 },
        "model": { "type": "string" },
        "systemPrompt": { "type": "string" },
        "tools": { "type": "array", "items": { "type": "string" }, "description": "允许的工具白名单；缺省继承会话默认集" },
        "config": { "type": "object" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/agent-detail" }
  }
}
```

错误：`-32000` 会话不存在 · `-32001` 模版/模型不可用。成功后发 `agent.status.updated` 事件。

#### `agent.setModel` / `agent.setThinking` / `agent.setPermission` / `agent.setSwarmMode` / `agent.swarm`

会话级 agent 配置覆盖与多 agent 分发。

```json
{
  "agent.setModel": {
    "params": {
      "type": "object",
      "required": ["sessionId", "model"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" },
        "model": { "type": "string", "description": "模型 id（会话级持久覆盖；区别于 message.send 的一次性覆盖）" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "agent.setThinking": {
    "params": {
      "type": "object",
      "required": ["sessionId", "thinking"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" },
        "thinking": { "type": "boolean", "description": "思考模式开关" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "agent.setPermission": {
    "params": {
      "type": "object",
      "required": ["sessionId", "mode"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" },
        "mode": { "type": "string", "enum": ["auto", "ask", "deny"], "description": "权限模式覆盖（auto = 自动放行、ask = 需审批、deny = 拒绝）" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "agent.setSwarmMode": {
    "params": {
      "type": "object",
      "required": ["sessionId", "enabled"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "enabled": { "type": "boolean", "description": "会话级 swarm 模式开关（启用后输入默认多 agent 分发）" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "agent.swarm": {
    "params": {
      "type": "object",
      "required": ["sessionId", "input"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "input": { "type": "string", "minLength": 1 },
        "agents": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" }, "description": "参与分发的 agent 子集；缺省全部" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["dispatched"],
      "properties": { "dispatched": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" } } },
      "additionalProperties": false
    }
  }
}
```

错误：`agent.setModel` `-32011` 模型不可用；`agent.setPermission` `-32602` mode 非法；`agent.swarm` `-32001` 指定 agent 不存在。`agent.setModel` 成功后发 `session.model_changed`；`agent.swarm` 成功后发 `agent.swarm_dispatched`。

#### `agent.getContext` / `agent.getUsage` / `agent.generateAgentsMd` / `agent.addAdditionalDir`

```json
{
  "agent.getContext": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["context"],
      "properties": {
        "context": {
          "type": "object",
          "required": ["contextWindow", "usedTokens", "messageCount"],
          "properties": {
            "contextWindow": { "type": "integer" },
            "usedTokens": { "type": "integer" },
            "messageCount": { "type": "integer" },
            "systemPromptTokens": { "type": "integer" }
          }
        }
      },
      "additionalProperties": false
    }
  },
  "agent.getUsage": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId", "description": "缺省取会话第一个 agent" }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["usage"],
      "properties": {
        "usage": {
          "type": "object",
          "required": ["inputTokens", "outputTokens", "totalTokens"],
          "properties": {
            "inputTokens": { "type": "integer" },
            "outputTokens": { "type": "integer" },
            "totalTokens": { "type": "integer" },
            "turnsCount": { "type": "integer" }
          }
        }
      },
      "additionalProperties": false
    }
  },
  "agent.generateAgentsMd": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["content"],
      "properties": { "content": { "type": "string", "description": "生成的 AGENTS.md 文本" } },
      "additionalProperties": false
    }
  },
  "agent.addAdditionalDir": {
    "params": {
      "type": "object",
      "required": ["sessionId", "path"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath", "description": "加入 agent 工作区扫描范围的附加目录（须过路径校验，§13.5）" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`agent.getContext` / `agent.getUsage` `-32001` agent 不存在；`agent.addAdditionalDir` `-32022` 路径越界/不存在。

### 6.5 Workspace 域

```json
{
  "workspace.list": {
    "params": { "type": "object" },
    "result": { "type": "array", "items": { "$ref": "https://mirri.dev/protocol/v1/workspace-view" } }
  },
  "workspace.create": {
    "params": {
      "type": "object",
      "required": ["path"],
      "properties": {
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "name": { "type": "string", "maxLength": 100 }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/workspace-view" }
  },
  "workspace.delete": {
    "params": { "type": "object", "required": ["workspaceId"], "properties": { "workspaceId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "workspace.refresh": {
    "params": { "type": "object", "properties": { "workspaceId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/workspace-view" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/workspace-view",
  "type": "object",
  "required": ["id", "path", "name"],
  "properties": {
    "id": { "type": "string" },
    "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath", "description": "POSIX 风格绝对路径" },
    "name": { "type": "string" },
    "status": { "type": "string", "enum": ["ready", "scanning", "error"] },
    "scanStats": { "type": "object", "description": "refresh 后的索引统计（可选）" }
  }
}
```

错误：`workspace.create` `-32602` 路径非法/不存在 · `-32003` 路径越界;`workspace.delete/refresh` `-32009` 工作区不存在。

### 6.6 Config 域

```json
{
  "config.get": {
    "params": { "type": "object", "properties": { "keys": { "type": "array", "items": { "type": "string" } } } },
    "result": { "type": "object", "required": ["values"], "properties": { "values": { "type": "object", "description": "key → value 映射；未指定的 key 返回全部" } } }
  },
  "config.set": {
    "params": {
      "type": "object",
      "required": ["values"],
      "properties": {
        "values": { "type": "object", "description": "key → value，浅合并" },
        "persist": { "type": "boolean", "default": true, "description": "false 仅本次进程生效" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "config.watch": {
    "params": { "type": "object", "properties": { "keys": { "type": "array", "items": { "type": "string" } } } },
    "result": { "type": "object", "required": ["ok", "watching"], "properties": { "ok": { "const": true }, "watching": { "type": "array", "items": { "type": "string" } } } }
  },
  "config.getDiagnostics": {
    "params": { "type": "object" },
    "result": {
      "type": "object",
      "required": ["diagnostics"],
      "properties": {
        "diagnostics": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["severity", "code", "message"],
            "properties": {
              "severity": { "type": "string", "enum": ["error", "warning", "info"] },
              "code": { "type": "string" },
              "message": { "type": "string" },
              "path": { "type": "string", "description": "config 中的 key 路径（可选）" }
            }
          }
        }
      }
    }
  },
  "config.getExperimentalFeatures": {
    "params": { "type": "object" },
    "result": {
      "type": "object",
      "required": ["features"],
      "properties": {
        "features": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["id", "enabled"],
            "properties": {
              "id": { "type": "string" },
              "enabled": { "type": "boolean" },
              "description": { "type": "string" }
            }
          }
        }
      }
    }
  },
  "config.replaceSections": {
    "params": {
      "type": "object",
      "required": ["sections"],
      "properties": {
        "sections": { "type": "object", "description": "section 名 → 完整配置值；**原子替换**整个 section，未列出的 section 保持不变" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "config.removeProvider": {
    "params": { "type": "object", "required": ["providerId"], "properties": { "providerId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

- `config.set` / `config.replaceSections` / `config.removeProvider` 成功后服务端对所有已 `config.watch` 的客户端发 `config.changed` 事件；文件监听（外部编辑）触发的变更同样发 `config.changed`——**统一覆盖 doc 06 的 `config.reloaded` 语义，不单独定义 `config.reloaded` 事件**。
- `config.watch` 是连接级注册（重复调用合并 key 集合）；断开即失效，无需显式取消。
- `config.replaceSections` 校验失败时整体回滚（原子性）；`data.details` 列出问题项。
- 错误：`-32602` 未知 key/类型非法 · `-32012` 配置校验失败（`data.details` 列出问题项）· `config.removeProvider` `-32010` provider 不存在。

### 6.7 Skill 域

```json
{
  "skill.list": {
    "params": { "type": "object", "properties": { "workspaceId": { "type": "string" } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["id", "name", "version", "enabled"],
            "properties": {
              "id": { "type": "string" },
              "name": { "type": "string" },
              "description": { "type": "string" },
              "version": { "type": "string" },
              "enabled": { "type": "boolean" }
            }
          }
        }
      }
    }
  },
  "skill.activate": {
    "params": {
      "type": "object",
      "required": ["skillId"],
      "properties": {
        "skillId": { "type": "string" },
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "agentId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/AgentId" }
      }
    },
    "result": { "type": "object", "required": ["ok", "skillId"], "properties": { "ok": { "const": true }, "skillId": { "type": "string" } } }
  },
  "skill.listForWorkspace": {
    "params": { "type": "object", "required": ["workspaceId"], "properties": { "workspaceId": { "type": "string" } } },
    "result": {
      "type": "object",
      "required": ["skills"],
      "properties": {
        "skills": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["id", "name", "version", "enabled"],
            "properties": {
              "id": { "type": "string" },
              "name": { "type": "string" },
              "description": { "type": "string" },
              "version": { "type": "string" },
              "enabled": { "type": "boolean" },
              "workspaceId": { "type": "string" }
            }
          }
        }
      }
    }
  }
}
```

错误：`skill.activate` `-32024` skill 不存在 · `-32003` 会话不匹配；`skill.listForWorkspace` `-32009` 工作区不存在。成功后发 `skill.activated` 事件。

### 6.8 MCP 域

> 本节原为提案期方法目录（`mcp.listServers`/`listTools`/`toggleTool`/`getStartupMetrics`），已被替换为"契约面（既有）+ 规划面"。**MCP 域的事实源是 [docs/specs/mcp-architecture.md](../mcp-architecture.md)**（三池连接模型、配置分层读写、A1-A5 裁决、Rollout P1-P4）；本节只做协议视角的摘要。

**契约面（既有，registry 事实行，both 面）**：

```json
{
  "mcp.list": {
    "params": { "type": "object" },
    "result": { "items": [{ "name": "string", "transport": "stdio|sse|http", "status": "string", "toolCount": "integer", "error": ["string", "null"] }] }
  },
  "mcp.startupMetrics": { "params": { "type": "object" }, "result": { "metrics": "array" } },
  "mcp.reconnect": { "params": { "name": "string" }, "result": { "ok": "true" } }
}
```

`mcp.reconnect` 内部走 `reconnectAndJoin`（并发触发只执行一次）。插件域另有 `plugin.setPluginMcpServerEnabled`（插件 server 的启用总开关，both 面）。

**规划（以 MCP spec Rollout P1-P4 为准，wire 新增 9 + 扩展 1）**：

```
mcp.config.list(scope?: 'user'|'workspace') → 层视图（含 source: user|projectRoot|project|plugin 与 effective 过滤/超时）
mcp.config.set(scope, name, config)          → noResult（全量替换；workspace scope 仅写 project 层）
mcp.config.remove(scope, name)               → noResult
mcp.config.test(scope, name, config?)        → 连接测试（按展开后环境变量实测；未落盘直测或已落盘实测）
mcp.config.setToolFilter(name, filter)       → noResult（工具级开关统一写入口；插件 server 写 capability state）
mcp.status()                                 → 无 workspace 上下文的全局状态视图（UserPool 视角）
mcp.auth.start(serverName)                   → { authUrl, state }（needs-auth 服务器发起 OAuth）
mcp.auth.complete(serverName, {code|error})  → noResult（授权回调入 IMcpOAuthStore 后自动重连）
event.mcp.server.status                      → { name, pool: 'user'|'workspace'|'plugin', status }（状态事件，见 §7.10；命名经 MCP 域 ADR-010 统一为既有事件名）
mcp.list                                     → 扩展字段 pool/source（向后兼容新增）
```

错误：`mcp.reconnect` `-32024` server 不存在。配置读写遵循 MCP spec 裁决 A1/A2/A5（插件连接定义只读、project-root 层只读、配置值原文呈现——解析值不经 wire）。状态变化发 `event.mcp.server.status`（§7.10），工具集合变化发 `tool.list.updated`。

### 6.9 Model 域

```json
{
  "model.list": {
    "params": { "type": "object", "properties": { "providerId": { "type": "string" }, "refresh": { "type": "boolean", "default": false } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["id", "providerId", "name"],
            "properties": {
              "id": { "type": "string" },
              "providerId": { "type": "string" },
              "name": { "type": "string" },
              "contextWindow": { "type": "integer" },
              "maxOutputTokens": { "type": "integer" },
              "pricing": { "type": "object", "description": "价格信息，可选" }
            }
          }
        }
      }
    }
  },
  "model.listProviders": {
    "params": { "type": "object" },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["providerId", "providerType", "configured"],
            "properties": {
              "providerId": { "type": "string" },
              "providerType": { "type": "string" },
              "configured": { "type": "boolean" },
              "healthy": { "type": "boolean" },
              "modelsCount": { "type": "integer" },
              "error": { "type": ["string", "null"] }
            }
          }
        }
      }
    }
  },
  "model.addProvider": {
    "params": {
      "type": "object",
      "required": ["providerType"],
      "properties": {
        "providerType": { "type": "string" },
        "apiKey": { "type": "string", "description": "敏感字段，仅入参，永不回传" },
        "baseUrl": { "type": "string", "format": "uri" },
        "config": { "type": "object" }
      }
    },
    "result": { "type": "object", "required": ["ok", "providerId"], "properties": { "ok": { "const": true }, "providerId": { "type": "string" } } }
  },
  "model.deleteProvider": {
    "params": { "type": "object", "required": ["providerId"], "properties": { "providerId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "model.refreshProvider": {
    "params": { "type": "object", "properties": { "providerId": { "type": "string" } } },
    "result": { "type": "object", "required": ["ok", "modelsFound"], "properties": { "ok": { "const": true }, "modelsFound": { "type": "integer" } } }
  }
}
```

错误：`model.list` `-32010` provider 不存在 · `model.addProvider` `-32602` 配置非法 · `model.deleteProvider` `-32010` provider 不存在。

### 6.10 Auth 域

```json
{
  "auth.status": {
    "params": { "type": "object" },
    "result": {
      "type": "object",
      "required": ["authenticated"],
      "properties": {
        "authenticated": { "type": "boolean" },
        "user": { "type": ["object", "null"], "description": "用户信息（登录态时）" },
        "loginMethod": { "type": ["string", "null"], "enum": [null, "oauth", "token", "device"] }
      }
    }
  },
  "auth.login": {
    "params": {
      "type": "object",
      "required": ["method"],
      "properties": {
        "method": { "type": "string", "enum": ["oauth", "token", "device"] },
        "token": { "type": "string", "description": "method=token 时提供" },
        "code": { "type": "string", "description": "method=oauth/device 的授权码回调" },
        "redirectUrl": { "type": "string", "format": "uri" }
      }
    },
    "result": {
      "type": "object",
      "required": ["ok", "authenticated"],
      "properties": {
        "ok": { "const": true },
        "authenticated": { "type": "boolean" },
        "user": { "type": ["object", "null"] },
        "authUrl": { "type": ["string", "null"], "description": "oauth/device 需要用户打开的外部授权地址" }
      }
    }
  },
  "auth.logout": {
    "params": { "type": "object" },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`auth.login` `-32002` 凭据无效 · `-32004` 登录频控。登录/登出成功后发 `auth.status.changed`（§7.14 补充事件）。

### 6.11 File 域

文件域所有 `path` 参数必须先经 `validate_workspace_path` 类校验（§13.5），越界一律 `-32022`。

```json
{
  "file.list": {
    "params": {
      "type": "object",
      "required": ["path"],
      "properties": {
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "workspaceId": { "type": "string" },
        "maxDepth": { "type": "integer", "minimum": 0, "maximum": 64, "default": 1 },
        "includeHidden": { "type": "boolean", "default": false }
      }
    },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["name", "path", "type"],
            "properties": {
              "name": { "type": "string" },
              "path": { "type": "string" },
              "type": { "type": "string", "enum": ["file", "dir", "symlink"] },
              "size": { "type": "integer" },
              "modifiedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
            }
          }
        }
      }
    }
  },
  "file.read": {
    "params": {
      "type": "object",
      "required": ["path"],
      "properties": {
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "workspaceId": { "type": "string" },
        "encoding": { "type": "string", "enum": ["utf-8", "base64"], "default": "utf-8" },
        "offset": { "type": "integer", "minimum": 0, "description": "字节偏移（分段读取）" },
        "length": { "type": "integer", "minimum": 1, "maximum": 10485760, "description": "读取字节上限" }
      }
    },
    "result": {
      "type": "object",
      "required": ["content", "encoding"],
      "properties": {
        "content": { "type": "string", "description": "utf-8 文本或 base64" },
        "encoding": { "type": "string", "enum": ["utf-8", "base64"] },
        "truncated": { "type": "boolean", "description": "超出 length/size 上限被截断" },
        "size": { "type": "integer", "description": "文件总字节数" }
      }
    }
  },
  "file.search": {
    "params": {
      "type": "object",
      "required": ["query"],
      "properties": {
        "query": { "type": "string", "minLength": 1, "maxLength": 200 },
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "maxResults": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 },
        "includeHidden": { "type": "boolean", "default": false }
      }
    },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["path"],
            "properties": {
              "path": { "type": "string" },
              "name": { "type": "string" },
              "matchedBy": { "type": "string", "enum": ["filename", "content"], "description": "命中方式；content 命中时含行号字段" },
              "lineNumber": { "type": "integer" },
              "snippet": { "type": "string" }
            }
          }
        }
      }
    }
  },
  "file.grep": {
    "params": {
      "type": "object",
      "required": ["pattern"],
      "properties": {
        "pattern": { "type": "string", "minLength": 1, "maxLength": 1000, "description": "正则表达式" },
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "glob": { "type": "string", "description": "文件过滤 glob，如 **/*.rs" },
        "maxResults": { "type": "integer", "minimum": 1, "maximum": 5000, "default": 1000 },
        "contextLines": { "type": "integer", "minimum": 0, "maximum": 10, "default": 0 },
        "caseSensitive": { "type": "boolean", "default": false }
      }
    },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["path", "lineNumber", "line"],
            "properties": {
              "path": { "type": "string" },
              "lineNumber": { "type": "integer" },
              "startColumn": { "type": "integer" },
              "endColumn": { "type": "integer" },
              "line": { "type": "string", "description": "命中的整行内容" },
              "context": { "type": "array", "items": { "type": "string" }, "description": "contextLines > 0 时的上下文行" }
            }
          }
        }
      }
    }
  },
  "file.gitStatus": {
    "params": { "type": "object", "properties": { "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" } } },
    "result": {
      "type": "object",
      "required": ["changes"],
      "properties": {
        "branch": { "type": ["string", "null"] },
        "changes": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["path", "status", "staged"],
            "properties": {
              "path": { "type": "string" },
              "status": { "type": "string", "enum": ["added", "modified", "deleted", "renamed", "untracked", "conflicted"] },
              "staged": { "type": "boolean" }
            }
          }
        }
      }
    }
  },
  "file.diff": {
    "params": {
      "type": "object",
      "properties": {
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "workspaceId": { "type": "string" },
        "base": { "type": "string", "enum": ["HEAD", "index", "working"], "default": "working", "description": "diff 基准；working = 与工作区已保存内容对比" },
        "staged": { "type": "boolean", "default": false, "description": "仅暂存区 diff（等价 base=index）" }
      }
    },
    "result": { "type": "object", "required": ["diff"], "properties": { "diff": { "type": "string", "description": "unified diff 文本" }, "truncated": { "type": "boolean" } } }
  },
  "file.download": {
    "params": { "type": "object", "required": ["path"], "properties": { "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" }, "workspaceId": { "type": "string" } } },
    "result": {
      "type": "object",
      "required": ["name", "mimeType", "size"],
      "properties": {
        "name": { "type": "string" },
        "mimeType": { "type": "string" },
        "size": { "type": "integer" },
        "contentBase64": { "type": "string", "description": "文件内容（base64）。>16 MiB 时返回 413 类错误码 -32021，客户端分段读取" }
      }
    }
  },
  "file.upload": {
    "params": {
      "type": "object",
      "required": ["path"],
      "properties": {
        "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "workspaceId": { "type": "string" },
        "contentBase64": { "type": "string", "description": "与 contentText 二选一" },
        "contentText": { "type": "string" },
        "overwrite": { "type": "boolean", "default": false },
        "createDirs": { "type": "boolean", "default": false }
      }
    },
    "result": { "type": "object", "required": ["ok", "bytesWritten"], "properties": { "ok": { "const": true }, "bytesWritten": { "type": "integer" } } }
  }
}
```

错误：`-32020` 文件/目录不存在 · `-32021` 文件过大 · `-32022` 路径越界/被拒绝 · `-32003` 只读工作区写入被拒 · `-32024` Git 仓库错误（gitStatus/diff）。

### 6.12 Terminal 域

终端模式受能力协商 `terminalMode` 约束（§5.4）：`engine` 模式走本域正向 RPC；`host` 模式（ACP）由宿主提供 PTY，`terminal.create` 由服务端作为**反向 RPC** 发起（§8.4）。

```json
{
  "terminal.create": {
    "params": {
      "type": "object",
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId", "description": "关联会话（可选）；不关联时终端仍可用，只是事件不带 sessionId" },
        "name": { "type": "string", "maxLength": 100 },
        "cwd": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "env": { "type": "object", "additionalProperties": { "type": "string" } },
        "cols": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 80 },
        "rows": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 24 },
        "shell": { "type": "string", "description": "缺省取平台默认 shell" }
      }
    },
    "result": {
      "type": "object",
      "required": ["terminalId", "cols", "rows"],
      "properties": {
        "terminalId": { "type": "string", "description": "UUID" },
        "cols": { "type": "integer" },
        "rows": { "type": "integer" },
        "pty": { "type": "boolean", "description": "是否 PTY（非 PTY 时仅管道式捕获输出）" }
      }
    }
  },
  "terminal.list": {
    "params": { "type": "object", "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["terminalId", "running"],
            "properties": {
              "terminalId": { "type": "string" },
              "name": { "type": ["string", "null"] },
              "sessionId": { "type": ["string", "null"] },
              "cwd": { "type": ["string", "null"] },
              "running": { "type": "boolean" },
              "cols": { "type": "integer" },
              "rows": { "type": "integer" },
              "exitCode": { "type": ["integer", "null"] }
            }
          }
        }
      }
    }
  },
  "terminal.close": {
    "params": { "type": "object", "required": ["terminalId"], "properties": { "terminalId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "terminal.attach": {
    "params": {
      "type": "object",
      "required": ["terminalId"],
      "properties": {
        "terminalId": { "type": "string" },
        "sinceSeq": { "type": "integer", "description": "仅重放此 seq 之后的 terminal.output（若终端缓冲仍持有）" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok", "description": "attach 成功后，该终端输出以 terminal.output 事件推送" }
  },
  "terminal.input": {
    "params": {
      "type": "object",
      "required": ["terminalId"],
      "properties": {
        "terminalId": { "type": "string" },
        "data": { "type": "string", "description": "UTF-8 文本输入" },
        "dataBase64": { "type": "string", "description": "二进制输入（与 data 二选一）" }
      }
    },
    "result": { "type": "object", "required": ["ok", "bytes"], "properties": { "ok": { "const": true }, "bytes": { "type": "integer" } } }
  },
  "terminal.resize": {
    "params": {
      "type": "object",
      "required": ["terminalId", "cols", "rows"],
      "properties": {
        "terminalId": { "type": "string" },
        "cols": { "type": "integer", "minimum": 1 },
        "rows": { "type": "integer", "minimum": 1 }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32013` 终端不存在 · `-32024` 终端已退出（input/resize） · `-32004` 输出推送过频被限。

### 6.13 Task 域

```json
{
  "task.list": {
    "params": { "type": "object", "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" }, "status": { "type": "string", "enum": ["pending", "running", "paused", "completed", "failed", "cancelled"] } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["taskId", "status", "createdAt"],
            "properties": {
              "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
              "sessionId": { "type": ["string", "null"] },
              "title": { "type": ["string", "null"] },
              "status": { "type": "string", "enum": ["pending", "running", "paused", "completed", "failed", "cancelled"] },
              "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" },
              "completedAt": { "type": ["string", "null"] }
            }
          }
        }
      }
    }
  },
  "task.get": {
    "params": { "type": "object", "required": ["taskId"], "properties": { "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" } } },
    "result": { "type": "object", "description": "TaskDetail：task.list 字段 + result/error/description 等完整字段" }
  },
  "task.cancel": {
    "params": { "type": "object", "required": ["taskId"], "properties": { "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32014` 任务不存在 · `-32024` 任务已终态（cancel）。

### 6.14 Plan 域

```json
{
  "$id": "https://mirri.dev/protocol/v1/plan-doc",
  "type": "object",
  "required": ["steps"],
  "properties": {
    "title": { "type": "string" },
    "steps": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "description", "status"],
        "properties": {
          "id": { "type": "string" },
          "description": { "type": "string" },
          "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled", "failed"] },
          "dependsOn": { "type": "array", "items": { "type": "string" } }
        }
      }
    },
    "updatedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
  }
}
```

```json
{
  "plan.get": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": { "type": "object", "required": ["plan"], "properties": { "plan": { "anyOf": [ { "$ref": "https://mirri.dev/protocol/v1/plan-doc" }, { "type": "null" } ] } } }
  },
  "plan.clear": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "plan.enter": {
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "mode": { "type": "string", "enum": ["auto", "manual"], "default": "auto", "description": "进入计划模式：引擎暂停工具执行，先输出计划" }
      }
    },
    "result": { "type": "object", "required": ["ok", "mode"], "properties": { "ok": { "const": true }, "mode": { "type": "string" } } }
  },
  "plan.cancel": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在 · `-32016` 无活动计划（clear/enter 的异常场景按 `-32024` 返回）。计划状态变化（新增/步骤完成/清空）发 `plan.updated`（§7.14 补充事件）。

### 6.15 Goal 域

```json
{
  "goal.create": {
    "params": {
      "type": "object",
      "required": ["sessionId", "description"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "description": { "type": "string", "minLength": 1, "maxLength": 5000 },
        "scope": { "type": "string", "description": "作用域/关联任务，可选" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/goal" }
  },
  "goal.get": {
    "params": { "type": "object", "required": ["goalId"], "properties": { "goalId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/goal" }
  },
  "goal.pause": {
    "params": { "type": "object", "required": ["goalId"], "properties": { "goalId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "goal.resume": {
    "params": { "type": "object", "required": ["goalId"], "properties": { "goalId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "goal.cancel": {
    "params": { "type": "object", "required": ["goalId"], "properties": { "goalId": { "type": "string" }, "reason": { "type": "string", "maxLength": 500 } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/goal",
  "type": "object",
  "required": ["goalId", "sessionId", "description", "status", "createdAt"],
  "properties": {
    "goalId": { "type": "string" },
    "sessionId": { "type": "string" },
    "description": { "type": "string" },
    "status": { "type": "string", "enum": ["active", "paused", "cancelled", "completed"] },
    "progress": { "type": ["number", "null"], "minimum": 0, "maximum": 1 },
    "scope": { "type": ["string", "null"] },
    "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" },
    "completedAt": { "type": ["string", "null"] }
  }
}
```

错误：`-32015` goal 不存在 · `-32024` 状态机非法迁移。状态变化必发 `goal.updated`。

### 6.16 Shell 域

```json
{
  "shell.run": {
    "params": {
      "type": "object",
      "required": ["command"],
      "properties": {
        "command": { "type": "string", "minLength": 1, "maxLength": 100000 },
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId", "description": "关联会话（审计、环境继承）" },
        "cwd": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
        "env": { "type": "object", "additionalProperties": { "type": "string" } },
        "shell": { "type": "string", "description": "缺省平台默认" },
        "capture": { "type": "boolean", "default": true, "description": "捕获 stdout/stderr（false 时仅返回 exitCode）" },
        "background": { "type": "boolean", "default": false, "description": "true 时立即返回 taskId，输出走 background.task.* 事件（§7.9）" },
        "timeoutMs": { "type": "integer", "minimum": 100, "maximum": 3600000, "description": "缺省不限；超时 kill 并置 timedOut" }
      }
    },
    "result": {
      "type": "object",
      "required": ["taskId"],
      "properties": {
        "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
        "result": {
          "description": "background=false 时返回",
          "type": "object",
          "required": ["exitCode", "durationMs"],
          "properties": {
            "exitCode": { "type": ["integer", "null"] },
            "stdout": { "type": "string" },
            "stderr": { "type": "string" },
            "durationMs": { "type": "integer" },
            "timedOut": { "type": "boolean" }
          }
        }
      }
    }
  },
  "shell.cancel": {
    "params": {
      "type": "object",
      "required": ["taskId"],
      "properties": {
        "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
        "signal": { "type": "string", "enum": ["SIGINT", "SIGTERM", "SIGKILL"], "default": "SIGTERM" }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`shell.run` `-32602` 参数非法 · `-32022` cwd 越界 · `-32024` 命令启动失败；`shell.cancel` `-32018` 任务不存在。shell 命令执行受权限策略约束，未授权返回 `-32003`。

### 6.17 Cron 域

```json
{
  "cron.list": {
    "params": { "type": "object" },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": { "$ref": "https://mirri.dev/protocol/v1/cron-job" }
        }
      }
    }
  },
  "cron.create": {
    "params": {
      "type": "object",
      "required": ["expression", "task"],
      "properties": {
        "expression": { "type": "string", "description": "5 段 cron 表达式（本地时区）" },
        "task": {
          "type": "object",
          "required": ["type"],
          "properties": {
            "type": { "type": "string", "enum": ["shell", "message", "tool"] },
            "params": { "type": "object" }
          }
        },
        "timezone": { "type": "string", "default": "local" },
        "enabled": { "type": "boolean", "default": true }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/cron-job" }
  },
  "cron.delete": {
    "params": { "type": "object", "required": ["jobId"], "properties": { "jobId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

```json
{
  "$id": "https://mirri.dev/protocol/v1/cron-job",
  "type": "object",
  "required": ["jobId", "expression", "enabled", "task"],
  "properties": {
    "jobId": { "type": "string" },
    "expression": { "type": "string" },
    "timezone": { "type": "string" },
    "enabled": { "type": "boolean" },
    "nextFireAt": { "type": ["string", "null"], "description": "实际触发时间（含抖动后的真实值，客户端以此为准）" },
    "lastFireAt": { "type": ["string", "null"] },
    "lastStatus": { "type": ["string", "null"], "enum": [null, "success", "failed", "running"] },
    "task": { "type": "object", "properties": { "type": { "type": "string" }, "params": { "type": "object" } } }
  }
}
```

错误：`cron.create` `-32602` 表达式非法；`cron.delete` `-32017` job 不存在。任务触发后发 `background.task.started` / `background.task.terminated`。

**触发抖动**：服务端可对整分（`:00`/`:30`）触发时刻施加最多 **±60s** 抖动以错峰（负载均衡）；实际触发时间以 `cron-job.nextFireAt` 为准（每次查询/更新后反映真实值），客户端不得假设触发时刻恰为表达式命中的整分。

### 6.18 Background 域

```json
{
  "background.list": {
    "params": { "type": "object", "properties": { "status": { "type": "string", "enum": ["running", "completed", "failed", "stopped"] } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["taskId", "status", "startedAt"],
            "properties": {
              "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
              "name": { "type": ["string", "null"] },
              "status": { "type": "string", "enum": ["running", "completed", "failed", "stopped"] },
              "startedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" },
              "finishedAt": { "type": ["string", "null"] },
              "exitCode": { "type": ["integer", "null"] }
            }
          }
        }
      }
    }
  },
  "background.getOutput": {
    "params": {
      "type": "object",
      "required": ["taskId"],
      "properties": {
        "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
        "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "字符偏移；服务端仅保留有限环形缓冲，超界回滚到最早可用" },
        "limit": { "type": "integer", "minimum": 1, "maximum": 100000, "default": 10000 }
      }
    },
    "result": {
      "type": "object",
      "required": ["output", "nextOffset", "finished"],
      "properties": {
        "output": { "type": "string" },
        "nextOffset": { "type": "integer" },
        "finished": { "type": "boolean" },
        "totalChars": { "type": "integer", "description": "累计输出长度（可能超过缓冲）" }
      }
    }
  },
  "background.stop": {
    "params": { "type": "object", "required": ["taskId"], "properties": { "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" }, "signal": { "type": "string", "enum": ["SIGINT", "SIGTERM", "SIGKILL"], "default": "SIGTERM" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "background.detach": {
    "params": { "type": "object", "required": ["taskId"], "properties": { "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok", "description": "断开本连接的输出订阅；任务继续运行，其他连接/后台仍可取输出" }
  }
}
```

错误：`-32018` 任务不存在 · `-32024` 任务已终态（stop）。

### 6.19 Plugin 域

```json
{
  "plugin.list": {
    "params": { "type": "object", "properties": { "status": { "type": "string", "enum": ["loaded", "error", "disabled", "not_loaded"] } } },
    "result": {
      "type": "object",
      "required": ["items"],
      "properties": {
        "items": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["pluginId", "name", "version", "enabled", "status"],
            "properties": {
              "pluginId": { "type": "string" },
              "name": { "type": "string" },
              "version": { "type": "string" },
              "type": { "type": "string", "enum": ["builtin", "external"] },
              "enabled": { "type": "boolean" },
              "status": { "type": "string", "enum": ["loaded", "error", "disabled", "not_loaded"] },
              "error": { "type": ["string", "null"] }
            }
          }
        }
      }
    }
  },
  "plugin.install": {
    "params": {
      "type": "object",
      "required": ["source"],
      "properties": {
        "source": { "type": "string", "description": "本地目录路径 / 归档路径 / 注册表包名（以 URL scheme 区分）" },
        "name": { "type": "string" },
        "force": { "type": "boolean", "default": false, "description": "覆盖同名已装插件" }
      }
    },
    "result": { "type": "object", "required": ["ok", "pluginId"], "properties": { "ok": { "const": true }, "pluginId": { "type": "string" } } }
  },
  "plugin.enable": {
    "params": { "type": "object", "required": ["pluginId"], "properties": { "pluginId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "plugin.disable": {
    "params": { "type": "object", "required": ["pluginId"], "properties": { "pluginId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "plugin.remove": {
    "params": { "type": "object", "required": ["pluginId"], "properties": { "pluginId": { "type": "string" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  },
  "plugin.reload": {
    "params": { "type": "object", "properties": { "pluginId": { "type": "string" } } },
    "result": { "type": "object", "required": ["ok", "reloaded"], "properties": { "ok": { "const": true }, "reloaded": { "type": "array", "items": { "type": "string" } } } }
  },
  "plugin.getInfo": {
    "params": { "type": "object", "required": ["pluginId"], "properties": { "pluginId": { "type": "string" } } },
    "result": {
      "type": "object",
      "required": ["info"],
      "properties": {
        "info": {
          "type": "object",
          "required": ["pluginId", "name", "version", "enabled", "status"],
          "properties": {
            "pluginId": { "type": "string" },
            "name": { "type": "string" },
            "version": { "type": "string" },
            "description": { "type": "string" },
            "author": { "type": "string" },
            "type": { "type": "string", "enum": ["builtin", "external"] },
            "enabled": { "type": "boolean" },
            "status": { "type": "string", "enum": ["loaded", "error", "disabled", "not_loaded"] },
            "error": { "type": ["string", "null"] },
            "commands": {
              "type": "array",
              "items": {
                "type": "object",
                "required": ["name"],
                "properties": {
                  "name": { "type": "string" },
                  "description": { "type": "string" }
                }
              }
            }
          }
        }
      }
    }
  },
  "plugin.activateCommand": {
    "params": {
      "type": "object",
      "required": ["pluginId", "command"],
      "properties": {
        "pluginId": { "type": "string" },
        "command": { "type": "string", "description": "插件注册的命令名（见 plugin.getInfo.info.commands）" },
        "args": { "type": "array", "items": { "type": "string" } }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32019` 插件不存在 · `plugin.install` `-32602` 安装源非法 · `plugin.activateCommand` `-32602`/`-32024` 命令不存在或执行失败（`data.details` 带错误信息）· `-32024` 插件加载失败。插件加载/卸载后发 `tool.list.updated`（若注册了工具）。

### 6.20 Command 域

斜杠命令（slash command）目录与执行。

```json
{
  "command.list": {
    "params": { "type": "object", "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId", "description": "缺省返回全局命令" } } },
    "result": {
      "type": "object",
      "required": ["commands"],
      "properties": {
        "commands": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["name", "description"],
            "properties": {
              "name": { "type": "string", "description": "斜杠命令名（不含 / 前缀）" },
              "description": { "type": "string" },
              "argsSchema": { "type": "object", "description": "参数 JSON Schema（可选）" },
              "category": { "type": "string" }
            }
          }
        }
      }
    }
  },
  "command.run": {
    "params": {
      "type": "object",
      "required": ["command"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId", "description": "命令执行的会话上下文（可选）" },
        "command": { "type": "string", "description": "斜杠命令名（不含 / 前缀）" },
        "args": { "type": "array", "items": { "type": "string" } }
      },
      "additionalProperties": false
    },
    "result": {
      "type": "object",
      "required": ["output"],
      "properties": {
        "output": { "type": "string", "description": "命令输出文本" },
        "exitCode": { "type": ["integer", "null"] }
      },
      "additionalProperties": false
    }
  }
}
```

错误：`command.run` `-32024` 命令不存在/执行失败（`data.details` 带原因）· `-32000` 会话不存在。

### 6.21 Prompt 域

排队中的 prompt 管理（`message.steer mode=queue` 的批量视图与兜底入口）。

```json
{
  "prompt.list": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": {
      "type": "object",
      "required": ["prompts"],
      "properties": {
        "prompts": {
          "type": "array",
          "items": {
            "type": "object",
            "required": ["promptId", "status", "createdAt"],
            "properties": {
              "promptId": { "type": "string" },
              "text": { "type": "string" },
              "status": { "type": "string", "enum": ["queued", "running", "aborted", "consumed"] },
              "createdAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
            }
          }
        }
      }
    }
  },
  "prompt.steer": {
    "params": { "type": "object", "required": ["sessionId"], "properties": { "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" } } },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" },
    "description": "将排队中的 prompt 批量提交为 turn 输入（message.steer mode=queue 的批量/兜底入口；若 message.steer 已覆盖单条注入，本方法负责整体放行）"
  },
  "prompt.abort": {
    "params": {
      "type": "object",
      "required": ["sessionId", "promptId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "promptId": { "type": "string" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在 · `prompt.abort` `-32024` promptId 不存在/已消费。

### 6.22 Question 域（正向入口）

question 域的正向 RPC 入口：反向 RPC `question.ask` 定义见 §8.2；`question.respond` 为多渠道应答入口（§8.2，参数 `{sessionId, requestId, answer?, dismissed?}`）。

```json
{
  "question.dismiss": {
    "params": {
      "type": "object",
      "required": ["sessionId", "questionId"],
      "properties": {
        "sessionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/SessionId" },
        "questionId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/RequestId" }
      },
      "additionalProperties": false
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32000` 会话不存在 · `-32008` question 不存在/已应答。成功后发 `question.dismissed(reason=user_dismissed)`。

### 6.23 方法目录索引

| 域 | 方法 | 计数 |
|----|------|------|
| 连接级 | `session.subscribe` / `session.unsubscribe` / `ping` / `pong`（`session.snapshot` 见 §6.1） | 4 |
| Session | `create` `list` `get` `close` `resume` `fork` `export` `archive` `restore` `snapshot` `reload` `rename` `updateMeta` `clearContext` `compact` `cancelCompaction` `undo` `status` `warnings` `startBtw` `children` | 21 |
| Message | `send` `abort` `steer` `list` | 4 |
| Tool | `list` `call` `catalog` | 3 |
| Agent | `list` `get` `create` `setModel` `setThinking` `setPermission` `setSwarmMode` `swarm` `getContext` `getUsage` `generateAgentsMd` `addAdditionalDir` | 12 |
| Workspace | `list` `create` `delete` `refresh` | 4 |
| Config | `get` `set` `watch` `getDiagnostics` `getExperimentalFeatures` `replaceSections` `removeProvider` | 7 |
| Skill | `list` `activate` `listForWorkspace` | 3 |
| MCP | `list` `reconnect` `startupMetrics`（契约面，both 面）+ 规划面 9 方法（见 §6.8 与 MCP spec） | 3 |
| Model | `list` `listProviders` `addProvider` `deleteProvider` `refreshProvider` | 5 |
| Auth | `status` `login` `logout` | 3 |
| File | `list` `read` `search` `grep` `gitStatus` `diff` `download` `upload` | 8 |
| Terminal | `create` `list` `close` `attach` `input` `resize` | 6 |
| Task | `list` `get` `cancel` | 3 |
| Plan | `get` `clear` `enter` `cancel` | 4 |
| Goal | `create` `get` `pause` `resume` `cancel` | 5 |
| Shell | `run` `cancel` | 2 |
| Cron | `list` `create` `delete` | 3 |
| Background | `list` `getOutput` `stop` `detach` | 4 |
| Plugin | `list` `install` `enable` `disable` `remove` `reload` `getInfo` `activateCommand` | 8 |
| Command | `list` `run` | 2 |
| Prompt | `list` `steer` `abort` | 3 |
| Question | `dismiss`（`respond` 见 §8.2） | 1 |
| **合计** | | **120（提案目录）** |

> **事实源声明**：本目录是设计期提案快照（120 方法）。方法面的落地事实源是 `rust/crates/mirri-proto/src/rpc/registry.rs`（注册表自带 pin 断言与 disposition 分类，以注册表为准）；两目录间的命名差异以注册表为准；新增域方法（如 MCP 配置面，§6.8）落地时同步入注册表。

兼容映射（落地核实：`approval.resolve` → 实名 `approval.decide`，registry.rs:522 both；`question.answer` 保留原名未改，registry.rs:538 both；旧 WS `interrupt` 帧 → `message.abort`；旧 WS `subscribe/unsubscribe` 帧 → `session.subscribe/unsubscribe`；旧 WS `ping/pong` 帧 → `ping/pong` 通知）。---

## 7. 事件目录

### 7.0 通用约定

事件 = 服务端发出的**通知**（`method` 即事件名，无 `id`）。所有事件遵守：

- 事件负载（params）含 `sessionId`（连接级事件如 `config.changed` 可省）。
- **`x-volatile: false`（durable，默认）**：事件持久化到会话事件日志，携带 `seq`（本会话单调递增，从 1 起）与 `epoch`（日志代次标识），可被 cursor 重放（§9）。
- **`x-volatile: true`**：不落日志、**不携带 `seq`/`epoch`**、不推进 cursor；重连后客户端不得指望恢复（快照/后续 durable 事件会覆盖其语义）。
- `x-volatile` 是 schema 注解，仅用于目录说明与规范测试，不影响校验。
- 事件类型的检查顺序（客户端实现指南）：先读 `seq` 判重放，再按类型分发；未知事件类型必须忽略（前向兼容）。

事件通用信封（durable）：

```json
{
  "$id": "https://mirri.dev/protocol/v1/event-envelope",
  "type": "object",
  "required": ["jsonrpc", "method", "params"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "method": { "type": "string", "description": "事件名，如 turn.started" },
    "params": {
      "type": "object",
      "properties": {
        "sessionId": { "type": "string", "description": "durable 事件必带" },
        "seq": { "type": "integer", "minimum": 1, "description": "仅 durable 事件携带；本会话事件日志位置" },
        "epoch": { "type": "string", "description": "仅 durable 事件携带；日志代次，变化即需 resync（§9.3）" },
        "timestamp": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
      },
      "additionalProperties": true
    }
  }
}
```

### 7.1 Session 生命周期

| 事件 | volatile | seq | 触发 |
|------|:--:|:--:|------|
| `session.created` | ✗ | ✅ | `session.create` / `session.restore` 成功 |

```json
{
  "session.created": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "title", "workspaceId"],
      "properties": {
        "sessionId": { "type": "string" }, "title": { "type": "string" },
        "workspaceId": { "type": ["string", "null"] }, "agentIds": { "type": "array", "items": { "type": "string" } }
      }
    }
  }
}
```

| 事件 | volatile | seq | 触发 |
|------|:--:|:--:|------|
| `session.updated` | ✗ | ✅ | `session.resume` / `session.fork` 等结构性变化 |
| `session.deleted` | ✗ | ✅ | `session.close` / `session.archive`；**会话关闭前最后一条 durable 事件** |
| `session.meta.updated` | ✗ | ✅ | 标题/tags/元数据变更（`session.updateMeta` / `session.rename` 触发；`session.updated` 之外的元数据子集） |
| `session.model_changed` | ✗ | ✅ | `agent.setModel` 成功（payload: `{sessionId, agentId, model}`） |
| `session.context_cleared` | ✗ | ✅ | `session.clearContext` 成功（payload: `{sessionId, agentId}`） |
| `session.undone` | ✗ | ✅ | `session.undo` 成功（payload: `{sessionId, removedSteps}`） |
| `session.work_changed` | ✗ | ✅ | 会话运行状态四元组发生真实迁移（会话管理域 US4.4/SDR-013）；**全局扇出**到每个连接（不要求订阅该会话——列表行的状态更新不依赖逐会话订阅）；回合结束的迁移排在对应 `turn.ended` **之后** |

```json
{
  "session.updated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "type": "string" },
        "change": { "type": "string", "enum": ["resumed", "forked", "status_changed", "agent_added", "agent_removed"] },
        "title": { "type": "string" },
        "status": { "type": "string", "enum": ["live", "closed", "archived"] }
      }
    }
  },
  "session.deleted": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "reason"],
      "properties": {
        "sessionId": { "type": "string" },
        "reason": { "type": "string", "enum": ["user_initiated", "idle_timeout", "shutdown", "archived"] }
      }
    }
  },
  "session.meta.updated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "type": "string" },
        "title": { "type": "string" },
        "tags": { "type": "array", "items": { "type": "string" } },
        "workspaceId": { "type": ["string", "null"] },
        "changedKeys": { "type": "array", "items": { "type": "string" } }
      }
    }
  },
  "session.model_changed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId", "model"],
      "properties": {
        "sessionId": { "type": "string" },
        "agentId": { "type": "string" },
        "model": { "type": "string" }
      }
    }
  },
  "session.context_cleared": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId"],
      "properties": {
        "sessionId": { "type": "string" },
        "agentId": { "type": "string" }
      }
    }
  },
  "session.undone": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "removedSteps"],
      "properties": {
        "sessionId": { "type": "string" },
        "removedSteps": { "type": "integer" }
      }
    }
  },
  "session.work_changed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "busy", "main_turn_active"],
      "properties": {
        "sessionId": { "type": "string" },
        "busy": { "type": "boolean", "description": "聚合忙碌：任一成员回合在途或后台工作存活" },
        "main_turn_active": { "type": "boolean", "description": "主对话成员的回合未收尾——列表'运行中'标记的判定输入" },
        "pending_interaction": { "type": "string", "enum": ["none", "approval", "question"] },
        "last_turn_reason": { "type": "string", "enum": ["completed", "cancelled", "failed"] }
      }
    }
  }
}
```

### 7.2 Turn 生命周期

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `turn.started` | ✗ | ✅ | turn 开始（含 `turnId`、model、agentId） |
| `turn.step.started` | ✗ | ✅ | step 开始（LLM 一次生成或一次工具调用） |
| `turn.step.completed` | ✗ | ✅ | step 完成（含内容/工具结果摘要） |
| `turn.step.interrupted` | ✗ | ✅ | step 被中断（abort/steer/超时） |
| `turn.step.retrying` | ✓ | 🚫 | 失败重试提示；随后必有 step.completed/interrupted |
| `turn.ended` | ✗ | ✅ | turn 结束，负载即 §6.2 的 `TurnEnd` |

```json
{
  "turn.started": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId", "turnId", "model"],
      "properties": {
        "sessionId": { "type": "string" }, "agentId": { "type": "string" },
        "turnId": { "type": "string" }, "model": { "type": "string" },
        "promptSummary": { "type": "string", "description": "用户输入摘要（≤120 字符）" }
      }
    }
  },
  "turn.step.started": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId", "stepType"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "stepType": { "type": "string", "enum": ["generate", "tool_call", "subagent", "shell"] },
        "toolName": { "type": "string", "description": "stepType=tool_call 时" },
        "toolCallId": { "type": "string" }
      }
    }
  },
  "turn.step.completed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "durationMs": { "type": "integer" },
        "contentSummary": { "type": "string", "description": "生成内容摘要（≤500 字符）" },
        "toolResultSummary": { "type": "object", "description": "工具结果摘要（isError/note/truncated）" },
        "tokensUsed": { "type": "object" }
      }
    }
  },
  "turn.step.interrupted": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId", "reason"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "reason": { "type": "string", "enum": ["abort", "steer", "timeout", "max_tokens", "hook"] }
      }
    }
  },
  "turn.step.retrying": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId", "attempt", "error"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "attempt": { "type": "integer", "minimum": 1 }, "error": { "type": "string" }
      }
    }
  },
  "turn.ended": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" },
        "stopReason": { "type": "string", "enum": ["end_turn", "max_tokens", "filtered", "paused", "unknown", "aborted", "blocked_by_hook"] },
        "steps": { "type": "integer" },
        "usage": { "type": "object", "required": ["inputTokens", "outputTokens"], "properties": { "inputTokens": { "type": "integer" }, "outputTokens": { "type": "integer" } } },
        "durationMs": { "type": "integer" },
        "blockedByPromptHook": { "type": "boolean" }
      }
    }
  }
}
```

### 7.3 流式输出

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `assistant.delta` | ✓ | 🚫 | 正文增量（`done` 为真后不再有该 turn 的 delta） |
| `thinking.delta` | ✓ | 🚫 | 思考过程增量（客户端可折叠展示） |

```json
{
  "assistant.delta": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId", "textDelta", "done"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "textDelta": { "type": "string", "description": "UTF-8 增量；块间空白保留" },
        "done": { "type": "boolean", "description": "true 表示本 step 输出结束（最后一条 delta）" }
      }
    }
  },
  "thinking.delta": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "stepId", "textDelta", "done"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" }, "stepId": { "type": "string" },
        "textDelta": { "type": "string" }, "done": { "type": "boolean" }
      }
    }
  }
}
```

### 7.4 工具调用

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `tool.call.started` | ✗ | ✅ | 工具调用开始（与 `turn.step.started` 的 tool_call 对应，含参数摘要） |
| `tool.call.delta` | ✓ | 🚫 | 工具流式输出（长输出工具） |
| `tool.progress` | ✓ | 🚫 | 进度百分比/阶段（长任务工具，如构建） |
| `tool.result` | ✗ | ✅ | 工具结果（完整 `ToolResult`；负载与 §6.3 一致） |

```json
{
  "tool.call.started": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "toolCallId", "toolName"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" },
        "toolCallId": { "type": "string" }, "toolName": { "type": "string" },
        "argsSummary": { "type": "string", "description": "参数 JSON 摘要（≤500 字符；敏感字段打码）" }
      }
    }
  },
  "tool.call.delta": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["sessionId", "toolCallId", "data", "done"],
      "properties": {
        "sessionId": { "type": "string" }, "toolCallId": { "type": "string" },
        "data": { "type": "string" }, "done": { "type": "boolean" }
      }
    }
  },
  "tool.progress": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["sessionId", "toolCallId"],
      "properties": {
        "sessionId": { "type": "string" }, "toolCallId": { "type": "string" },
        "percent": { "type": "number", "minimum": 0, "maximum": 1 },
        "phase": { "type": "string" },
        "message": { "type": "string" }
      }
    }
  },
  "tool.result": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "turnId", "toolCallId", "toolName", "result"],
      "properties": {
        "sessionId": { "type": "string" }, "turnId": { "type": "string" },
        "toolCallId": { "type": "string" }, "toolName": { "type": "string" },
        "result": {
          "type": "object", "required": ["output", "isError"],
          "properties": {
            "output": { "anyOf": [ { "type": "string" }, { "type": "array" } ] },
            "isError": { "type": "boolean" },
            "note": { "type": ["string", "null"] },
            "stopTurn": { "type": "boolean" },
            "truncated": { "type": "boolean" }
          }
        }
      }
    }
  }
}
```

### 7.5 Agent

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `agent.status.updated` | ✗ | ✅ | agent 状态迁移（idle/running/paused/suspended/error/stopped） |
| `agent.swarm_dispatched` | ✗ | ✅ | `agent.swarm` 分发完成（payload: `{sessionId, dispatchedAgents}`） |

```json
{
  "agent.status.updated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId", "status"],
      "properties": {
        "sessionId": { "type": "string" }, "agentId": { "type": "string" },
        "status": { "$ref": "https://mirri.dev/protocol/v1/agent-status" },
        "previousStatus": { "type": "string" },
        "turnId": { "type": ["string", "null"] }
      }
    }
  },
  "agent.swarm_dispatched": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "dispatchedAgents"],
      "properties": {
        "sessionId": { "type": "string" },
        "dispatchedAgents": { "type": "array", "items": { "type": "string" } }
      }
    }
  }
}
```

### 7.6 Approval

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `approval.requested` | ✗ | ✅ | 新的待审批请求（与反向 RPC `approval.ask` 伴生，事件版用于多客户端广播） |
| `approval.resolved` | ✗ | ✅ | 审批已裁决（approved/denied + 裁决方） |
| `approval.expired` | ✗ | ✅ | 审批超时自动拒绝 |

```json
{
  "approval.requested": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "toolName"],
      "properties": {
        "sessionId": { "type": "string" },
        "requestId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/RequestId" },
        "agentId": { "type": "string" },
        "toolName": { "type": "string" }
      }
    }
  },
  "approval.resolved": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "approved"],
      "properties": {
        "sessionId": { "type": "string" },
        "requestId": { "type": "string" },
        "approved": { "type": "boolean" },
        "reason": { "type": ["string", "null"] },
        "resolvedBy": { "type": "string", "description": "裁决方标识（connectionId / method）" }
      }
    }
  },
  "approval.expired": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId"],
      "properties": { "sessionId": { "type": "string" }, "requestId": { "type": "string" } }
    }
  }
}
```

> `approval.requested` 是**瘦通知**：只携带 id/工具名/归属，`display`/`action`/`approval_rule` 经反向 RPC `approval.ask` 传递（伴生关系）。同时收到两者的客户端必须按字段合并（先到的行 + 后到的富化字段），不得因事件先到而丢弃 ask 的展示载荷。

### 7.7 Question

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `question.requested` | ✗ | ✅ | 新提问（与反向 RPC `question.ask` 伴生） |
| `question.answered` | ✗ | ✅ | 已回答 |
| `question.dismissed` | ✗ | ✅ | 被跳过/超时 |

```json
{
  "question.requested": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "question"],
      "properties": {
        "sessionId": { "type": "string" },
        "requestId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/RequestId" },
        "turnId": { "type": ["string", "null"] },
        "question": { "type": "string" },
        "choices": { "type": "array", "items": { "type": "string" }, "description": "可选；非空时客户端应渲染为选项" },
        "expiresAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
      }
    }
  },
  "question.answered": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "answer"],
      "properties": {
        "sessionId": { "type": "string" }, "requestId": { "type": "string" }, "answer": { "type": "string" }
      }
    }
  },
  "question.dismissed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "reason"],
      "properties": {
        "sessionId": { "type": "string" }, "requestId": { "type": "string" },
        "reason": { "type": "string", "enum": ["timeout", "user_dismissed", "turn_ended"] }
      }
    }
  }
}
```

### 7.8 Background

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `background.task.started` | ✗ | ✅ | 后台任务开始（shell/cron/任务） |
| `background.task.terminated` | ✗ | ✅ | 后台任务终态（completed/failed/stopped） |

```json
{
  "background.task.started": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["taskId", "name", "startedAt"],
      "properties": {
        "taskId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/TaskId" },
        "name": { "type": "string" },
        "sessionId": { "type": ["string", "null"] },
        "startedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
      }
    }
  },
  "background.task.terminated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["taskId", "status", "finishedAt"],
      "properties": {
        "taskId": { "type": "string" },
        "status": { "type": "string", "enum": ["completed", "failed", "stopped"] },
        "exitCode": { "type": ["integer", "null"] },
        "error": { "type": ["string", "null"] },
        "finishedAt": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Timestamp" }
      }
    }
  }
}
```

### 7.9 Goal / Skill / Compaction / Subagent

| 事件 | volatile | seq | 触发 |
|------|:--:|:--:|------|
| `goal.updated` | ✗ | ✅ | goal 状态/进度变化（创建、暂停、恢复、取消、完成） |
| `skill.activated` | ✗ | ✅ | `skill.activate` 成功 |
| `compaction.started` | ✗ | ✅ | 上下文压缩开始 |
| `compaction.completed` | ✗ | ✅ | 压缩完成（含新摘要、丢弃统计） |
| `compaction.cancelled` | ✗ | ✅ | 压缩被取消 |
| `subagent.spawned` | ✗ | ✅ | 子 agent 创建（含父级关联） |
| `subagent.started` | ✗ | ✅ | 子 agent 开始执行 |
| `subagent.suspended` | ✗ | ✅ | 子 agent 挂起（等待外部输入/审批） |
| `subagent.completed` | ✗ | ✅ | 子 agent 正常结束（含结果摘要） |
| `subagent.failed` | ✗ | ✅ | 子 agent 失败（含错误） |

```json
{
  "goal.updated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "goalId", "status"],
      "properties": {
        "sessionId": { "type": "string" }, "goalId": { "type": "string" },
        "status": { "type": "string", "enum": ["active", "paused", "cancelled", "completed"] },
        "progress": { "type": ["number", "null"], "minimum": 0, "maximum": 1 },
        "summary": { "type": "string" }
      }
    }
  },
  "skill.activated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "skillId", "skillName"],
      "properties": {
        "sessionId": { "type": "string" }, "skillId": { "type": "string" }, "skillName": { "type": "string" }
      }
    }
  },
  "compaction.started": {
    "x-volatile": false,
    "params": { "type": "object", "required": ["sessionId", "trigger"], "properties": { "sessionId": { "type": "string" }, "trigger": { "type": "string", "enum": ["context_full", "manual", "auto"] } } }
  },
  "compaction.completed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "type": "string" },
        "messagesBefore": { "type": "integer" }, "messagesAfter": { "type": "integer" },
        "tokensSaved": { "type": "integer" },
        "summary": { "type": "string" }
      }
    }
  },
  "compaction.cancelled": {
    "x-volatile": false,
    "params": { "type": "object", "required": ["sessionId", "reason"], "properties": { "sessionId": { "type": "string" }, "reason": { "type": "string" } } }
  },
  "subagent.spawned": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "subagentId", "parentAgentId"],
      "properties": {
        "sessionId": { "type": "string" },
        "subagentId": { "type": "string" }, "parentAgentId": { "type": "string" },
        "task": { "type": "string", "description": "任务描述摘要" },
        "model": { "type": "string" }
      }
    }
  },
  "subagent.started": {
    "x-volatile": false,
    "params": { "type": "object", "required": ["sessionId", "subagentId", "startedAt"], "properties": { "sessionId": { "type": "string" }, "subagentId": { "type": "string" }, "startedAt": { "type": "string" } } }
  },
  "subagent.suspended": {
    "x-volatile": false,
    "params": { "type": "object", "required": ["sessionId", "subagentId", "reason"], "properties": { "sessionId": { "type": "string" }, "subagentId": { "type": "string" }, "reason": { "anyOf": [ { "type": "object", "required": ["kind"], "properties": { "kind": { "type": "string", "enum": ["approval", "question", "rate_limit"] }, "request_id": { "type": "string" }, "tool_call_id": { "type": "string" } } }, { "type": "string", "description": "迁移窗内旧版自由字符串 reason（Legacy）" } ] } } } }
  },
  "subagent.completed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "subagentId"],
      "properties": {
        "sessionId": { "type": "string" }, "subagentId": { "type": "string" },
        "resultSummary": { "type": "string" }, "durationMs": { "type": "integer" }
      }
    }
  },
  "subagent.failed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "subagentId", "error"],
      "properties": { "sessionId": { "type": "string" }, "subagentId": { "type": "string" }, "error": { "type": "string" } }
    }
  }
}
```

### 7.10 MCP

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `event.mcp.server.status` | ✗ | ✅ | server 连接状态变化（规划，Rollout P3——命名沿用既有 session 事件投影 `mcp.server.status`；本目录曾提案改名 `serverChanged`，经 MCP 域 ADR-010（2026-09-08）统一回既有名，负载扩展 pool/变更类别与双订阅面见该裁决） |
| `tool.list.updated` | ✗ | ✅ | 工具集合变化（MCP 连接/插件加载卸载/开关工具） |

```json
{
  "event.mcp.server.status": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["name", "pool", "status"],
      "properties": {
        "name": { "type": "string" },
        "pool": { "type": "string", "enum": ["user", "workspace", "plugin"] },
        "status": { "type": "string", "enum": ["pending", "connected", "failed", "disabled", "needs-auth"] },
        "error": { "type": ["string", "null"] }
      }
    }
  },
  "tool.list.updated": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId"],
      "properties": {
        "sessionId": { "type": "string" },
        "agentId": { "type": ["string", "null"] },
        "reason": { "type": "string", "enum": ["mcp_connected", "mcp_disconnected", "plugin_loaded", "plugin_unloaded", "tool_toggled"] }
      }
    }
  }
}
```

> 事件名对齐既有 `event.<type>` 惯例与 [MCP spec](../mcp-architecture.md) 4.3；状态枚举为五态（`pending/connected/failed/disabled/needs-auth`，MCP spec N7）。

### 7.11 Config 与能力

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `config.changed` | ✗ | ✅ | `config.set` / `config.replaceSections` / `config.removeProvider` 或文件监听（外部编辑）触发；**连接级事件，无 sessionId** |
| `capabilities_changed` | ✗ | ✅ | 服务端能力变化（热更新/配置变更）；**连接级事件，无 sessionId** |

> **统一说明**：doc 06 的 `config.reloaded` 事件**不单独定义**——`config.changed` 同时覆盖 RPC 触发（`config.set` 等）与文件监听触发的配置变更两种来源，客户端无需区分事件名。

```json
{
  "config.changed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["changedKeys"],
      "properties": {
        "changedKeys": { "type": "array", "items": { "type": "string" } },
        "values": { "type": "object", "description": "变更后的值（仅已订阅 keys）" },
        "persist": { "type": "boolean" }
      }
    }
  },
  "capabilities_changed": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["addedCapabilities", "removedCapabilities", "reason"],
      "properties": {
        "addedCapabilities": { "type": "array", "items": { "type": "string" } },
        "removedCapabilities": { "type": "array", "items": { "type": "string" } },
        "reason": { "type": "string", "description": "变化原因，如 config_reloaded / plugin_loaded / experimental_toggle" }
      }
    }
  }
}
```

### 7.12 错误与告警（连接级）

| 事件 | volatile | seq | 说明 |
|------|:--:|:--:|------|
| `error` | ✓ | 🚫 | 无对应请求的异步错误（任务失败通知等）；或请求已应答后补充的错误信息 |
| `warning` | ✓ | 🚫 | 异步告警（限流提示、性能告警、过时用法提示） |

```json
{
  "error": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["code", "message"],
      "properties": {
        "code": { "type": "integer", "description": "错误码（§10）" },
        "message": { "type": "string" },
        "data": { "type": "object" },
        "sessionId": { "type": "string" },
        "relatedRequestId": { "type": ["string", "null"], "description": "关联的已应答请求 id（若有）" }
      }
    }
  },
  "warning": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["code", "message"],
      "properties": {
        "code": { "type": "string", "description": "警告类别码，如 rate_limit_warning / deprecated_method" },
        "message": { "type": "string" },
        "sessionId": { "type": "string" }
      }
    }
  }
}
```

### 7.13 补充事件（现有 bus 事件的对齐映射）

以下事件在现有 `mirri-core` `SessionEvent` bus 上已存在，统一协议将其显式化；负载与现有变体保持一致（snake_case 转 camelCase）：

| 事件 | volatile | seq | 对应现有 `SessionEvent` |
|------|:--:|:--:|------|
| `message.received` | ✗ | ✅ | `MessageReceived`（user 消息确认入上下文） |
| `agent.created` | ✗ | ✅ | `AgentCreated` |
| `agent.removed` | ✗ | ✅ | `AgentRemoved` |
| `terminal.output` | ✓ (engine 模式) | 🚫 | 终端输出（高频率，不落日志） |
| `terminal.exit` | ✗ | ✅ | 终端进程退出（含 exitCode） |
| `plan.updated` | ✗ | ✅ | 计划文档变化（新增/步骤状态/清空） |
| `session.subscribed` / `session.unsubscribed` | ✓ | 🚫 | 订阅确认（对应旧协议 subscribed/unsubscribed 帧） |
| `auth.status.changed` | ✗ | ✅ | 认证状态变化（登录/登出） |

```json
{
  "message.received": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["sessionId", "agentId", "role", "contentSummary"],
      "properties": {
        "sessionId": { "type": "string" }, "agentId": { "type": "string" },
        "role": { "type": "string", "enum": ["user", "assistant", "system", "tool"] },
        "contentSummary": { "type": "string" }
      }
    }
  },
  "terminal.output": {
    "x-volatile": true,
    "params": {
      "type": "object",
      "required": ["terminalId", "data"],
      "properties": {
        "terminalId": { "type": "string" },
        "sessionId": { "type": ["string", "null"] },
        "data": { "type": "string", "description": "PTY 输出块（UTF-8；含 ANSI 转义）" }
      }
    }
  },
  "terminal.exit": {
    "x-volatile": false,
    "params": {
      "type": "object",
      "required": ["terminalId", "exitCode"],
      "properties": {
        "terminalId": { "type": "string" },
        "sessionId": { "type": ["string", "null"] },
        "exitCode": { "type": ["integer", "null"] }
      }
    }
  }
}
```

### 7.14 事件目录索引

| 分组 | 事件 | durable | volatile |
|------|------|:--:|:--:|
| Session | session.created / session.updated / session.deleted / session.meta.updated / session.model_changed / session.context_cleared / session.undone / session.work_changed | 8 | — |
| Turn | turn.started / turn.step.started / turn.step.completed / turn.step.interrupted / turn.ended | 5 | — |
| Turn（瞬时） | turn.step.retrying | — | 1 |
| 流式 | assistant.delta / thinking.delta | — | 2 |
| Tool | tool.call.started / tool.result | 2 | — |
| Tool（瞬时） | tool.call.delta / tool.progress | — | 2 |
| Agent | agent.status.updated / agent.swarm_dispatched | 2 | — |
| Approval | approval.requested / approval.resolved / approval.expired | 3 | — |
| Question | question.requested / question.answered / question.dismissed | 3 | — |
| Background | background.task.started / background.task.terminated | 2 | — |
| Goal | goal.updated | 1 | — |
| Skill | skill.activated | 1 | — |
| Compaction | compaction.started / compaction.completed / compaction.cancelled | 3 | — |
| Subagent | subagent.spawned / subagent.started / subagent.suspended / subagent.completed / subagent.failed | 5 | — |
| MCP | event.mcp.server.status / tool.list.updated | 2 | — |
| Config | config.changed | 1 | — |
| 能力协商 | capabilities_changed | 1 | — |
| 错误 | error / warning | — | 2 |
| 补充 | message.received / agent.created / agent.removed / terminal.exit / plan.updated / auth.status.changed / session.subscribed / session.unsubscribed | 6 | 2 |
| **合计** | | **44** | **9** |

---

## 8. 反向 RPC

反向 RPC = 服务端主动发出的**请求帧**（带 `id`），客户端必须应答（或超时）。用途：需要客户端介入的交互（审批、提问、文件代理、宿主资源）。

**通用规则**：

1. 客户端在 `client_hello` 的 `capabilities.reverseRpc` 白名单中声明支持的反向方法前缀；服务端不得发起白名单外的方法。未命中白名单时服务端改用事件广播 + 正向 RPC 回调（§7.6 的 `approval.requested` + `approval.respond` 降级路径）。
2. 请求 id 由服务端生成（UUID），与业务 `requestId` 解耦但建议同值（本协议中二者保持一致）。
3. 应答：普通 JSON-RPC Response（`result` 或 `error`）。客户端必须对每个反向请求在超时窗口内应答。
4. 超时：由各流程自行定义（见下）；超时后服务端按流程语义处理（自动拒绝/降级/失败），并向客户端补发对应事件。

### 8.1 approval.ask → approval.respond

工具权限门：turn 内工具执行被策略判定为 `Ask` 时，服务端向连接发起 `approval.ask`，等待客户端裁决。

```json
{
  "approval.ask": {
    "x-role": "server → client request",
    "type": "object",
    "required": ["jsonrpc", "id", "method", "params"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string", "description": "即 requestId" },
      "method": { "const": "approval.ask" },
      "params": {
        "type": "object",
        "required": ["request_id", "tool_name", "tool_call_id", "action", "display"],
        "properties": {
          "request_id": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/RequestId", "description": "审批请求 id，同时是 approval.decide 接受的 id（先到先裁决）" },
          "tool_name": { "type": "string" },
          "tool_call_id": { "type": "string" },
          "action": { "type": "string", "description": "人读动作摘要（如 execute Write）" },
          "display": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/ToolInputDisplay" },
          "reason": { "type": "string", "description": "触发问询的策略原因（如 plan-review / goal-start）" },
          "approval_rule": { "type": "string", "description": "本次调用被门控的审批刻面（如 Write(/path)），always-allow 升级的记忆键" },
          "agent_id": { "type": "string", "description": "发起成员的完整 agent id（agent-<uuid>），仅成员发起时携带" }
        },
        "additionalProperties": false
      }
    }
  }
}
```

> **命名方言（设计债，2026-09-16 收编）**：`approval.ask` 参数为 snake_case，事件投影为 camelCase（`requestId`/`toolName`），两套拼写并存是迁移期实现现状，本版以现状为准；命名统一列为后续提案。`summary` 字段由 `action` + `display` 取代（结构化预览优于扁平摘要）。

应答（成功）＝ JSON-RPC Response，`result`：

```json
{
  "type": "object",
  "required": ["approved"],
  "properties": {
    "approved": { "type": "boolean" },
    "reason": { "type": "string", "maxLength": 500 },
    "remember": { "type": "boolean", "default": false, "description": "true = 记住该决策（写入 session 级审批缓存）" }
  }
}
```

应答（拒绝语义统一走 `result.approved = false`；`error` 仅用于协议层失败）。

**超时与降级**：
- 默认超时 60s（`approval.requested.expiresAt` 可见，可配置）。超时 → 自动 `approved: false`，发 `approval.expired`。
- 并发语义：同一 `requestId` 到达的多个裁决（本连接响应 + 其他连接 `approval.respond`）以**先到为准**，后到返回 `-32007`（approval 已裁决）。
- 客户端白名单未含 `approval` 前缀时：服务端不发 `approval.ask`，只发 `approval.requested` 事件；客户端用正向 RPC `approval.respond`（参数与上文 `result` 相同，外加 `requestId`）回调。

**`approval.respond`（正向 RPC，降级/多渠道入口）**：

```json
{
  "approval.respond": {
    "params": {
      "type": "object",
      "required": ["sessionId", "requestId", "approved"],
      "properties": {
        "sessionId": { "type": "string" },
        "requestId": { "type": "string" },
        "approved": { "type": "boolean" },
        "reason": { "type": "string", "maxLength": 500 },
        "remember": { "type": "boolean", "default": false }
      }
    },
    "result": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/Ok" }
  }
}
```

错误：`-32007` approval 不存在/已裁决 · `-32000` 会话不存在。兼容别名：`approval.resolve`（现网 IPC/HTTP 方法，参数 `approved` 同义；迁移期保留）。

### 8.2 question.ask → question.respond

引擎向用户提问（歧义澄清、偏好确认）。与 approval 流程同构。

```json
{
  "question.ask": {
    "x-role": "server → client request",
    "type": "object",
    "required": ["jsonrpc", "id", "method", "params"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string" },
      "method": { "const": "question.ask" },
      "params": {
        "type": "object",
        "required": ["sessionId", "requestId", "question"],
        "properties": {
          "sessionId": { "type": "string" },
          "requestId": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/RequestId" },
          "turnId": { "type": ["string", "null"] },
          "question": { "type": "string" },
          "choices": { "type": "array", "items": { "type": "string" } }
        }
      }
    }
  }
}
```

应答 `result`（成功）：

```json
{
  "type": "object",
  "properties": {
    "answer": { "type": "string", "description": "用户回答；choices 场景应回选项原文" },
    "dismissed": { "type": "boolean", "description": "true = 跳过本题（引擎自行处理缺省策略）" }
  }
}
```

超时：默认 120s；超时 → 按 `dismissed` 处理，发 `question.dismissed(reason=timeout)`。**`question.respond` 正向 RPC** 与 `approval.respond` 同构（参数 `{sessionId, requestId, answer?, dismissed?}`）；兼容别名 `question.answer`（现网方法）。错误：`-32008` question 不存在/已应答。

### 8.3 fs 文件代理（ACP 场景）

ACP 宿主（IDE）持有文件系统权限与路径语义；引擎需要读写文件时通过反向 RPC 请宿主代劳。**仅在客户端声明 `fs` 前缀时启用**；否则引擎直接走本地文件系统（Web/桌面/TUI 默认）。

```json
{
  "fs.readTextFile": {
    "x-role": "server → client request",
    "type": "object",
    "required": ["jsonrpc", "id", "method", "params"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string" },
      "method": { "const": "fs.readTextFile" },
      "params": {
        "type": "object",
        "required": ["path"],
        "properties": {
          "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
          "encoding": { "type": "string", "enum": ["utf-8", "base64"], "default": "utf-8" }
        }
      }
    }
  }
}
```

对应正向 RPC（宿主 → 引擎，携带结果）：`fs.readTextFile.response`；应答与反向请求的 JSON-RPC Response 二选一（推荐用 Response 回显 id，`fs.readTextFile.response` 仅用于异步宿主框架）。

```json
{
  "fs.readTextFile.response": {
    "x-role": "client response (success)",
    "type": "object",
    "required": ["jsonrpc", "id", "result"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string", "description": "回显 fs.readTextFile 的 id" },
      "result": {
        "type": "object",
        "required": ["content", "encoding"],
        "properties": {
          "content": { "type": "string" },
          "encoding": { "type": "string" },
          "size": { "type": "integer" }
        }
      }
    }
  }
}
```

```json
{
  "fs.writeTextFile": {
    "x-role": "server → client request",
    "type": "object",
    "required": ["jsonrpc", "id", "method", "params"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string" },
      "method": { "const": "fs.writeTextFile" },
      "params": {
        "type": "object",
        "required": ["path", "content"],
        "properties": {
          "path": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
          "content": { "type": "string" },
          "encoding": { "type": "string", "enum": ["utf-8", "base64"], "default": "utf-8" },
          "overwrite": { "type": "boolean", "default": true }
        }
      }
    }
  }
}
```

应答 `result`：`{ "bytesWritten": 123 }`；错误：`-32020` 文件不存在 · `-32022` 路径被宿主拒绝 · `-32003` 宿主权限不足。

**超时**：单次 10s。超时后引擎将该次写入/读取视为失败（工具结果 `isError: true`），并向宿主发 `warning` 事件；宿主未响应期间来自引擎的并发 fs 请求上限 16 个，超限返回 `-32004`。

### 8.4 terminal.create（宿主持有 PTY 模式，ACP）

`terminalMode=host` 时，引擎不创建 PTY；工具/任务需要终端时反向请求宿主分配。宿主随后通过事件/通知回报终端状态。

```json
{
  "terminal.create": {
    "x-role": "server → client request",
    "type": "object",
    "required": ["jsonrpc", "id", "method", "params"],
    "properties": {
      "jsonrpc": { "const": "2.0" },
      "id": { "type": "string" },
      "method": { "const": "terminal.create" },
      "params": {
        "type": "object",
        "required": ["cwd", "cols", "rows"],
        "properties": {
          "sessionId": { "type": "string" },
          "cwd": { "$ref": "https://mirri.dev/protocol/v1/defs#/$defs/PosixPath" },
          "env": { "type": "object" },
          "cols": { "type": "integer" },
          "rows": { "type": "integer" },
          "command": { "type": ["string", "null"], "description": "要运行的命令；null = 交互 shell" }
        }
      }
    }
  }
}
```

应答 `result`：`{ "terminalId": "..." }`。此后：

| 帧 | 方向 | 形状 | 说明 |
|----|------|------|------|
| `terminal.output` | 宿主 → 引擎（通知） | `{terminalId, data}` | PTY 输出块 |
| `terminal.waitForExit` | 宿主 → 引擎（通知） | `{terminalId, exitCode?, error?}` | 进程退出；引擎据此完成挂起的 shell 任务 |
| `terminal.kill` | 引擎 → 宿主（请求） | `{terminalId, signal?}` → `{ok}` | 引擎要求终止（对应 `terminal.close` 正向语义的 host 版） |
| `terminal.resize` | 引擎 → 宿主（通知） | `{terminalId, cols, rows}` | 尺寸变更（宿主尽力而为） |

**超时**：`terminal.create` 30s（超时 → 引擎按「终端不可用」失败并降级为管道式 shell）；`terminal.kill` 不设超时（幂等，宿主可重复执行）。宿主异常退出（进程死亡）时，引擎把所有未回收终端标记为已退出并完成挂起任务。

### 8.5 反向 RPC 汇总

| 反向方法 | 应答方法 | 超时 | 降级路径（不在白名单时） | 错误码 |
|----------|---------|------|-------------------------|--------|
| `approval.ask` | JSON-RPC Response / `approval.respond` | 60s | 事件 `approval.requested` + 正向 `approval.respond` | `-32007` |
| `question.ask` | JSON-RPC Response / `question.respond` | 120s | 事件 `question.requested` + 正向 `question.respond` | `-32008` |
| `fs.readTextFile` | Response / `fs.readTextFile.response` | 10s | 引擎本地文件系统 | `-32020` `-32022` `-32003` |
| `fs.writeTextFile` | Response / `fs.writeTextFile.response` | 10s | 引擎本地文件系统 | `-32020` `-32022` `-32003` |
| `terminal.create` | Response | 30s | `terminalMode=engine` 正向创建 | `-32013` `-32024` |
| `terminal.kill` | Response | 无（幂等） | — | `-32013` |

---

## 9. Cursor 同步

### 9.1 模型

- **事件日志**：每个会话一份持久化事件日志（wire log，续写式）。durable 事件按产生顺序获得本会话的 `seq`（从 1 起，单调递增）。
- **epoch**：日志代次标识（服务端启动时生成、日志轮转/重建时更换）。`epoch` 附着在每条 durable 事件上（或由客户端从游标/快照获得）。
- **游标**：`{seq, epoch}`。客户端按会话记录「已消费到 seq N」。
- **volatile 事件不参与**：不占 seq、不入日志、重连不可重放（§7.0）。

```
日志:  seq 1      2      3      4      5       6
       turn.started  assistant.delta(volatile, 无 seq)
       turn.started  tool.call.started  tool.result  turn.ended
       客户端游标 {seq: 3, epoch: "A"} → 重放 seq 4..6
```

### 9.2 重连追平

1. 客户端重连，在 `client_hello.params.cursors` 提交各会话游标（§5.3）。
2. 服务端逐会话处理：
   - 游标 `epoch` 与服务端一致且 `seq` 可达 → 在 `client_hello` 响应中**重放** `seq+1` 起的 durable 事件（`replayedEvents`；量大时可改用「先 resync 后 snapshot」，由服务端按 `maxEventBufferSize` 权衡）。
   - 游标 `epoch` 不一致或 `seq` 落后超过缓冲 → **不重放**，该会话列入 `resyncedSessions`，客户端对其执行 §9.4。
3. 重放期间新产生的事件按序尾随（同一条连接内顺序保证）。

### 9.3 resync_required

服务端在**连接存活期间**无法追平时，发送 `resync_required` 通知（会话级）：

```json
{
  "$id": "https://mirri.dev/protocol/v1/resync-required",
  "type": "object",
  "required": ["jsonrpc", "method", "params"],
  "properties": {
    "jsonrpc": { "const": "2.0" },
    "method": { "const": "resync_required" },
    "params": {
      "type": "object",
      "required": ["sessionId", "reason", "currentSeq"],
      "properties": {
        "sessionId": { "type": "string" },
        "reason": {
          "type": "string",
          "enum": ["buffer_overflow", "session_recreated", "epoch_changed"]
        },
        "currentSeq": { "type": "integer", "description": "服务端当前 seq，客户端可直接采用" },
        "epoch": { "type": "string", "description": "当前 epoch；与客户端游标不同即需 resync" }
      }
    }
  }
}
```

| reason | 触发条件 | 客户端动作 |
|--------|---------|-----------|
| `buffer_overflow` | 客户端落后超过 `maxEventBufferSize`（订阅时 cursor 已被逐出；或连接存活期间 live 转发器 Lagged 丢弃了中间事件） | 调 `session.snapshot` 重建，采用 `{currentSeq, epoch}` 为新游标 |
| `session_recreated` | 会话被删除后以相同 id 重建（日志截断） | 同上；同时清空旧会话 UI 状态 |
| `epoch_changed` | 服务端重启/日志轮转，且无法按旧 epoch 提供重放 | 同上 |

处理流程（客户端）：

```
收到 resync_required(sessionId, reason, currentSeq, epoch)
  ├─ 调 session.snapshot {sessionId} ──▶ 重建 messages/turns/agents/审批等
  ├─ 丢弃该会话在「旧游标之后、snapshot 之前」收到的所有事件（含 volatile）
  ├─ 设置本地游标 = snapshot.cursor（= {currentSeq, epoch}）
  └─ 恢复实时：此后 durable 事件按 seq 续接（断点校验：首个 seq ≠ currentSeq+1 时重新 resync）
```

实现锚点（2026-10-10）：

- 订阅路径（cursor 逐出 / epoch 不匹配）：`session.subscribe` 处理中
  `cursor_replay_bypassed` 判定为真时，在 ack 后发送 reason 为
  `epoch_changed`（epoch 不匹配）或 `buffer_overflow`（环形缓冲逐出）的
  `resync_required`。判定精确语义：cursor 落后于日志尾部 `tail_seq` 且
  重放留下间隙——环形缓冲已在 cursor 与重放窗口前端之间逐出事件（前端
  > cursor.seq + 1），或重放窗口为空（epoch 不匹配抑制了全部重放）。判定
  显式忽略 `seq: 0` 游标（fresh-sync 标记，web 客户端默认），该游标不视为
  错过事件的声明。
- live 路径：连接存活期间该会话的 live 转发器 `RecvError::Lagged` 时，
  发送 reason 为 `buffer_overflow` 的 `resync_required`。
- 日志任务路径：会话 EventBus → 持久日志的转发任务自身 Lagged 时，事件
  未落日志——此时将 `next_seq` 前进被丢数量，制造可检测的 seq 间隙。注意
  skip 后 `tail_seq()`（最后 APPENDED 的 seq）可能低于 `next_seq`；位于
  洞下方的 cursor 会在洞后 seq 处恢复正常续推——服务端仍跨洞重放，检测到
  丢失的是客户端「收到的事件 seq 严格递增」的连续性校验（触发快照重建）。
  该路径不发 `resync_required`（任务不持有连接）。
- 载荷兼容：`params` 除规格字段（`sessionId`/`reason`/`currentSeq`/
  `epoch`）外额外携带 `seq`（= `currentSeq`），供 web 客户端直接事件分发
  路径（`extractDirectEvent` 读取信封 `seq`）消费。

### 9.4 快照重建

`session.snapshot`（§6.1）返回该会话的权威状态 + 权威游标。规格要求：

- 快照的 `cursor` 必须等于调用瞬间的日志尾部 `{seq, epoch}`；客户端采用后，下一条 durable 事件的 `seq` 必须是 `cursor.seq + 1`，否则重复 resync。**竞态消除**：`session.subscribe` 支持可选 `cursor: {seq, epoch}` 参数（§6.0）——客户端把快照返回的游标随订阅一起提交，服务端在同一锁/事务内**原子地**以 `cursor.seq + 1` 建立订阅起点并续推，杜绝「快照之后、订阅之前」事件丢失窗口。
- 快照最大 8 MiB（超出按 `maxMessages` 等参数裁剪，客户端可分批拉取历史）。
- 快照只含 durable 状态；volatile 状态（流式 delta）不恢复，客户端 UI 以「正在流式输出」占位即可。

### 9.5 事件顺序保证

- 同一会话：durable 事件按 `seq` 严格递增发送（单连接 FIFO）。
- 跨会话：无总序要求；客户端按 sessionId 分桶。
- 批处理（数组帧）内的事件顺序 = 逻辑顺序；批内混入 volatile 事件时，volatile 事件的位置语义是「介于前后两个 durable 事件之间」，客户端依此渲染。
- serializable 关系：`turn.started(seq=5)` 必先于同 turn 的 `tool.call.started(seq=6)`、`turn.ended(seq=9)`；客户端不得做跨事件重排。

---

## 10. 错误处理

### 10.1 错误码总表

错误码按域分组；`domain` 列对应各方法所属领域（§6）。协议层错误（`-32700..-32603`）与 JSON-RPC 2.0 规范对齐，对所有方法通用。

| code | domain | 语义 |
|------|:------|------|
| **JSON-RPC / 协议层** | | |
| `-32700` | rpc | Parse error — 请求 JSON 无法解析。`data.details` |
| `-32600` | rpc | Invalid Request — 结构非法（缺 method/id 等） |
| `-32601` | rpc | Method not found — 未知方法/事件名 |
| `-32602` | rpc | Invalid params — 参数校验失败。`data.details`（字段级错误） |
| `-32603` | rpc | Internal error — 服务端内部错误。`data.errorId`（UUID，必带） |
| **Session** | | |
| `-32000` | session | Session not found — 会话不存在 |
| **Agent** | | |
| `-32001` | agent | Agent not found — agent 不存在 |
| **认证 / 授权 / 限流** | | |
| `-32002` | auth | Unauthorized — 未认证/凭据无效。`data.retryAfterMs?` |
| `-32003` | auth | Permission denied — 已认证但策略拒绝（含审批未过、路径越界策略拒绝、只读工作区） |
| `-32004` | auth | Rate limited — 限流。`data.retryAfterMs`（必带） |
| **Turn** | | |
| `-32005` | turn | Turn timeout — turn 超时（仅 `message.send` wait 模式 / 反向 RPC 超时） |
| **Tool** | | |
| `-32006` | tool | Tool not found — 工具不存在 |
| **Approval / Question** | | |
| `-32007` | approval | Approval not found or already resolved — 审批不存在/已裁决 |
| `-32008` | question | Question not found or already answered — 提问不存在/已应答 |
| **Workspace** | | |
| `-32009` | workspace | Workspace error — 工作区不存在/未创建/损坏 |
| **Model / Provider** | | |
| `-32010` | model | Provider not found — 模型 provider 不存在 |
| `-32011` | model | Model not found — 模型不存在 |
| **Config** | | |
| `-32012` | config | Config error — 配置校验/持久化失败。`data.details` |
| **Terminal** | | |
| `-32013` | terminal | Terminal not found — 终端不存在 |
| **Task** | | |
| `-32014` | task | Task not found — 任务不存在 |
| **Goal** | | |
| `-32015` | goal | Goal not found — goal 不存在 |
| **Plan** | | |
| `-32016` | plan | Plan not found / inactive — 无活动计划 |
| **Cron** | | |
| `-32017` | cron | Cron job not found — cron 任务不存在 |
| **Background** | | |
| `-32018` | background | Background task not found — 后台任务不存在 |
| **Plugin** | | |
| `-32019` | plugin | Plugin not found — 插件不存在 |
| **File** | | |
| `-32020` | file | File not found — 文件/目录不存在 |
| `-32021` | file | File too large — 超过下载/读取/上传上限 |
| `-32022` | file | Path forbidden — 路径越界/被安全策略拒绝 |
| **Sync（cursor）** | | |
| `-32023` | sync | Resync required — 游标失效，须先 `session.snapshot`。`data.cursor` |
| **State** | | |
| `-32024` | state | Invalid state — 状态机非法操作/资源状态不支持（归档损坏、turn 已结束、任务已终态等）。`data.details?` |
| **Contract / Connection** | | |
| `-32025` | contract | Contract violation — 宿主/客户端违反协议（如应答了未知反向请求）。`data.details` |
| `-32026` | contract | Connection mismatch — 跨连接操作被拒（如用 B 连接的请求 id 应答 A 连接的审批） |
| `-32027` | contract | Request timeout — 泛化请求超时 |

### 10.2 错误对象构造规则

```json
{ "jsonrpc": "2.0", "id": "r-1", "error": { "code": -32004, "message": "rate limited", "data": { "retryAfterMs": 5000, "domainCode": "rate_limit" } } }
```

- `message`：人类可读，≤512 字符；必要时携带上下文（如 `session 'x' has no agents yet; ...`，沿用现有风格）。
- `data` 约定字段（全部可选，camelCase）：`errorId`（-32603 必带）、`domainCode`（引擎领域码，如 `provider_error`）、`retryAfterMs`（-32004 必带）、`details`（字段级/结构化详情）。
- **HTTP 状态码映射**（仅迁移期 REST 兼容层使用）：`-32601/-32000/-32001/-32006/-32009/-32010/-32011/-32013/-32014/-32015/-32016/-32017/-32018/-32019/-32020` → 404；`-32602/-32023/-32024` → 400；`-32002` → 401；`-32003` → 403；`-32004` → 429；`-32700/-32600/-32603/-32005/-32021/-32022/-32025/-32026/-32027` → 500。
- **重试建议**：`-32004` 依 `retryAfterMs`；`-32005` 可重试（幂等性由调用方评估，`message.send` 重试会产生第二条消息——`mode=fire`/`wait` 都应在结果中带 `turnId` 供去重）；`-32024` 不建议重试；`-32023` 必须先 resync。

### 10.3 错误事件

- 应答类错误：永远走 Response 帧，不额外发事件。
- 异步错误（无请求可挂）：发 `error` / `warning` 通知（§7.12）。

---

## 11. REST 保留端点

统一协议上线后，REST 仅保留 3 个运维端点（**无业务能力**）+ 浏览器能力例外端点（**OAuth redirect 回调、文件上传/下载**——浏览器导航 / multipart 无法经 WS 表达）：

| 端点 | 方法 | 认证 | 响应 | 语义 |
|------|------|:--:|------|------|
| `/api/v1/healthz` | GET | 无 | `200 {"status":"ok","version":"0.28.0"}` | 存活探针（探活不需要 WS 握手开销） |
| `/api/v1/meta` | GET | 无 | `200 {serverId, version, startedAt, protocolVersion, wsUrl}` | 服务信息；客户端据此发现 WS 端点 `wsUrl` |
| `/api/v1/shutdown` | POST | 见 §13.4 | `200 {"ok":true}` | 优雅停机（仅 loopback + token） |

约束：

- 三个运维端点**不再使用** `{code, msg, data, request_id}` 信封，直接返回上述纯 JSON；`/api/v1/healthz` 在未就绪时返回 `503`（启动中）。
- `/meta` 的 `wsUrl` 使客户端可以纯 REST 引导：`GET /meta` → 取 `wsUrl` → WS 握手 → 全业务。
- **浏览器能力例外端点**仅限浏览器必需场景：OAuth redirect 回调（`auth.login` 顶层导航回跳，返回码换取 token 的浏览器机理由此成立）、`file.download` / `file.upload` 的浏览器直传（multipart/form-data 大文件）。这些端点不承载完整业务能力，其余业务一律以 WS RPC 为准。
- 迁移期旧 REST 业务端点保留（§12），但全部返回 `warning` 头 `X-Mirri-Deprecated: use-ws`，并在 `msg` 中提示。

---

### 12. 迁移策略

目标：**零中断**地从 3 传输碎片协议切换到单 WS JSON-RPC，旧接口只增不减地淡出。

> 阶段化是设计约束（每阶段独立可交付可回滚）；**执行状态统一见 [ROADMAP.md](ROADMAP.md)**，本节不维护进度标记。执行路线与本文的一处差异作为裁决保留：方法目录不走"§6 全目录"而走 **v2 契约 parity**（以 TS v2 契约为基准的双后端对拍，contract-e2e fixture），以注册表为事实源。

### 12.1 Phase 1 — 双栈共存（Rust server 增加 WS JSON-RPC）

- 在现有 14 条 REST + 旧 WS（`/api/v1/ws`）+ IPC 之外，新增 `GET /api/v1/rpc`（WS，统一协议）。
- 现有行为**完全不变**：REST 信封、旧 WS 帧、IPC 线格式均保持（向后兼容，现有客户端零改动）。
- 同期的增量：
  - `RpcRegistry` 扩充到 §6 全目录（120 个方法）；`message.send` 改为 `fire` 语义（新增 `turnId` 字段，旧字段 `turn` 在 `wait` 模式保留）。
  - 事件总线接入统一协议：`SessionEvent` bus 适配为 durable 事件日志（补 `seq`/`epoch`），volatile 事件由 turn 引擎直接发。
  - 反向 RPC 支持 approval/question（替代 HTTP 端点路径）。
- 验收标志：`rust/crates/mirri-server/tests/ws_tests.rs` 新增统一协议全套；旧 WS 测试依旧全绿。

### 12.2 Phase 2 — Web 客户端切换（见 [plans/02-phase2-web-migration.md](plans/02-phase2-web-migration.md)）

- Web（`apps/mirri-web` + `packages/kap-server` 适配层）改用 `/api/v1/rpc` 单连接：TCP 断开 REST 鉴权之外的业务请求，删除双连接关联逻辑。
- kap-server 的 `{seq, epoch}` cursor 语义**原样迁移**（协议设计即取自其 v1 实现）；`resync_required` 的三种 reason 与现有枚举一致。
- 既有 REST 业务端点标记 deprecated（`X-Mirri-Deprecated` 头 + 日志），**不删除**。
- 验收标志：Web 全功能跑在 WS JSON-RPC 上；REST 业务端点流量归零。

### 12.3 Phase 3 — TUI 与 ACP 迁移（见 [plans/03-phase3-tui-acp-migration.md](plans/03-phase3-tui-acp-migration.md)）

- TUI：`createRPC()` 内存调用替换为 FFI 三函数（§3.3），TUI 的 120 个方法映射到 §6 目录（多数可直接对准；差异部分在适配层完成）。
- ACP：`acp-server` 换用 Rust binary stdio transport（§3.2）；`fs.readTextFile`/`fs.writeTextFile`/`terminal.create` 走反向 RPC（§8.3/§8.4），不再依赖 TS `Klient`。ACP SDK conformance 测试为验收门槛。
- Unix socket IPC：保留为 stdio 的等价别名（部署形态由上层决定）；线格式已是 JSON-RPC 2.0，仅需补事件推送（IPC 连接自动获得「默认订阅全部会话」语义，或要求显式 `session.subscribe`，二选一在实现时定，建议后者）。
- 验收标志：TUI / ACP 全功能通过；`cargo test --workspace` 全绿。

### 12.4 Phase 4 — 旧接口退役（见 [plans/04-phase4-ts-retirement.md](plans/04-phase4-ts-retirement.md)）

- 删除 14 条 REST 业务路由、旧 `/api/v1/ws` 协议、`approval.resolve`/`question.answer` 别名。
- 保留：3 个运维端点 + 浏览器能力例外端点（OAuth redirect、文件上传/下载）+ `/api/v1/rpc` + stdio/FFI。
- TS 引擎（`agent-core-v2`/`kap-server`）整体退役；`packages/kap-server` 若保留则仅作为遗留 Web 壳。

### 12.5 兼容矩阵（每个 Phase 结束时）

| 客户端 | Phase 1 | Phase 2 | Phase 3 | Phase 4 |
|--------|:--:|:--:|:--:|:--:|
| Web（新） | WS JSON-RPC | WS JSON-RPC | WS JSON-RPC | WS JSON-RPC |
| Web（旧 REST 用法） | REST+旧 WS | deprecated | deprecated | ❌ |
| TUI | FFI（新）| FFI | FFI | FFI |
| ACP | stdio（TS） | stdio（TS） | stdio（Rust） | stdio（Rust） |
| IPC（本地工具） | JSON-RPC | JSON-RPC | JSON-RPC | 由 stdio 取代 |

---

## 13. 安全考虑

### 13.1 认证

- **主认证方式：Bearer token**。WebSocket 握手时通过 `Sec-WebSocket-Protocol` 子协议携带：`Sec-WebSocket-Protocol: mirri.bearer.<token>`。原因：浏览器 WebSocket API 无法设置 `Authorization` 头；子协议携带是浏览器下唯一干净的 token 通道（沿用 `packages/kap-server/src/transport/ws/bearerProtocol.ts` 既有实现）。
- 桌面/CLI 客户端（非浏览器）可用标准 `Authorization: Bearer <token>` 头 + 手选子协议 `mirri.v1`。
- **token 永不出现在 URL/query/frame body**；`/meta` 等公开端点不得回显 token。
- 服务端校验使用**常量时间比较**（constant-time compare，防时序侧信道），杜绝短路径短路比较；比较前先做长度检查。
- token 校验失败的握手：服务端直接关闭 WS（不回 `server_hello` 或回 `-32002` 错误的 `client_hello` 响应，然后关闭；两种行为皆可，但**不得**泄露 token 有效性以外的信息——对不存在 token 与错误 token 使用相同响应与相同延迟）。
- 会话/连接 token 轮换：`auth.login` 成功后服务端可下发新 token；旧 token 在宽限期（默认 5 分钟）内仍有效，宽限期后失效。

### 13.2 传输安全

- 本地（localhost）：允许明文 `ws://`；限制同 loopback 绑定（默认只绑 `127.0.0.1`/`::1`）。
- 远程：必须 `wss://`（TLS）；服务器配置提供 TLS 时，`/meta.wsUrl` 返回 `wss` scheme。
- 帧级：`maxMessageSize`（默认 16 MiB）超限即断开；`maxEventBufferSize` 防 event 洪峰撑爆连接缓冲（溢出转 `resync_required`，而非无限增长）。

### 13.3 连接与请求安全

- 握手限流：同一来源握手频率超限 → `-32004`；未完成 `client_hello` 的连接 10s 后强制关闭（沿用现有 `HANDSHAKE_TIMEOUT`）。
- 反向 RPC 白名单（§8）：客户端未声明 `approval`/`question`/`fs`/`terminal` 前缀时，服务端自动降级，杜绝「引擎反向驱动宿主做未授权操作」。
- 跨连接隔离：审批/提问/终端的 `requestId` 绑定发起连接；其他连接用 `approval.respond` 等正向方法裁决时校验 `sessionId` 归属（`-32026`）。
- 批量帧限制：数组帧 ≤256 条、仅限服务端→客户端，防止放大攻击。
- 并发限制：每个客户端对同一会话的 `message.send` 并发上限 1（同 agent turn 串行），超额返回 `-32024` 或排队（实现时定，建议返回 `-32004`）。

### 13.4 特权操作（shutdown）

- `POST /api/v1/shutdown` 双保险：
  1. 未配置 token 时**仅接收 loopback 来源**（`ConnectInfo` 检查 sockaddr），远程一律 `403`；
  2. 配置了 token 时校验 Bearer，且**同时**要求 loopback（shutdown 永不向远程开放）。
- stdio/FFI 形态无 shutdown 端点；退出语义为宿主断开/调用 `process.shutdown` 反向方法（仅限本进程内）。

### 13.5 路径与文件安全

- 所有 `path` 类参数（file.*、workspace.create、session.export 的 outputPath、shell cwd）先过工作区路径校验（复用 `validate_workspace_path`：canonicalize 后必须落在允许根内，拒绝 `..` 逃逸、symlink 逃逸、设备文件）。
- 校验失败 → `-32022`，**绝不**静默截断/修正路径。
- `file.download`/`file.read` 输出上限（默认 16 MiB，`-32021`）；`file.upload` 入站上限同值。
- `session.export` 的 `outputPath` 只许落在导出根（`<home>/.mirri-code/`）内（沿用现有 issue 15K 修复）。

### 13.6 敏感信息

- `model.addProvider` 的 `apiKey` 仅入参，任何响应/事件/日志不回传；日志中 `argsSummary` 类字段对 key/token 打码。
- 快照与事件摘要（`contentSummary`）默认截断（≤120/500 字符），避免把大段敏感内容灌进事件流。
- Auth 域的用户信息只在 `auth.status`/`auth.login` 结果中返回。

### 13.7 审计

- 服务端对以下事件写审计日志（含 connectionId、客户端类型、sessionId）：认证成败、会话创建/关闭/导出/归档、`message.send`、`tool.call`、审批裁决、config.set、shutdown。
- 不写日志内容本体，只写摘要，降低敏感数据落盘面。

---

## 附录 A：与既有实现的对应关系（实现锚点）

统一协议元素与现有代码锚点的对应表见 [context.md](context.md) §5（参考类；协议本身不依赖任何具体实现位置，锚点表服务于迁移与审计）。

## 附录 B：开放问题（实现前需裁定）

1. IPC 连接的事件获得方式：隐式全订阅 vs 显式 `session.subscribe`（§12.3 建议后者）。
2. `message.send` 的 `wait` 模式在 Phase 4 是否删除（建议保留：CLI/脚本价值高）。
3. `session.snapshot` 与日志的竞态窗口：`session.subscribe` 的 `cursor` 参数（§6.0）已给出消除方案（§9.4）；实现验收点——服务端须保证「快照游标读取」与「订阅建立」在同一锁/事务内完成。
4. 事件日志的物理格式与轮转策略（`wire.jsonl` 单向兼容迁移，细节归 [05-crate-structure.md](05-crate-structure.md) / [plans/04-phase4-ts-retirement.md](plans/04-phase4-ts-retirement.md)）。
5. 批处理（数组帧）是否对 HTTP 之外的传输开放（当前仅 WS）。

## 附录 C：修订记录

### 2026-10-13 session.work_changed 事件与 SessionView 运行状态字段登记

§7.1 新增 `session.work_changed`（durable、全局扇出——§2.3 增补全局扇出例外条款）、session-view 增补运行状态四元组字段（busy/mainTurnActive/pendingInteraction/lastTurnReason，视图字段 camelCase；事件负载 snake_case，与参照引擎 v2 的 wire 拼写一致）。动机：新内核（rust）上线后网页刷新即丢侧边栏"运行中"标记——列表读面与状态事件两侧在新内核均缺失，进行中的会话无从辨认。需求与设计裁决见会话管理域 [SDR-013](../session-management-requirements.md) / [ADR-009](../session-management-architecture.md)；参照实现为 TS v2 的 work facts（列表行四元组 + durable 全局广播）。

### 2026-10-10 resync_required 三触发点落地

§9.3 的 `resync_required` 通知此前仅有客户端实现（v2 TS 引擎行为的遗
留），rust server 从未发送——事件在「订阅 cursor 逐出 / live 转发器
Lagged / 日志任务 bus Lagged」三个点静默丢失，客户端页面会永久停留在
turn active（见 §9.3 实现锚点）。本次将三个触发点全部落地，并在日志任
务 Lagged 路径以 seq 间隙替代通知（该任务不持有连接）。动机与排查记录
见 2026-10-10 会话 wire 日志分析（fork 会话 turn.ended 落盘但页面未收
到）。

### 2026-09-08 MCP 状态事件命名统一

MCP 状态事件名由本目录提案的 `event.mcp.serverChanged` 改回既有事件名 **`event.mcp.server.status`**（§6.8 / §7.10 / §7.14 同步）。动机与裁决记录见 MCP 域架构文档 [ADR-010](../mcp-architecture.md)：引擎已存在 session 级 `event.mcp.server.status` 事件投影（池级 fan-out），两个近似事件并存会使消费方与契约分裂；终态为统一名 + 负载扩展（pool/变更类别）+ 双订阅面（session 级 + app 级）。
