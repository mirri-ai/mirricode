# Rust Core ACP 复用 Desktop 运行时（ACP Desktop Runtime Sharing）— 设计文档

> 状态: Draft rev 4（三轮独立设计评审收敛：rev 1 B1-B2/H1-H4/M1-M4/L1-L3 · rev 2 N-H1/N-M1-N-M4/N-L1-L3 · rev 3 R3-H1/R3-M1/R3-M2）· 2026-10-28 ·
> 作者: Architecture Team
> 上游: [00-overview.md](00-overview.md) §2 目标架构 · 需求侧裁决: [../session-persistence-requirements.md](../session-persistence-requirements.md) SDR-009 / ADR-019（跨入口会话可见性——本设计是其"desktop 在场时"的增强路径）
> **执行状态不在本文维护**：阶段进度、批次状态、开放项统一见 [ROADMAP.md](ROADMAP.md)；现状证据见本文 §1 与 [context.md](context.md)

## 1. 背景与动机

SDR-009 已裁决 IDE（ACP）入口的会话沉淀个人数据目录（写共享、读钉扎），解决了"desktop 不在场时"的可见性。但"desktop **在场**时"，ACP 仍走自己的嵌入引擎——两个引擎进程并发写同一份 home 数据，desktop 界面对 ACP 会话的进展**没有实时投影**（只有落盘后的列表可见性，无运行态跟随）。

由此产生三个问题（终态达成后即消失）：

1. **实时投影缺席** — 用户在 IDE 里驱动的回合（消息增量、工具进度、turn 生命周期），desktop 界面看不到进行中的流，只能等落盘后看结果。而"深度编码在 IDE、审查整理在 desktop"的动线恰恰要求两边近乎同步。
2. **双引擎并发窗口** — ACP 嵌入引擎与 desktop server 共享 home，两个进程同时活着的窗口内，会话一致性域的全部协调问题（并发写、对账互踩）都真实存在，而其双开跟随能力尚在实验门控后（默认关闭）。
3. **运行资源重复** — 同一工作区上，ACP 嵌入引擎与 desktop server 各持一份 MCP 连接池、插件加载、配置快照。

**已有的轨道与它的缺口**：ACP 的远程引擎模式（显式环境变量 → 远程适配器，连接 server 的 RPC 端点，审批/问询反向 RPC 已在握手协商）提供了可复用的客户端轨道，但它是为"ACP 连自己的私有 server"设计的——连接即对全部会话自动订阅的行为与 desktop 共享场景正面冲突（见 D5）；且宿主整链缺自动发现、回退、工作区路由与重连。本设计补齐的接线是：**自动发现 desktop server、回退嵌入、工作区路由、订阅策略与审批仲裁、重连与开关**五块。现状证据（适配器与宿主链的测试覆盖差异、订阅行为细节）见 [context.md](context.md)。

## 2. 目标架构

**ACP 宿主启动时探测本机 desktop server（读其状态文件 + pid 存活 + 端口探活），活着则以远程客户端复用之；不在场（或连接失败）时回退现有嵌入引擎形态。会话真实运行在 desktop 引擎里，desktop 界面的实时投影零新增机制——引擎内会话的订阅/游标/replay 全保真通道直通。**

```
mirri acp 启动
│
├── 0. MIRRI_ACP_REMOTE_ENGINE（显式 URL）→ 强制远程（最高优先级，运维覆盖面，不动）
│       └─ token 优先级：MIRRI_ACP_REMOTE_TOKEN > server.token 文件（显式模式两者皆可）
│
├── 1. MIRRI_ACP_ENGINE=embedded → 强制嵌入（不探测——测试确定性与显式 opt-out 语义）
│
├── 2. 自动探测（新增默认路径，MIRRI_ACP_WORKSPACE_DIR 设置时禁用——测试信号量）
│   ├── 读 <home>/server/desktop-state.json → {pid, port}
│   ├── pid 存活 + GET http://127.0.0.1:<port>/api/v1/healthz 双验证
│   ├── 握手后版本/能力校验（server_hello 的 version 与 RemoteCaps；不满足视为探测失败）
│   ├── 通过 → RemoteAdapter::connect(ws://127.0.0.1:<port>/api/v1/rpc, <home>/server.token)
│   │         —— 以 ACP 订阅策略连接（D5：不做连接期全量自动订阅）
│   └── 失败（无状态文件 / pid 已死 / 端口不通 / 握手超时 / 版本不满足）→ 走 3
│
└── 3. 嵌入引擎回退（现有路径 + SDR-009 的 home 注入——按 ADR-019 落地批次独立交付）
    └── for_workspace_root(…, Some(home), …)

连接中途死亡（desktop 退出）→ 后续新请求触发再探测 →
    成功：以新连接服务后续新请求；重连后对旧会话的首次寻址调用先补订阅再执行
    （D5 的 touch-to-subscribe，不复活未触碰的会话）
    失败：宿主降级为嵌入引擎服务新会话（ADR-019 的 home 注入保证存储归一）；
    旧会话的调用保持 NotConnected 直至 ACP 进程重启（在途回合按既有 transport-error 失败）
```

职责边界：

- **探测层**（新增，`mirri-acp` 内）：纯本机只读探测（状态文件 + 存活验证 + 端口探活 + 握手校验），不做注册、不做锁——desktop server 的生命周期完全由 desktop 所有，ACP 只是受益方。desktop-lite（自有 daemon 与状态目录）**不在探测面内**（非目标：lite 是独立形态，状态文件路径不同，不会误连）。
- **远程会话路由**（新增）：ACP 的 `session/new` / `session/load` 携带 `cwd`；远程模式下把 cwd 翻译为服务端 `session.create` 的 `workspaceId`（见 D2）。
- **订阅策略与审批仲裁**（新增，见 D5）：ACP 只订阅自己驱动的会话；审批反向 RPC 的多客户端归属由服务端既有"最后订阅者"路由 + ACP 侧的透明让渡语义承载。
- **引擎替换接缝**（新增）：远程模式下 AcpHandler 持有的引擎句柄从"构造期固定"改为"可替换的内部槽"——连接死亡触发再探测，成功则新 RemoteAdapter 整体入槽，失败则嵌入引擎入槽（此后新会话走嵌入，旧会话保持 NotConnected）。**远程模式宿主无 factory 构造**（现状代码即如此：远程走单引擎构造，嵌入才有 cwd 池）——远程模式下 cwd 池退化为单槽，会话到引擎的路由表在槽替换时一并失效（旧会话寻址穿透到当前槽，按新连接的 NotConnected 或 touch-to-subscribe 语义呈现，绝不落在已死适配器上）。

### 2.1 与 SDR-009 / ADR-019 的关系

两批互补、不互相依赖、可独立交付：

| 场景 | desktop 在场 | desktop 不在场 |
|---|---|---|
| 本设计（运行时复用） | ACP 连 desktop server，实时投影，单引擎 | 回退嵌入（home 注入来自 ADR-019） |
| SDR-009（存储沉淀） | 远程会话天然落 server 的 home | 嵌入引擎注入 home，落盘可见 |

**对上游否决项的重开（H2，治理义务）**：ADR-019 与 SDR-009 的否决记录均含"IDE 入口默认连接常驻服务进程——远程模式保留为可选环境变量开关，非默认"。本设计在**新前提**下重开该否决项：探测 + 回退瓦解了原否决理由（"要求 IDE 场景必须先有一个常驻服务在跑"——现在不要求，desktop 不在时自动回退嵌入）。本设计落地时须在 ADR-019 与 SDR-009 追加修订记录指向本文档；落地前两份上游文档的"非默认"表述仍然有效。

### 2.2 并发协调的消解

远程复用后，"ACP 嵌入引擎 vs desktop server 双进程写同一会话"的窗口**基本消失**（同 cwd 的 ACP 会话都在 server 里跑）。残余窗口按连接死法分写：

- **server 进程死亡**（desktop 退出）：在场回合按 transport-error 失败；此后新会话经再探测失败降级为嵌入引擎（存储经 ADR-019 归一）；旧会话保持 NotConnected 直至 ACP 进程重启。
- **连接断而 server 活**（网络抖动，罕见于本机环回）：在场会话仍由 server 持有；再探测成功后 touch-to-subscribe 恢复服务。

两个引擎同时写**同一会话**的情形不发生（会话归属随连接一次性确定）。跨引擎并发写共享 home 的既有协调问题归会话一致性域 SC1 管辖（其 ROADMAP 已登记跨域开放项），本设计不新增并发语义。

## 3. 关键设计决策（ADR）

### D1 探测以 desktop 状态文件为唯一锚，不做端口扫描

- **决策**：探测顺序 = `<home>/server/desktop-state.json`（pid+port）→ pid 存活 + `http://127.0.0.1:<port>/api/v1/healthz` 双验证 → 读 `<home>/server.token`（注意位置：home 根下，非 `server/` 子目录）连接 → 握手后版本/能力校验。状态文件缺失时**不**扫端口（没有 desktop 的机器上探测为零成本一次读失败）。版本校验以握手返回的 `server_hello.version` 为最低门（具体门槛值随实现批定，缺省 = 当前主版本号）。
- **理由**：状态文件是 desktop 自己维护的存活事实，pid 双验证是既有模式（desktop 自身的 stale 清理同款）。轮询端口会误连 `mirri serve`（用户终端起的前台服务）——那是用户自己的会话进程，ACP 不应擅自接管。healthz 是公开路由（免 token），探活不消耗认证面。
- **否决的替代**：固定只探 58827 常量兜底（desktop 换端口方案会静默失效，且无法区分"desktop 没在"与"desktop 换了端口"）；扫描 `mirri serve` 也纳入（用户前台进程被 IDE 暗中复用，惊讶行为）；不校验版本（ACP 宿主与 desktop 自带 rust 二进制可能来自不同发布版本，能力漂移会在运行中段以难解的形式炸出）。

### D2 远程模式的会话路由：cwd → workspaceId 翻译在 ACP 端；未注册 cwd 一律拒绝

- **决策**：ACP 远程模式下，`session/new`/`session/load` 的 cwd 翻译为引擎 `session.create` 的 `workspaceId` 参数：先远程 `workspace.list` 匹配已注册根；命中 → 直接路由；**未命中 → 该请求报错**（明确告知"该目录未注册为 desktop 的工作区"），不回退、不自动注册。根匹配的规范化采用**词法规范化**（`lexically_resolve`，与注册表根的存储形态对齐），不解析符号链接。
- **理由**：工作区决定会话的文件系统沙箱根与权限作用域——未注册 cwd 若回退 server 默认工作区，工具调用的文件读写范围会整体落在与 IDE 项目无关的目录上，是文件写错位置的正确性问题，不是"归属失真 + 告警"能涵盖的。直接拒绝与服务端"未知 `workspaceId` 报 `invalid_params`"对称（错误语义一致：要么路由到正确的根，要么诚实报错），也是对 IDE 用户最可预期的行为（错误消息可指导其先在 desktop 打开该目录）。
- **否决的替代**：未注册回退默认工作区（沙箱根错位——被否决的核心）；ACP 端自动注册（注册语义属 desktop/web 的用户操作面，IDE 的临时目录会污染全局工作区注册表）；服务端新增"按 cwd 自动注册"入口（注册副作用藏在 create 里，同污染）。

### D3 探测的一次性、再探测与显式开关

- **决策**：
  - **启动探测一次**；三种出口（远程成功 / 探测失败回退嵌入 / 无 desktop 回退嵌入）只写 stderr 日志，不向 IDE 客户端发任何 ACP 通知（`initialize` 响应不携带引擎形态——引擎形态对 IDE 是实现细节）。
  - **连接死亡后的再探测**：远程连接死亡时，ACP 对后续新请求触发一次再探测（desktop 回来了就以新连接继续服务）；**不迁移在途回合**（连接死亡的在场回合按既有 transport-error 语义失败，pending 失败已有测试）；再探测失败则**该次**请求按 `NotConnected` 语义报错，且宿主自此降级为嵌入引擎服务新会话（进程不崩；降级后不再自动再探测——desktop 回归后新连接需 ACP 进程重启触发，IDE 重启即触发）。再探测成功后，对重连前已存在会话的首次会话寻址调用，适配器先补订阅再执行（D5 的 touch-to-subscribe）。
  - **显式开关与优先级**（自上而下）：`MIRRI_ACP_REMOTE_ENGINE`（显式 URL，最高优先级强制远程）> `MIRRI_ACP_ENGINE=embedded`（强制嵌入，不探测）> 自动探测（`MIRRI_ACP_WORKSPACE_DIR` 设置时禁用——测试信号量，既有测试用它指向临时目录保证封闭性）。
- **理由**：回退嵌入后 IDE 继续可用（SDR-009 的底座）；再探测不迁移的语义把"重连"限制为无状态的新连接（无在途回合/事件游标/审批挂起的搬迁问题）。显式 opt-out 是确定性要求：开发机/CI 上恰有 desktop 运行时，ACP 测试会静默走远程，破坏封闭性。
- **否决的替代**：运行期持续探测 + 会话热迁移（在途回合、事件游标、审批挂起无法搬迁，收益不抵）；连接死亡后进程自杀等 IDE 重启（可用性倒退）；自动探测无开关（用户脚本行为随"desktop 是否在跑"非确定翻转，不可复现）。

### D4 token 读取走既有文件语义，不新增认证面

- **决策**：自动探测模式下 token 只来自 `<home>/server.token`（0600 权限文件，同机同用户可读），作为 bearer 连接。显式 `MIRRI_ACP_REMOTE_ENGINE` 模式下 token 优先级为 `MIRRI_ACP_REMOTE_TOKEN` > `server.token` 文件（现状行为保持）。不为 ACP 引入独立的连接凭据或 desktop 侧的授权确认。
- **理由**：ACP 与 desktop 是同一用户同一台机器上的两个进程，token 文件本身就是"同 home 即同信任域"的既有边界（web UI 也这样读）。
- **否决的替代**：desktop 弹窗确认"IDE 想连接"（打断流且无安全增益——token 已可读）；每连接一次性 token（需要 desktop 增设签发面，收益为零）。

### D5 ACP 远程订阅策略与审批路由仲裁

- **决策**：
  - **禁用连接期全量自动订阅**：`RemoteAdapter` 为 ACP 形态提供 opt-in 构造参数（如 `SubscribePolicy::OwnSessionsOnly`）——连接后不调用连接期的 `session.list` + 逐会话 `session.subscribe`。这一禁用同时消除了"连接即批量复活全部持久化会话进 server 内存"的副作用（服务端订阅处理器对 persisted-only 会话按需复活）。
  - **订阅触发集 = 本连接驱动会话的全部入口**：自动订阅触发集从引擎方法的固定名单（create/resume/fork）**扩展为"ACP 宿主实际驱动会话创建的调用点"**——包括 `session.create`、`session.resume`，以及 ACP 的 `session/fork` 实际调用的引擎方法 `forkSession`（该方法名不在适配器现有触发集内，需补入或由 ACP 宿主层显式触发订阅）。**touch-to-subscribe**：`OwnSessionsOnly` 下，对"本连接尚未订阅的会话"的任何会话寻址调用（prompt/cancel/fork/close 等），适配器先补 `session.subscribe` 再执行原调用——覆盖重连后对旧会话的首次触碰（服务端的按需复活随订阅发生，事件流与审批 callback 槽随订阅安装）。touch-to-subscribe 不触碰从未被寻址的会话（不复活；`close` 对未订阅旧会话会先瞬态复活再关闭——瞬态无害，语义一致）。
  - **审批路由的服务端既有语义 + ACP 透明让渡**：服务端审批反向 RPC 是"最后订阅者赢"（每连接订阅会话时把自己的 callback 盖到会话 artifacts 上）——该语义**保持不动**：ACP 订阅自己驱动的会话后，该会话的 `approval.ask` 路由到 IDE（正确——IDE 用户正在驱动它，ACP 把 ask 翻译为 `session/request_permission` 呈现给 IDE 用户，这条桥已有测试）；web/desktop 界面随后打开同一会话（它也订阅）时路由让渡给 web——**这是期望行为**：谁最后订阅谁接管审批交互，ACP 端在路由让渡后收到的不再有 ask，不产生双端竞答。多客户端审批的完整仲裁（广播 vs 单播）如未来需要，属会话一致性/事件投递域的裁决，本设计不预设。
  - **会话关闭即退订**：ACP `session/close` 后适配器退订（服务端 `session.unsubscribe` 方法存在，适配器侧调用是**新接线**；退订后 callback 槽保持已附着状态——服务端既有语义：连接死亡/吊销不清槽，本设计不改变该语义）。
- **理由**：连接期全量订阅是"ACP 连自己的私有 server（全量会话本来就是自己的）"场景的合理默认，desktop 共享场景它是三个连锁错误的根源（批量复活、路由抢占、跨工作区事件泄漏）——订阅策略按宿主形态分叉，比改服务端语义（广播审批、订阅白名单）侵入面小一个数量级。审批的"最后订阅者赢"在 IDE↔web 切换的动线上恰好语义正确（正在看会话的人接管审批）。touch-to-subscribe 把"订阅"从"创建时的事件"扩展为"首次触碰时的事件"，使重连后的旧会话服务完整（事件流可达 + 审批 callback 重装）。
- **否决的替代**：服务端把审批改为广播给所有订阅连接（协议与语义升级，涉及在途审批的重复应答去重，超出本设计必要面）；保持连接期全量订阅（B1 三连锁）；ACP 侧静默吞掉 ask（审批挂起到超时，IDE 用户以为在跑实则卡死——可用性谎言）；重连后对旧会话调用直接报错、强制 IDE 走 `session/load`（语义可行但动线断裂——IDE 的下一动作往往是继续对话而非显式加载）。

### D5a 远程模式的 MCP overlay（归属声明）

- **决策**：会话级 MCP overlay 在远程模式下按既有参数透传（ACP 的 `mcpServers` 请求参数 → `session.create`/`session.resume` 的 overlay 入口），overlay 在 server 侧会话上生效，无需新增机制。
- **理由**：overlay 是会话作用域的请求参数，不依赖宿主进程形态；现状透传路径已覆盖远程调用面。
- **否决的替代**：ACP 侧本地连接 overlay servers 再桥接（把连接生命周期、认证、事件转发全部重造一遍，无收益）。

### D6 远程模式的列表可见性（US11.4 承载位置）

- **决策**：远程模式下 IDE 以项目为中心的列表语义，由 **ACP 宿主进程在数据读取后过滤**承载：`session/list` 查询服务端（serve 形态无钉扎、全注册工作区可见），ACP 端按**驱动该 list 请求所依据的 cwd**（ACP 宿主启动根——IDE 以单项目根启动宿主，列表随宿主根）匹配的工作区 id 过滤结果再呈现给 IDE。过滤用 D2 同一套 cwd → 工作区 id 翻译。该承载位置与 US11.4 "数据读取侧执行、不是界面层展示过滤"的条款**不冲突**：过滤发生在 ACP 宿主（引擎的客户端代理层）的数据读取面，不是 IDE 插件界面层。
- **理由**：US11.4 的钉扎语义在远程模式没有引擎侧载体（`ListingVisibility` 是嵌入宿主构造），必须显式声明承载位置；ACP 宿主过滤与嵌入宿主钉扎对 IDE 客户端呈现同一语义。
- **否决的替代**：不过滤、全量呈现给 IDE（多项目混用时 IDE 列表被无关工作区污染，US11.4 语义在远程模式静默失效）。**已知取舍**：词法规范化匹配对同目录异拼写（macOS `/var` ↔ `/private/var`）会误报"未注册"——与注册表根的既有存储形态一致（注册表按词法形式存根），此类拼写差在 desktop 注册时同样存在，故不额外做符号链接折叠（保持与 D2 同一规则）。
- **上游回写义务**：本决策是对 US11.4 执行位置条款的**承载位置增补**（"钉扎在数据读取侧"需补"远程模式下由 ACP 宿主的读取过滤承载"）——落地时在 `session-persistence-requirements.md` US11.4 追加验收增补，本文档 §6 登记该跨域动作。

## 4. 与既有架构裁决的关系

| 既有裁决 | 关系 |
|---|---|
| SDR-009 / ADR-019（跨入口可见性） | 互补不依赖：本设计是"desktop 在场"分支的增强；ADR-019 的 home 注入服务"desktop 不在场"分支。两批可独立交付，交付顺序不限。**本设计重开其"远程非默认"否决项**（§2.1 的重开声明） |
| 工作区管理域 SDR-006（宿主钉扎） | 嵌入模式钉扎不变（引擎侧 `ListingVisibility`）；远程模式的等价语义由 ACP 宿主读取过滤承载（D6），US11.4 执行位置条款的承载位置增补见 D6 回写义务 |
| 会话一致性域 SC1（另一入口） | 并发窗口基本消除（单引擎）；残余窗口（回退链上 server 持有的在途回合）归其既有管辖与 ROADMAP 开放项 |
| ADR-005-R1（存储根切割） | 无触碰：远程会话落 server 的 home `v3_sessions`，与 serve 形态一致 |
| `MIRRI_ACP_REMOTE_ENGINE` 既有语义 | 保持为最高优先级强制覆盖，自动探测只在其缺席时生效（D3 优先级链） |

## 5. 验收标准（设计级）

1. desktop 在场：IDE 内 `session/new`（已注册工作区 cwd）→ desktop web 界面**实时**看到该会话的消息增量与工具进度（以引擎订阅推送为准，非落盘轮询）；会话落 desktop server 的 home 存储。
2. desktop 在场但 cwd 未注册：`session/new` 报错，错误消息指明"目录未注册为 desktop 工作区"；不产生新注册行、不创建会话、无沙箱错位的会话产生。
3. desktop 不在场：ACP 静默回退嵌入引擎（行为与 SDR-009 交付后的现状一致），IDE 正常可用。
4. 探测无副作用且可关闭：无 desktop 的机器上，ACP 启动只多一次状态文件读失败（无端口扫描、无注册表写）；`MIRRI_ACP_ENGINE=embedded` 或 `MIRRI_ACP_WORKSPACE_DIR` 设置时不探测。
5. 显式 `MIRRI_ACP_REMOTE_ENGINE` 指定时行为与现状逐字节一致（强制覆盖不被自动探测拦截）。
6. 远程连接中途死亡：在场回合按既有 transport-error 语义失败；后续新请求触发一次再探测，再探测失败时该请求按 `NotConnected` 错误呈现且宿主降级为嵌入引擎服务新会话（旧会话保持 `NotConnected`）；再探测成功后，对重连前已存在会话的首次调用先补订阅（该会话的事件流恢复、审批回调可用），未再被触碰的会话不被复活；ACP 进程全程不崩。
7. 订阅策略：ACP 远程连接建立后，server **不复活**任何 ACP 未驱动的持久化会话；ACP 只收到自己创建/加载/触碰（含 fork）的会话的事件通知。
8. 审批路由：ACP 驱动的会话在 IDE 侧收到 `session/request_permission`；web 随后打开同一会话后，后续审批路由到 web，IDE 不再收到（最后订阅者让渡，无双端竞答）。
9. 列表钉扎：远程模式下 IDE 的 `session/list` 只见当前 cwd 所属工作区的会话（含该工作区历史会话）。

## 6. 文档关系

- 需求侧（跨入口可见性）：`../session-persistence-requirements.md` Epic N11 / SDR-009；`../session-persistence-architecture.md` ADR-019。**落地时的上游回写**：① ADR-019 与 SDR-009 各追加一条修订记录（"远程默认否决项经 14 号设计在探测+回退前提下重开"）；② US11.4 追加执行位置增补（远程模式由 ACP 宿主读取过滤承载，见 D6）。
- 并发协调残余：`../session-consistency-ROADMAP.md` 跨域开放项。
- 实现计划：`plans/`（本设计评审通过后另立）。
