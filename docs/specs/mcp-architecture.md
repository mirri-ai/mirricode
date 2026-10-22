# MCP 域架构补齐设计（RFC）

> 状态：设计稿（待评审） · 2026-09-04 · 2026-10-21 增补：ADR-014（链路终点细化——全部 agent、派发与恢复两入口同权） ·
> 源起：rust-v2-method-parity 程序 B6 讨论中用户提出的架构问题（连接爆炸、配置管理面、启动延迟）。
> 本文不局限于 B6 计划，是对 MCP 域完整功能面的架构规划。
> 配套文档：[mcp-requirements.md](mcp-requirements.md)（需求侧事实源）· [mcp-context.md](mcp-context.md)（现状参考）· [mcp-ROADMAP.md](mcp-ROADMAP.md)（执行状态唯一家）。

## 1. Scope

**IN**：
- MCP 配置的分层读写模型（user 级 / workspace 级）与 wire 契约
- 连接架构：分层池化（app 级 user 池 + workspace 池 + 插件池）、懒连接、工具缓存
- 热更新：配置变更 → 连接池增量操作 → 状态事件
- 状态观测与连接测试（无 workspace 上下文的管理面呈现）
- OAuth 授权流补齐（needs-auth 状态的服务器如何完成授权）
- rust 与 TS 两侧的落地映射与分阶段交付

**OUT**：
- MCP 协议本身的修改（连接复用标准化等，等生态）
- 跨进程连接守护进程（daemon/gateway）——列为 followup，本文只预留架构位
- 恢复 v1 退役 RPC（`mcp.enable/disable/enableTool/disableTool/toggleState`、`mcp.global.*` ×12）

## 2. Goals & Non-Goals

**Goals**：
1. 配置管理 UI（desktop/web/TUI）可以在**任何上下文**（无 workspace、无 session）管理 user 级 MCP 配置；进入 workspace 后可管理 project 级配置
2. 同一进程内，user 级 server 只连一次（跨 workspace 共享）
3. workspace 启动与 user 级 MCP 连接建立完全解耦（不依赖任何推迟式缓解）
4. 配置变更后连接**立即**按需调整（enable 即连、disable 即断、配置变化即重连），并有状态事件推送
5. 工具级开关按作用域生效（user 层与 project 层各自可设）
6. 两侧引擎（rust / agent-core-v2）行为一致，klient 契约是唯一形状源

**Non-Goals**：
- 不做"运行时临时开关不落盘"的 v1 模式——一切开关都是配置（可审计、可同步）
- 不做多租户/团队配置同步
- 不承诺跨进程共享（v1 阶段）

## 3. Context（动机摘要）

MCP 域的核心问题（现状详盘见 [mcp-context.md](mcp-context.md)）：连接随 workspace 重复（同进程内 user 层 server 被拉起多份）、启动被 MCP 阻塞（缓解只是推迟等待而非解耦）、配置读写面空白（三级配置发现已存在但 wire 面无读写）、运行时开关随 v1 退役后缺配置驱动的热更新、OAuth 授权闭环缺 wire 面。

### 4. Proposal

### 4.1 配置作用域模型（数据层）

配置是唯一事实源（配置驱动哲学，v1 退役的延续）。新增 wire 契约（klient 新增 `contract/global/mcp.ts`——同时补上 mcp.list 等既有方法的契约文件，见 [mcp-context.md](mcp-context.md) §3）：

```
mcp.config.list(scope?: 'user' | 'workspace') → 层视图
  [{ name, source: 'user'|'projectRoot'|'project'|'plugin', revision, config, effective:
     { enabled, enabledTools?, disabledTools?, timeouts, transport } }]
  — 不带 scope = 调用上下文的合并视图（每 server 标注来源层；plugin 条目另含 runtimeName）
  — scope='user' = 仅 user 层原文；scope='workspace' = project-root+project 合并
  — config 为原文（`${VAR}` 引用保留）；解析值不出现在默认响应（裁决 A5）
  — 每层带解析状态（ok | failed+原因）：坏层条目缺省呈现、诊断可见，整个请求不因坏层失败（ADR-007）

mcp.config.set(scope, name, config, expectedRevision?)    → noResult  — scope: 'user'|'workspace'
mcp.config.remove(scope, name, expectedRevision?)         → noResult
mcp.config.setToolFilter(scope, name, filter, expectedRevision?) → noResult — 工具级开关的统一写入口：
  { enabledTools?, disabledTools? }
  — scope 缺省 = 写目标 server 定义所在的最高优先级**可写**层（现状语义）
  — scope='workspace' 且项目层未定义该名字 = 在项目层创建**过滤覆盖条目**（仅含过滤字段，连接定义仍取被遮蔽层——裁决 A7）
  — 目标 server 为插件提供（运行时名 plugin-<id>:<name>）→ 写插件用户状态（裁决 A1 例外）
  — 任何 scope 下 project-root 层永不机器写入（裁决 A2 的统一约束——ADR-006）
  — expectedRevision 携带读取时的层版本；不匹配返回明确的冲突错误（ADR-005，对齐域 N3/US1.2）

mcp.config.preview(scope?, name, config?) → 解析视图 — 显式的「显示真实值」预览：返回环境变量展开后的完整配置。
                                               裁决 A5 唯一例外的出口（D-S3）：不缓存、不进事件与日志（ADR-005）
mcp.config.test(scope, name, config?) → 连接测试  — 按展开后的值实测（不落盘的 config 直测，或已落盘的）；
                                               因未定义变量失败时结果诊断含变量名
```

工具级开关 = 过滤字段的细粒度写：普通层 server 走 `setToolFilter` 改条目内的 `enabledTools`/`disabledTools`；插件来源 server 走 `setToolFilter` 改插件用户状态（见 A1）。

**服务器级启停的写路径**：`enabled` 是配置条目字段而非独立开关——启停经 `mcp.config.set` 读改写整条（携带 `expectedRevision` 保并发安全）；不设独立开关方法（v1 的 `mcp.enable/disable` 已随 v1 退役，不恢复——ADR-009）。

**裁决 A7（分层合并按字段族）**：合并分两个字段族——**连接族**（transport/command/env/超时/enabled）维持整条遮蔽（最高的完整条目胜出，现状不变）；**过滤族**（enabledTools/disabledTools）改为**显式才覆盖**：从低层到高层，仅当某层条目（完整条目或过滤覆盖条目）显式携带过滤字段时替换当前值，未携带则继承下层。理由：工具开关是使用侧偏好，project 层重定义 server 的连接不应静默清掉 user 层的过滤偏好；域需求 US2.3 的"项目层单项覆盖用户层开关"只有在过滤族可独立分层时才可表达。行为变更说明：现状的"整条替换（未携带即清空）"对过滤族而言是意外清空，本裁决将其收敛为显式覆盖（ADR-006）。

**裁决 A1（plugin 层读写边界）**：插件提供的 server 进独立命名空间池（见 4.2）。**连接定义只读**——manifest 的 mcpServers 条目（transport/command/env/凭据）是插件资产，`mcp.config.set/remove` 对插件来源拒绝；**例外：工具级过滤可写**——`mcp.config.setToolFilter` 对插件 server 写入插件用户状态（`installed.json` capability state 的 per-server 记录含 `{enabled, enabledTools?, disabledTools?}`），manifest 永不落用户改动。理由：连接定义随插件版本演进（用户改了会被升级冲掉）；工具开关是使用侧偏好，与 per-server 启用开关同存储同性质——插件资产不可变，用户偏好可写，两者存储分离。

**裁决 A2（写路径约束）**：workspace scope 写仅对 project 层落盘（project-root 层只读——它是 git 仓库共享资产，由用户手工/其它工具管理；视图展示但 wire 面不写）。避免程序静默改写 git 内文件。

**裁决 A5（配置值的环境变量语义；D-S3 修订 2026-09-08）**：配置**存储与默认 wire 呈现均为原文**（`${VAR}` / `${env:VAR}` 引用保留），**展开仅发生在连接建立/测试路径，及唯一的显式例外：配置页「显示真实值」预览请求**（裁决来源：mcp-settings-page-requirements.md D-S3）——预览结果须显式请求获得、不缓存、不进事件与日志；除该例外，解析值不出现在任何默认响应、事件与日志。未定义变量按既有兼容语义展开为空串，但加载诊断与连接测试失败必须携带变量名。展开范围对齐 TS 值级语义：仅字符串值（含插件 manifest 的 mcpServers——现状不展开，落地补齐）、对象键不展开、单遍不递归。理由：解析值即真实凭据，默认面（session 记录、桌面缓存、遥测）的泄漏面必须最小化；原文呈现让"配置里写了什么"与"环境里解析出什么"始终可分离审计（N9）；预览例外是"配置作者核对占位符引用"的产品裁决，以显式请求+不缓存收窄暴露面。

### 4.2 连接架构：三池模型（运行时层）

```
┌─ App 进程 ──────────────────────────────────────────────┐
│                                                          │
│  UserPool（app 级单例，常驻）                             │
│   ├─ server A ── stdio 子进程 ──┐                        │
│   └─ server B ── http 连接 ─────┤ 只连一次，全 workspace 复用 │
│                                  │                       │
│  WorkspacePool（per workspace）  │                       │
│   ├─ server C（project 层）──────┤ 各自独立生命周期          │
│   └─ server D（同名遮蔽 A 时：   │                        │
│      workspace 视图看不到 A，    │                        │
│      A 连接惰性保留/引用计数回收） │                        │
│                                                          │
│  PluginPool（per 插件生命周期）   │                       │
│   └─ server E                     │                       │
└──────────────────────────────────────────────────────────┘
```

- 连接键 `(pool, serverName)`；同名跨层 = 不同连接，视图层按 v2 合并优先级遮蔽
- **UserPool 预热**：app 启动即连（desktop 场景 workspace 打开零等待）
- **WorkspacePool**：workspace 打开时按 project 层配置连接；workspace 关闭即断
- **遮蔽回收**：project 层同名 user server 时，UserPool 该连接引用计数归零后回收（或保留——见风险 R3）

**裁决 A3（懒连接与缓存的适用层）→ 已被 ADR-013 修订（2026-10-12）**：原裁决为「UserPool 预热 + 常驻（不做懒）；WorkspacePool 懒连接（首次工具调用才连）+ `tools/list` 结果按 (server, serverVersion) 缓存，缓存带 TTL 上限（契约默认 10 分钟，防 server 静默更新——域 US3.4；双侧一致，ADR-009）」。经代码核验（连接已不阻塞启动、状态变化通知已全链落地）与提示缓存前缀稳定性考量，懒连接与清单缓存**均裁决不实施**——详见 ADR-013 与域 SDR-008。现状终态：服务启动后即在后台连接全部已启用服务器（两池同策略，不阻塞任何请求），工具清单在连接建立时清点一次、连接期间驻留内存。

**裁决 D6（Session Overlay——随会话的第四种来源）**：caller-supplied MCP server 配置（ACP `session/new` 携带的 mcpServers、未来 SDK 的 per-session 注入）构成独立于三池的第四种来源：**随 session 生灭**——不落盘（不进 4.1 的任何配置层）、不跨 workspace 复用（不进 UserPool）、不参与热更新事件链（4.3 的 diff 范围不含 overlay）。工具照常以 `server__tool` 注册进该 session 的工具面，session 关闭即注销。**遮蔽**：overlay 与三池任一层同名时，overlay 优先级最高（最内层 scope 胜出，与 user→project 遮蔽同向），被遮蔽池连接的回收沿用 R3 语义。TS 侧 ACP adapter 现有的 warn-and-drop 仅对 `type:'acp'` 条目收敛（本裁决生效后 warn 语义指向"overlay 已接管"）。已知缺口（followup）：rust ACP 路径目前不读启动参数中的 mcpServers，两侧 overlay 注入面以 TS 先行。

**跨进程（daemon）**：v1 不做。架构位预留：Pool 实现收窄为 trait（`McpPoolOps`），未来换 daemon 后端时上层数据流不变。

### 4.3 热更新（配置 → 连接的事件链）

```
mcp.config.set/remove
  → 配置落盘（原子写）
  → 变更 diff（add / remove / reconfig / field-change）
  → 池增量操作：
      add     → connect（受 enabled 控制）
      remove  → disconnect
      reconfig→ disconnect + connect（transport/凭据变化）
      field-change → 不重连，仅更新配置与工具注册（非连接绑定字段变更——工具过滤/超时等）
  → event.mcp.server.status {name, pool, status, 变更类别} 通知订阅方（session 级 + app 级）
```

- 状态事件同时服务：管理 UI 实时刷新、`mcp.list` 的一致性、auth 完成后的 reconnect 通知
- **事件是轻量通知而非负载推送**：负载只含名称/池/状态/变更类别；工具数、错误详情等由消费方收到事件后重取对应查询面（事件触发重取——ADR-009）
- **双订阅面**：session 级 fan-out 直达活跃会话（US4.4 的工具面终点）；**app 级订阅面**服务无会话上下文的消费方（设置页 PS1.3、域 US4.3）。事件名沿用既有 `event.mcp.server.status`（符合 `event.<type>` 惯例——ADR-010）
- **非重连性配置变更同样产生变更通知**：配置条目的任一持久化修改（如仅调整超时）即使不触发池增量操作、status 不变，也产生变更通知（含变更类别）——否则外部编辑后，其它客户端的字段级呈现会停留在旧值（域 US4.3 / 设置页 PS1.3 的完备性，ADR-008）
- **链路终点是活跃会话的工具面**：池增量操作完成后，执行中 agent 任务的下一步即可感知/调用新状态（域需求 US4.4，ADR-002）；该会话**全部 agent**（含子代理）的工具面均属终点——池状态变化的活跃 agent 广播与 agent 进入会话的装配时刻（派发/恢复）两个入口同权（ADR-014 细化）；在途调用的处置遵循域 US4.1 裁决
- **解析失败保守处理（确立为契约）**：任意来源读盘遇解析失败时保持当前连接与已加载状态；诊断经 `mcp.config.list` 的每层解析状态暴露，不进入任何池操作（ADR-007）
- **调用时强制校验（防御纵深）**：禁用态的拦截/放行在工具调用管线的校验环节执行——先于连接建立/复用，以调用时点的有效配置（按调用所属作用域判定）为准，不依赖工具清单快照的新鲜度（域需求 US4.5，ADR-003）；清单注册与调用校验是两层独立防线

**裁决 A6（对账内核与触发面）**：配置与运行时的一致性由**同一对账内核**（重读配置 → diff → 池增量操作）的**三个触发入口**保证，不允许出现第二套对账逻辑：① wire 写入（`mcp.config.set/remove/setToolFilter`）——按写入项做增量 diff；② 配置文件监听——页面外修改（手工编辑、git pull、其它进程写入）触发全量重读 + diff；③ 手动同步 `mcp.sync()`——与 ② 同款**幂等对账**，作为监听事件丢失、宿主形态无监听时的逃生门；无差异时零成本空操作，对账结果摘要（新增/移除/重连/重试计数）返回给调用方。裁决来源：mcp-settings-page-requirements.md D-S2。理由：文件系统事件是 best-effort（休眠唤醒、网络盘、事件合并均可能丢失）且监听仅存在于部分宿主形态，漂移需要自愈；手动入口若采用 v1 全杀重连式 reload 会推倒全部健康连接，扰动面 = 全部 server。

### 4.4 状态观测与连接测试

- `mcp.list`（已有）扩展：每项加 `pool: 'user'|'workspace'|'plugin'` 与 `source`（配置来源层）——向后兼容（新增字段）
- 新增 `mcp.status()`：**无 workspace 上下文**的全局视图（UserPool + 本上下文可达的 workspace 池）——这是"配置页面在 app 级打开时"的呈现面
- `mcp.config.test`（4.1）：管理 UI "测试连接"按钮的后端；timeout 遵循 `startupTimeoutMs`
- 新增 `mcp.tools(name)`：单 server 的工具清单（名称/描述/当前过滤状态）——配置页工具弹窗的数据源（裁决来源：mcp-settings-page-requirements.md D-S1）。连接中的 server 复用 4.2 的 tools/list 缓存（缓存命中不重复枚举）；未连接的返回明确的"未连接"语义而非空列表假象

### 4.5 OAuth 授权流补齐

401 → `needs-auth` 后的闭环（复用 v2 auth 域的形状模式，但不混用 provider OAuth 的 token 体系）。**双发起路径，同一 token 存储后端**（ADR-011）：

**① 会话内路径（合成工具）**：needs-auth 时向活跃 agent 会话注入合成工具 `mcp__<server>__authenticate`，在对话流内驱动浏览器授权（RFC 9728/8414/7591 发现 + PKCE + loopback callback）；令牌入存储后透明刷新、断连自动重连。

**② wire 路径（无会话场景，P4）**：设置页等无会话上下文的消费方发起：

```
mcp.auth.start(serverName)  → {authUrl, state}（server 的 OAuth authorize URL）
  → 用户浏览器完成授权 → callback（本地 loopback 或桌面协议注册）
mcp.auth.complete(serverName, {code|error}) → noResult
  → 令牌入共享 token 存储 → 池触发 reconnect → status 事件
```

- token 存储双侧磁盘布局统一（P4 强制——对齐域 US6.2/N4 的跨引擎兼容；现状不互通，见 [mcp-context.md](mcp-context.md) §4）
- **裁决 A4**：HTTP/SSE server 的 401 流优先；stdio server 无 OAuth（协议上 stdio 不产生 401），设计只覆盖远程 transport

### 4.6 两侧落地映射

| 层 | TS（agent-core-v2） | rust（mirri-*） |
|---|---|---|
| 配置读写 | workspaceMcpConfig 域 + 新 app 级 user-config 读写（user mcp.json 在无 workspace 上下文也要可写——现状 config-loader 以 cwd 为前提） | EMB `mcp_config_handlers` + `mirri-mcp` 配置持久化 |
| 池 | `McpConnectionManager` 拆三池（现有单 manager 的接口保留为聚合门面） | `McpConnectionManager` 同款拆分 + EMB 组合根持 UserPool |
| wire | klient `contract/global/mcp.ts`（新） | EMB routes + registry/TSV |
| 热更新 | manager 增量 op + 事件 | 同 |
| OAuth | 已有 service/store，补 wire（无会话场景） | 模块已落地（合成工具 + token 存储）；补 wire 面与令牌布局统一（ADR-011） |
| 缓存 | tools/list 缓存 per server | 同 |
| 环境变量展开 | 插件 manifest 补齐展开；加载诊断带变量名（US1.6） | 对齐 TS 值级语义（现状差异见 [mcp-context.md](mcp-context.md) §1） |

### 4.7 wire 契约汇总（新增 12 + 扩展 1）

```
mcp.config.list / mcp.config.set / mcp.config.remove / mcp.config.test / mcp.config.setToolFilter / mcp.config.preview
mcp.status
mcp.tools(name)
mcp.sync
mcp.auth.start / mcp.auth.complete（双路径之一：会话内合成工具已落地，wire 面服务无会话场景——ADR-011）
event.mcp.server.status（事件，命名对齐既有 session 事件，负载含 pool 与变更类别——ADR-010）
mcp.list（扩展 pool/source 字段）
```

均带 workspace 参数的方法遵循现有 session 上下文惯例（无 workspace 上下文 = app 级）；带显式目标工作区参数时可指向已打开的任一工作区，供配置页作用域选择器写入其 project 层（ADR-005）。

## 5. Trade-offs（被拒绝的选项）

| 选项 | 拒绝理由 |
|---|---|
| per-session 连接（v1 痕迹） | 连接/子进程随 session 爆炸；两侧现状已正确共享，不回头 |
| 单一全局池（不分层） | project 级 server 可能带仓库专属凭据/网络位置，跨 workspace 共享 = 泄漏面扩大；生命周期也无法对齐 workspace |
| 配置塞进 config.toml | `.mcp.json` 是行业标准（Claude Code/Cursor 兼容）、mcp.json 是既有用户资产；迁移成本 > 收益 |
| 恢复 v1 运行时开关 RPC | 与配置驱动哲学冲突（开关不落盘不可审计）；v1 退役是正确方向，缺的是热更新，把热更新补上而非回退 |
| 立即上 daemon（跨进程共享） | 为假想负载引入常驻进程与故障域；先用三池模型覆盖单进程（desktop 主场景），trait 预留换后端 |
| UserPool 也做懒连接 | user 层是跨 workspace 高频资产，预热一次全场景受益；懒反而每次首用都付冷启动成本 |
| mcp.auth 并入现有 auth 域 | token 体系不同（provider OAuth vs MCP server OAuth），磁盘布局与刷新语义都不同；形状模式复用，域保持独立 |
| 工具过滤写回插件 manifest | 插件升级即被冲掉；manifest 是插件资产（随插件分发/版本管理），用户偏好混入造成作者-用户-引擎三方写竞争 |
| 工具清单并入 `mcp.status`（不新增方法） | 状态查询被配置页之外的高频场景复用（agent 工具目录、桌面状态条），每次背着全量工具清单；独立 `mcp.tools(name)` 按需查询且直接吃 tools/list 缓存 |
| 手动同步采用 v1 全杀重连式 reload | 推倒全部健康连接，扰动面 = 全部 server；幂等 diff 对账（裁决 A6）无差异时零成本，且语义是 v1 的超集 |
| 插件工具开关留在 plugin 域 wire（如 `plugin.setMcpServerToolFilter`） | 配置页 UI 需按来源分流调用两个域，体验分裂；存储归插件域、wire 归 mcp 域本来就是本设计的分层（UI 心智模型 vs 资产归属），统一入口让 UI 无差别对待所有来源 |

## 6. Risks & Mitigations

| # | 风险 | 缓解 |
|---|---|---|
| R1 | 三池后工具名冲突（user 与 project 同名 server） | 工具 qualify 保持 `server__tool` 现状 + **视图层遮蔽**（project 覆盖 user，单一可见）；不用 scope 前缀污染工具名（对 LLM 不友好） |
| R2 | 热更新重连时正在执行的工具调用 | 池增量操作等待在途调用完成（`reconnectAndJoin` 模式扩展）或强制失败并报 `server_restarting`——裁决进实现阶段 |
| R3 | UserPool 遮蔽连接的回收时机（引用计数 vs 常驻） | 默认常驻（内存可预期），提供池级 `reap()` 显式回收；M×N 规模下常驻成本已比现状好 M 倍 |
| R4 | tools/list 缓存失效（server 更新但版本不变） | 缓存 TTL 上限（契约默认 10 分钟，ADR-009）+ `mcp.reconnect` 强制刷新作为逃生门 |
| R5 | user mcp.json 写入的并发（多 UI 同时编辑） | 原子写 + 版本号乐观锁（`config.set` 带 `expectedRevision`，冲突返回 -32001 类错误） |
| R6 | rust 与 TS 行为漂移 | 契约先落 klient（唯一形状源），TSV 增行沿用 parity 程序的 pin 机制 |
| R7 | desktop 场景 UserPool 预热拖慢 app 启动 | 预热异步化（app 主线程不等），workspace 打开时按 ready promise join |
| R8 | 插件升级/卸载时工具过滤偏好的生命周期 | 与 per-server enabled 同生命周期（capability state 随 installed.json 记录）：升级保留、卸载清理、重装重置；manifest 侧无用户痕迹，插件作者升级无感知 |
| R9 | 插件 manifest 补齐 env 展开改变既有行为 | 若现存插件的 mcpServers 配置里恰好写了 `${...}` 字面量且依赖其不展开，补齐后会解析——P1 时扫描生态内 manifest 的 `${` 出现率评估冲击；正常插件不受影响 |

## 7. Success Criteria

1. desktop 开 8 workspace + user 层 5 个 stdio server：子进程数 = 5
2. workspace 打开耗时与 user 层 MCP 数量解耦（p95 打开时间不随 user server 数增长）
3. 管理页（无 workspace 上下文）能完成 user 层增删改 + 工具级开关（含插件来源 server）+ 连接测试 + 授权闭环
4. `mcp.config.set` 后 ≤ 1s 内状态事件到达（同进程）
5. 跨端一致性：klient 契约 + rust/TS 实现通过现有 contract-e2e pin（新 fixture 条目）
6. 全量 `cargo test --workspace` / TS 套件绿（复用 parity 程序门禁标准）

## 8. Rollout（四阶段，每阶段独立可交付可评审）

> 各阶段范围为设计定档；**执行状态与开放项的唯一家在 [mcp-ROADMAP.md](mcp-ROADMAP.md)**——本节不随执行进度更新（ADR-009）。

| 阶段 | 内容 | 交付物 |
|---|---|---|
| P1 配置读写面 | klient 契约文件（含补齐 mcp.list 等既有方法契约）+ 双端 `mcp.config.list/set/remove/setToolFilter/preview`（含 revision 乐观锁、过滤族分层合并、解析分层诊断——ADR-005/006/007）+ 插件 manifest 环境变量展开补齐 + rust user mcp.json 无上下文读写 + 插件 capability state 工具过滤扩展 | 管理页可读写配置、可对任意来源 server 开关工具（热更新 P3 才落地——本阶段改完需重启生效，诚实降级） |
| P2 三池拆分 | UserPool 拆出 + EMB/SRV 组合根持有 + `mcp.status` + `mcp.config.test` + `mcp.tools(name)` | 连接不再跨 workspace 重复 |
| P3 热更新 | 变更 diff → 池增量 op → `event.mcp.server.status`（双订阅面——ADR-010）+ `mcp.sync`（手动对账入口，共用对账内核） | enable 即连、disable 即断 |
| P4 体验层 | ~~WorkspacePool 懒连接 + tools/list 缓存~~（ADR-013 裁决不实施）+ `mcp.auth.start/complete`（无会话场景）+ 令牌布局跨引擎统一 + `mcp.list` 扩展字段 | 授权闭环（启动解耦已由现状实现满足：连接不阻塞任何请求） |

每阶段照 parity 程序 SDD 协议（brief → 实现 → 评审含 mutation → 提交 → 账本）。

---

### 附：与 rust-v2-method-parity 程序的关系

- 本设计不阻塞 B6（v1 桩清理照做——v1 RPC 不复活，功能面按本文 P1-P4 重建）
- B6 的 `mcp.list/startupMetrics/reconnect` 不动（v2 既有面）
- 本设计落地时 rust 侧沿用 B1-B5 建立的全部机制（TSV 行、registry pin、fixture、KLIENT_RESPONSE_SCHEMAS）

### 附：ADR 记录

#### ADR-001 设置页需求触发的 wire 面新增与解析值裁决修订（2026-09-08）

1. **背景与动机**：MCP 设置页需求定稿（mcp-settings-page-requirements.md）：工具清单弹窗需要专门的查询入口；「同步配置」动作需要手动触发的幂等对账入口；「显示真实值」预览需要解析值的例外出口。
2. **决策**：
   - 新增 `mcp.tools(name)`（§4.4，来源 D-S1）：按 server 的工具清单查询，复用 tools/list 缓存
   - 新增 `mcp.sync` + 裁决 A6（§4.3，来源 D-S2）：三个对账触发面（wire 写入 / 文件监听 / 手动同步）共用同一对账内核
   - 裁决 A5 修订（来源 D-S3）：解析值展开增加唯一例外——显式预览请求（不缓存、不进事件/日志）
   - wire 契约汇总由新增 9 + 扩展 1 变为新增 11 + 扩展 1（§4.7）；§5 增补两条被拒选项
3. **被否决的备选**：① 复活 v1 退役的 `mcp.global.*` 命名——退役决策不回退；② 工具清单并入 `mcp.status`——状态查询被高频场景复用、背着全量清单；③ 手动同步采用 v1 式全杀重连 reload——推倒全部健康连接，扰动面 = 全部 server。
4. **影响范围**：§4.3 / §4.4 / §4.7 / §5、裁决 A5、裁决 A6；域需求 N9（对应 SDR-001）。

#### ADR-002 链路终点绑定活跃会话工具面（2026-09-08）

1. **背景与动机**：域需求 US4.4 显式化"配置变更须及时触达执行中的 agent 会话（下一步可感知/可调用）"，热更新链路需要明确终点消费方，消除"池已变、会话工具面未变"的实现灰区。
2. **决策**：§4.3 事件链的终点明确为活跃会话的工具注册面——池增量操作完成后刷新执行中任务的工具清单；在途调用的处置遵循域 US4.1 裁决（完成或以明确错误终止）。**（ADR-014 细化，2026-10-21）**：终点覆盖该会话全部 agent（含子代理），并新增 agent 装配时刻（派发/恢复）作为等价入口。
3. **被否决的备选**：仅靠状态事件让 UI 刷新、会话侧自轮询——事件面向观察者，不保证执行中任务的工具面同步，且引入轮询噪音。
4. **影响范围**：§4.3 链路终点说明；域需求 US4.4（对应 SDR-002）。

#### ADR-003 调用时点强制校验（2026-09-08）

1. **背景与动机**：域需求 US4.5 要求禁用/启用在工具调用的校验硬环节被及时阻拦/放开，需明确强制点的位置与判定依据，消除"仅从清单移除、直接调用仍被放行"的实现灰区。
2. **决策**：强制点位于工具调用管线的校验环节（执行前、先于连接建立/复用）；判定以调用时点的有效配置为准、按调用所属作用域解析（多层遮蔽遵循合并优先级）；清单注册与调用校验是两层独立防线（防御纵深）。
3. **被否决的备选**：① 仅靠清单移除（可见性）——清单可能过期且存在绕过模型的直接调用入口；② 仅在连接层拦截——强制点必须前置于连接判定，否则直接调用路径绕过。
4. **影响范围**：§4.3 调用时强制点说明；域需求 US4.5（对应 SDR-003）。

#### ADR-004 新增方法的 Rollout 阶段落位（2026-09-08）

1. **背景与动机**：ADR-001 新增 `mcp.tools` / `mcp.sync` 后 §8 阶段表未同步，两方法无落位，阶段交付物与 §4.7 契约汇总不一致。
2. **决策**：`mcp.tools(name)` 落 P2（与状态/测试同属管理面呈现）；`mcp.sync` 落 P3（与热更新共用对账内核，内核在 P3 建立）。
3. **被否决的备选**：`mcp.sync` 落 P4——对账内核 P3 才建立，手动入口晚于内核落地只增加阶段而无可复用差。
4. **影响范围**：§8 阶段表。

#### ADR-005 契约补齐：并发版本、解析值预览、目标工作区（2026-09-08）

1. **背景与动机**：需求-架构追溯发现三处无契约支撑（域 N3/US1.2/N-S3 的并发编辑冲突检测、PS2.5/D-S3 的预览出口、PS2.2 的写入选定工作区）。代码验证确认现状为纯 last-writer-wins（无任何 revision 机制）、rust wire 无预览方法、目标工作区参数未明确（现状依据见 mcp-context.md §7）。
2. **决策**：① `mcp.config.list` 响应增加每层 `revision`；`set/remove/setToolFilter` 增加可选 `expectedRevision`，不匹配返回明确的冲突错误；② 新增 `mcp.config.preview`——裁决 A5 唯一例外的出口，显式请求、不缓存、不进事件与日志；③ 带 workspace 参数的方法接受显式目标工作区（配置页在无当前工作区上下文时可选定已打开的工作区写入其 project 层）。落 Rollout P1。
3. **被否决的备选**：① 不做乐观锁——并发编辑静默覆盖，违反域 N3；② 预览搭 `mcp.config.test`——测试是连接诊断，返回形状不含完整解析视图；③ 前端本地展开——前端无服务端环境可见性，展开结果与真实连接行为不一致。
4. **影响范围**：§4.1 契约块、§4.7（新增 12 + 扩展 1）、§8 P1；闭合域需求 N3/US1.2/US1.6/PS2.1/PS2.2/PS2.5 的支撑（无需求变更）。

#### ADR-006 setToolFilter 目标层语义与过滤族分层合并（2026-09-08）

1. **背景与动机**：域 US2.3（项目层单项覆盖用户层工具开关）在整条遮蔽语义下不可表达（代码验证：合并为整条替换、`setToolFilter` 固定写最高优先级定义层且无 scope 参数）；且其目标层解析包含 project-root——机器写入本应只读的 git 共享文件，与裁决 A2 的写路径约束冲突（现状依据见 mcp-context.md §7）。
2. **决策**：① `setToolFilter` 增加目标作用域参数：缺省保持现状语义（写定义层）；显式 workspace 作用域在项目层创建**过滤覆盖条目**（仅含过滤字段，连接定义仍取被遮蔽层）；② 新增裁决 A7——合并按字段族：连接族整条遮蔽不变，过滤族显式才覆盖；③ project-root 层对全部 MCP 写方法（含 `setToolFilter`）永不机器写入。
3. **被否决的备选**：① 用 `mcp.config.set` 整条复制实现项目层覆盖——那是"项目重定义 server"，连接定义随上游演进腐化；② 过滤字段集合并集——黑白名单语义下并集使"下层启用 + 上层禁用"不可表达；③ 保持现状并修订 US2.3——US2.3 是已评审需求，架构应满足而非删需求。
4. **影响范围**：§4.1 契约块、裁决 A7（新增）、§8 P1；现存实现偏差的落地修正追踪在 [mcp-ROADMAP.md](mcp-ROADMAP.md)（开放项）。域需求无变更。

#### ADR-007 解析失败分层诊断与 diff 措辞修正（2026-09-08）

1. **背景与动机**：域 PS1.4/PS4.2 要求页面明示"哪层解析失败+原因"，但契约未定义读取语义（代码验证现状：`mcp.config.list` 遇坏层整个 RPC 失败——页面连列表都打不开；watcher 与 reconcile 仅日志，见 mcp-context.md §7）。另：reconcile 实际对非连接绑定字段变更（超时等）不断连接，§4.3 原四类措辞（tool-filter-change）未覆盖此含义。
2. **决策**：① `mcp.config.list` 成功响应内带每层解析状态（ok | failed+原因），坏层条目缺省呈现、诊断可见，整个请求不失败；写路径/reconcile 的"解析失败保守保持现状"确立为契约（§4.3）；② §4.3 diff 分类措辞修正：`tool-filter-change` → `field-change`（非连接绑定字段变更——工具过滤/超时等 → 不重连，仅更新配置与工具注册并广播事件）。
3. **被否决的备选**：① 维持整 RPC 失败——坏一层导致全局列表不可用，违反 PS1.4 的诊断诉求；② 解析失败静默跳过——不可见等于未发生，无法引导用户修复。
4. **影响范围**：§4.1 契约块（list 响应）、§4.3；闭合域需求 PS1.4/PS4.2/US4.2 第 3 条的支撑（无需求变更）。

#### ADR-008 变更通知覆盖非重连性配置变更（2026-09-08）

1. **背景与动机**：自查发现 §4.3 事件链只挂在池增量操作上——非重连性配置修改（如仅调整超时，域 US4.2 规定连接保持）不产生池操作、status 不变，于是不产生事件；外部编辑该类字段后，其它客户端的字段级呈现将停留在旧值，设置页 PS1.3 的字段级一致性存在破口。
2. **决策**：配置条目的任一持久化变更都产生变更通知（含变更类别），无论是否触发池增量操作、status 是否变化；事件负载为 `{name, pool, status}` + 变更类别字段。
3. **被否决的备选**：仅靠操作返回刷新——只覆盖本页发起的变更，外部编辑（手工/git/其它客户端）场景破口依旧。
4. **影响范围**：§4.3 事件触发面；域 US4.3、设置页 PS1.3 依赖此语义。
5. **后续修订**：事件命名与订阅面经 ADR-010 修订为 `event.mcp.server.status` + 双订阅面；本条的触发面语义（任一持久化变更都发、含变更类别）不变。

#### ADR-009 文档落位澄清与契约细节补全（2026-09-08）

1. **背景与动机**：规格评审发现四处文档级缺口——① §8 阶段表会随执行推进演化，而域 ROADMAP 缺位（状态寄居在 rust-core-design/ROADMAP.md），与"设计文档不持有进度"的治理约定冲突；② 服务器级启停的写路径未在契约中写明，实现者可能自造独立开关方法（即 v1 已退役的 `mcp.enable/disable`）；③ 事件负载与页面信息需求（工具数/错误）的关系未定义——推送全量与触发重取可各自解读；④ tools/list 缓存 TTL 只在风险表出现"上限"字样，无契约值，两侧一致性无从谈起。
2. **决策**：① 建立域 ROADMAP（[mcp-ROADMAP.md](mcp-ROADMAP.md)）为执行状态唯一家；§8 保留各阶段**范围定义**（设计内容）并指向之，不随执行进度更新；② §4.1 写明启停 = `mcp.config.set` 读改写 `enabled` 字段、无独立开关方法；③ §4.3 写明事件触发重取语义（轻量通知，负载不含工具清单与错误详情）；④ 缓存 TTL 上限契约默认 10 分钟（§4.2 裁决 A3、风险 R4）；⑤ 动机性内容抽象化（Goal/成功标准中的现状基线与对比措辞移出，现状盘点归 mcp-context.md）。
3. **被否决的备选**：① 阶段表整体迁入 ROADMAP——阶段划分与各阶段范围属设计裁决（ADR-004 已记录），迁出会让 RFC 失去交付叙事完整性；② 新增独立启停方法——与"一切开关都是配置"的哲学冲突（Non-Goals 第 1 条）；③ 事件负载携带全量信息——每次状态翻转推全量工具清单，重且与 `mcp.tools` 查询面职责重叠。
4. **影响范围**：§4.1、§4.2（裁决 A3）、§4.3、§7、§8；域文档 §10 的阶段映射注记（SDR-007 同批）。

#### ADR-010 事件契约对齐既有实现（2026-09-08）

1. **背景与动机**：实现核查发现引擎已存在 session 级事件 `event.mcp.server.status`（mirri-core 事件投影，池级 fan-out 到活跃会话的各事件总线），与本设计先前命名的 `event.mcp.serverChanged` 指同一语义；两个近似事件并存会使消费方与契约双双分裂。且该事件仅 session 级可达——设置页等无会话上下文的消费方没有任何订阅面（现状依据见 mcp-context.md §3）。
2. **决策**：① 事件统一名为 `event.mcp.server.status`（`serverChanged` 命名作废——ADR-001/ADR-008 正文已同步）；② 负载扩展 `pool` 与变更类别（继承 ADR-008 语义：任一持久化配置变更都发）；③ 订阅面分两层——session 级 fan-out（活跃会话，US4.4 的链路终点）与新增 **app 级订阅面**（无会话上下文的消费方，PS1.3 / 域 US4.3）；④ RefreshToolFilter 分支必须补发事件（现状缺口，开放项见 mcp-ROADMAP.md）。
3. **被否决的备选**：① 保留 `serverChanged` 作为并行新事件——语义重复，消费方、klient 契约与文档三处双份漂移；② 只给 session 事件加 app 广播、不改名——名字分裂在功能落地前修正成本最低。
4. **影响范围**：§4.3、§4.7、ADR-008（尾部修订注记）；域需求 US4.3 与设置页 PS1.3 引用的是"状态变化通知"能力而非方法名，无需求变更。

#### ADR-011 OAuth 授权流形状修订：双发起路径（2026-09-08）

1. **背景与动机**：实现核查发现 rust 侧 OAuth 已按"needs-auth 时向活跃会话注入合成工具 `mcp__<server>__authenticate`"落地（RFC 9728/8414/7591 发现 + PKCE + loopback + token 持久化 + 透明刷新 + 自动重连），与本设计 §4.5 原定的 `mcp.auth.start/complete` 单一路径形状不同；且两侧 token 磁盘布局不互通（违反域 US6.2/N4 的跨引擎兼容要求。现状依据见 mcp-context.md §4）。
2. **决策**：① 双发起路径收敛为终态——会话内走合成工具（已落地形态，记为设计认可），wire 走 `mcp.auth.start/complete`（P4 落地）服务无会话上下文的消费方（设置页 PS4.3）；两条路径共享同一 token 存储后端；② token 磁盘布局双侧统一为 P4 强制任务（对齐 US6.2/N4）；③ 裁决 A4（stdio 无 OAuth、设计只覆盖远程 transport）不变。
3. **被否决的备选**：① 全面改设计为合成工具单路径——设置页无会话上下文无法发起授权，PS4.3 无承载；② 撤掉合成工具只走 wire——已落地且会话内体验更好（用户不离开对话流即可完成授权），推倒重来无增益只添风险。
4. **影响范围**：§4.5、§4.6（OAuth 行）、§4.7（`mcp.auth.*` 标注双路径）、§8 P4（补"令牌布局统一"）。

#### ADR-012 setToolFilter 签名参数序与裁决 A7 落地追记（2026-09-08）

1. **背景与动机**：ADR-006 决策①的落地遇到两处规格-实现差异：① §4.1 契约块写 `setToolFilter(scope, name, filter, expectedRevision?)`（scope 在**前**），但 klient 既有签名为 `(name, filter, expectedRevision?)`——N5 约束（新参数只能尾部追加、旧调用形状不变）与契约参数序冲突；② 裁决 A7 需要区分"完整条目"与"过滤覆盖条目"，而 `McpServerConfig` 自定义 Deserialize 把无 transport 的 Claude-form 条目（command/url）与过滤覆盖条目都归一为对象 transport，解析产物无法区分家族归属。
2. **决策**：① klient `setToolFilter` 输入为 `z.tuple([string, mcpToolFilterSchema, scope?, expectedRevision?])`——scope/expectedRevision 尾部追加，旧三参形状 `[name, filter, expectedRevision]` 语义不变（单尾部字符串按 N5 读作 expectedRevision；仅当该字符串恰为字面量 `"user"`/`"workspace"` 时读作 scope——revision 为 16 位 hex 或 `"absent"`，永不与 scope 字面量冲突，判定无歧义；scope 无 revision 建议用对象拼写 `{name, filter, scope}`）；与 §4.1 参数序的差异以本条为准；② A7 落地形态：`mirri_mcp::config::merge_layers` 依据**原始 JSON** 判定完整条目（`entry_has_transport` = 显式 `transport` 键或可推断连接的 `command`/`url`——Claude-form 条目保持完整），过滤覆盖条目（仅 `enabledTools`/`disabledTools`，无连接字段——`upsert_filter_override_entry` 产物）只参与过滤族；`load_mcp_config`/`project_merged`/workspace reconcile 连接集全走 A7，仅过滤条目且无完整定义的名字不产出连接视图；③ 无 scope 写目标解析为 `[local, global]` + root 守卫：~~命中 project-root 定义返回 `mcp.config.read_only` 域错误（A2），不回落到 user 层~~（2026-10-17 SDR-S005 修订）：命中 project-root 定义时，写 filter-only override 到项目本地层（`upsert_filter_override_entry`）——A2 "root 文件永不机器写入"仍成立（override 落项目本地层，root 文件字节不动）；用户层定义仍直接 patch 用户文件（本句不变，owning-layer 语义保持）。修订动机：设置页要求工具级控制覆盖所有来源（mcp-settings-page-requirements.md PS3.3），read_only 直接拒绝使团队共享来源的 server 无法开关工具；override 路径与其显式 workspace scope 的既有行为一致。
3. **被否决的备选**：① 严格位置参数（3 元素 = scope）——破坏旧 `[name, filter, expectedRevision]` 调用；② 服务端按类型区分 scope 与 revision——两者同为字符串，静态类型无法区分；③ 按 §4.1 参数序改前置 scope——同样是破坏性变更，且与 klient 既有调用无法共存；④ 过滤覆盖条目反序列化即整文件报错——A7 覆盖条目是合法存储形态，加载必须容忍。
4. **影响范围**：§4.1 契约块（setToolFilter 签名，本条为参数序事实源）、裁决 A7（落地形态）、[mcp-ROADMAP.md](mcp-ROADMAP.md) 对应行；域需求无变更。

#### ADR-013 懒连接与工具清单缓存裁决不实施（2026-10-12）

1. **背景与动机**：裁决 A3 为 WorkspacePool 规划了懒连接 + tools/list TTL 缓存，落点为 Rollout P4。实施前核验发现两个前提已不成立：① 连接不阻塞启动——现状为「服务启动后在后台连接全部已启用服务器，连接 spawn 在首次引擎调用后的 Tokio runtime 内、且从不阻塞任何 RPC 或 UI 操作」，US3.2 的用户效果已天然成立，懒连接解决的痛点不存在；② 服务器状态变化有全链事件通知（`event.mcp.server.status`，含连接建立、工具数与变更类别——ADR-010），消费方可实时感知，无需"连接中"过渡态设计。
2. **决策**：懒连接与 tools/list 缓存**均不实施**。决定性理由：MCP 工具清单是发给模型的请求前缀的组成部分，服务器在会话中途连上会令工具表在前缀**中段**突变，破坏供应商提示缓存（prompt cache）的命中——此类「前缀中段突变」的代价在本仓缓存命中率治理中已有实测教训；现状「启动后即后台全连、首轮对话前基本就绪」恰好保证工具表稳定。清单缓存亦无必要：清单只在连接建立时清点一次，连接保持期间天然驻留内存，即缓存本身；server 静默更新已有重连逃生门覆盖。
3. **被否决的备选**：① 保留懒连接但连接完成前冻结工具表快照——引入"快照 vs 真实"双真相与失效时序问题，复杂度高于收益；② 仅对未信任/低频 workspace 懒——时序语义随信任状态分叉，事件链与工具注册的推理成本陡增；③ 保留 TTL 缓存作为防御——缓存与"重连即失效"逃生门功能重叠，维护两套一致性无增益。
4. **影响范围**：裁决 A3 标注修订；Rollout P4 范围收窄；域需求 US3.3/US3.4 标注不实施（SDR-008）；ROADMAP P4 缺口行收口。`mcp.auth.start/complete`（无会话授权入口）与 F-8（令牌布局跨引擎统一）不受本裁决影响，仍为 P4 开放项。

#### ADR-014 链路终点细化：会话内全部 agent 的工具面，派发与恢复两入口同权（2026-10-21）

1. **背景与动机**：ADR-002 将热更新链路终点绑定为「活跃会话的工具注册面」，未显式回答**派发时刻新进入的 agent**（子代理）是否属于该终点——子代理在池状态变化之后 spawn，错过全部已发生的广播注册，其工具表对 MCP 工具恒为空（即使其角色声明了授权）。恢复路径的子代理水合已从主 agent 工具表快照补注册，两入口行为不一致。跨域裁决（subagent 域 ADR-014 / SDR-014）已定「交付与过滤分离」，本 ADR 将本域的终点语义与之对齐。
2. **决策**：§4.3 链路终点「活跃会话的工具注册面」细化为「该会话**全部 agent**（主 agent 与子代理）的工具注册面」；该终点有两个等权入口——①池状态变化的活跃 agent 广播（既有机制，子代理 spawn 后即被覆盖）；②agent 进入会话的装配时刻（live 派发 spawn、重启恢复水合），从会话工具收敛点（主 agent 工具表）取当前快照注册。在途调用的处置仍遵循域 US4.1 裁决。技术形状与归域见 subagent 域 ADR-014。
3. **被否决的备选方案**：a) 终点维持仅主 agent 工具表——子代理工具表恒空，角色声明的 MCP 授权空诺（subagent 域 SDR-014 的能力虚诺）；b) 派发时重连 MCP 服务器——连接是会话级共享资源，重连扰动全部会话；c) 子代理调用时惰性发现——把注册面问题转化为每次调用的解析成本，且工具可见性（模型函数列表）仍需注册面支撑。
4. **影响范围**：ADR-002 的终点表述（本 ADR 细化）；域需求 US4.4 的消费主体隐含范围（全部 agent）；subagent 域 ADR-014 / SDR-014（跨域同批）。
