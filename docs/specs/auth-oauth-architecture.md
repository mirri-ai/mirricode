# 认证（auth-oauth）架构文档

> 本文档描述认证域的实现机制、接口形状与数据流；动机保持抽象（不含代码事实），现状依据见 [auth-oauth-context.md](auth-oauth-context.md)。需求见 [auth-oauth-requirements.md](auth-oauth-requirements.md)。

## 0. 架构总览

认证域采用「**协议库 + 工具箱 + 服务编排**」三层结构，双包并行（v1 主干与 v2 引擎 fork，见 §1.3）：

```
┌─ 消费层 ─────────────────────────────────────────────────┐
│ CLI 子命令/TUI 斜杠命令 ── SDK 认证门面（v1 链路）         │
│ web/desktop ── REST /oauth/* + /auth（kap-server）        │
│ ACP 宿主 ── terminal-auth 广告 → CLI 登录子进程           │
├─ 服务编排层（进程内）─────────────────────────────────────┤
│ OAuthService（设备码流状态机、单在途流、终态保留/回收）    │
│ AuthSummaryService（就绪门）/ AuthLegacyService（v1 投影）│
│ ManagedAuthFacade（登录→装配管线）                        │
├─ 工具箱层 ───────────────────────────────────────────────┤
│ MirriOAuthToolkit（多 provider 管理、凭证槽推导、装配编排、│
│  用量/资料/反馈的 token 供给）                             │
├─ 协议层（纯 HTTP，无状态）────────────────────────────────┤
│ OAuthManager（设备码驱动、懒刷新、锁）· oauth（三端点）    │
│ identity（设备身份头）· storage（文件令牌库）              │
│ token-state（valid/revoked/missing 三态分类）             │
└──────────────────────────────────────────────────────────┘
```

**分层动机**（抽象问题）：登录是横切能力——CLI、web、desktop、ACP 宿主、遥测都要消费同一份凭证与同一套流状态，若各入口自建会导致凭证槽漂移与状态不一致；因此协议细节（HTTP、锁、重试）下沉为纯函数/无状态模块，跨进程状态（流、装配）收敛在服务编排层，入口只做呈现。

## 1. 双包结构与依赖方向

### 1.1 包分工

| 组件 | 发布名 | 消费方 | 角色 |
|---|---|---|---|
| v1 协议库 | `@mirri-ai/mirri-code-oauth` | v1 引擎服务、CLI SDK 认证门面（TUI/CLI 的 `harness.auth`）、node-sdk | v1 主干 |
| v2 协议库 | `@mirri-ai/v2-oauth` | v2 引擎 auth 域、REST 服务、ACP 服务 | v2 fork，同源演进 |

两包共享设备码协议、令牌存储、三态分类、错误体系的同构实现（接口形状一致）；v2 fork 新增账户资料（`/me`）能力、完整身份头（User-Agent + 设备头）用于登录请求、API key 凭据的错误措辞区分、模型别名合并（刷新保留用户覆盖）等。

### 1.2 依赖方向

协议层不依赖引擎与配置；工具箱层依赖注入的配置适配器（`read/apply/write/remove`，由各宿主桥接到自己的配置面——v1 为 MirriConfig 文件、v2 为分段配置服务）；服务编排层依赖工具箱与 provider/配置/模型服务。**CLI 的 `harness.auth` 即使在 v2 引擎开启时仍走 v1 链路的 SDK 门面**（认证面与引擎面解耦）。

### 1.3 fork 的取舍

v2 需要独立演进（新 userinfo 契约、zod 校验、identity 改革）而 v1 消费面必须冻结，故以 fork 而非单包多入口的方式并行；代价是两份同构实现需人工同步（差异矩阵见 context）。

## 2. 设备码流（RFC 8628）

### 2.1 协议端点

全部 POST form-encoded 到授权主机（默认 `https://auth.kimi.com`，环境变量可覆盖）：

| 端点 | grant | 用途 |
|---|---|---|
| `/api/oauth/device_authorization` | — | 申请设备码（client_id + 设备身份头） |
| `/api/oauth/token` | `urn:ietf:params:oauth:grant-type:device_code` | 轮询授权结果 |
| `/api/oauth/token` | `refresh_token` | 刷新访问令牌 |

客户端 ID 为常量。请求/响应做**必填字段强校验**（设备码三元组；令牌三元组含数值合法性），不满足即抛协议错误而非落盘残缺数据。磁盘/服务端 wire 用 snake_case，进程内类型用 camelCase，转换函数单向成对。

### 2.2 流程驱动（OAuthManager.login）

```
申请设备码 → onDeviceCode 回调（入口拿到展示物）
  循环：轮询令牌
    success → 存凭证槽 → 返回
    denied  → 抛「授权被拒绝」
    expired_token → 跳出内层 → 重新申请设备码（外层循环）
    pending/slow_down → interval += 5（slow_down 时永久） → sleep(interval)
  墙钟预算（15 min）超时 → DeviceCodeTimeoutError
  调用方 AbortSignal → 抛「Login aborted」
```

设备身份头在每次协议请求时惰性求值（工厂注入，便于测试与身份切换）。轮询间隔的 slow_down 语义遵循 RFC 8628 §3.5（至少 +5s 且持续生效）。

### 2.3 REST 编排（服务层 startLogin）

web/desktop 消费面要求「发起即返回展示物、后台继续等授权」：

- **Deferred 捕获**：`onDeviceCode` 回调 resolve 一个 deferred，REST 处理器在拿到设备码的瞬间返回 `OAuthFlowStart`（含流 id、展示物、过期时刻）；
- **fast path 竞速**：登录 promise 若在设备码到来前就 resolve（已登录且令牌有效，工具箱静默刷新路径），直接返回 `{status:'authenticated'}`，不产生设备码；
- **失败前置**：设备码申请本身失败（网络/服务端）时，REST 层返回服务错误、**不产生可查询的流**（客户端观感：发起即报错、无流可轮询；v2 内部仍会登记流表条目但因其无设备码而不可查询，并在终态保留期后回收——v1 则字面上先取设备码再注册流）；
- **单在途流**：每个 provider 至多一个在途流；新流启动前 abort 旧流并置 `cancelled`；
- **终态保留 5 分钟后 GC**（unref 计时器，不因 GC 拖住进程）；provider 配置在登录期间变更时，相关在途流被取消（配置面变更监听失效流）。

流状态机：`pending → authenticated | denied | expired | cancelled`。失败分类：设备码超时 → `expired`；含 "aborted" 的 OAuth 错误 → `cancelled`；其余（含拒绝与未知）→ `denied`，`error_message` 携带原始诊断。

## 3. 令牌生命周期

### 3.1 三态模型（token-state）

凭证槽读出的令牌分为三态：

| 态 | 判定 | 语义 |
|---|---|---|
| `valid` | access_token 非空 | 可用（是否需刷新另行判断） |
| `revoked` | 文件存在但 access_token 为空（墓碑） | 曾登录、被服务端拒绝、需重登 |
| `missing` | 无文件 | 从未登录 |

**动机**：刷新被拒（401）后若直接删文件，新进程会把「凭证被吊销」误读为「从未登录」，无法给出正确的重登引导；墓碑以空令牌记录保留「曾登录」语义，且 wire 格式不变。

### 3.2 懒刷新（ensureFresh）

- **阈值**：`max(300s, expires_in × 0.5)`——长寿命令牌按比例提前刷新，短寿命令牌至少提前 5 分钟；
- **单进程合并**：in-flight promise 携带 force 标志——非强制调用可搭任意在途结果，强制调用只在「在途为非强制」时等其落定后另起（防止强制轮换搭上仍返回旧令牌的便车）；
- **跨进程锁**：以凭证槽为粒度的文件锁（`proper-lockfile`，哨兵文件 + 同名 `.lock` 目录；重试 120 次 × 0.5-1s，5s 判 stale）。拿锁后**重读**凭证：非强制按阈值短路、强制在「他进程已变更令牌」时复用他进程结果；锁不可用时快速失败（fail closed），Windows 与环境变量开关下禁用（降级为仅靠重读的尽力而为协调）；
- **401 竞态恢复**：刷新被拒后 sleep 100ms 重读磁盘——若他进程已轮换出不同 refresh_token，视为竞态，采用他进程令牌（并回报刷新成功）；否则写墓碑、回报 `unauthorized` 后原样上抛；
- **错误分类**：401/403/`invalid_grant` → 不可恢复；429/5xx/网络层 → 指数退避（1s,2s）重试至 3 次后抛可重试错误。

### 3.3 刷新观察者

构造时注入 `onRefresh(outcome)` 同步观察者（success | unauthorized | network_or_other），宿主据此上报遥测；观察者异常被吞（不得影响流）。

## 4. 凭证槽（多环境隔离）

### 4.1 槽位推导

单一推导函数是「环境 → 槽位」的唯一事实源，登录、装配、运行时消费全部经由它：

```
输入：oauthHost（env > 配置 > 默认），baseUrl（env > 显式 > 默认）
输出：默认组合 → 'oauth/mirri-code'（规范槽）
      其它组合 → 'oauth/mirri-code-env-' + sha256({oauthHost, baseUrl})[:16]
```

存储文件名从槽位键净化派生（`oauth/` 前缀剥离；规范槽短名 `mirri-code`；含路径分隔/隐藏前缀的键拒绝）。

### 4.2 运行时 vs 登录期解析

- **运行时**：环境变量覆盖优先；配置槽与期望槽不一致时以期望（环境推导）为准——换环境后不读旧槽；
- **登录期**：显式请求 > 环境变量 > 配置记录；配置槽与期望一致时沿用配置（保留 host 附加信息），否则按环境推导。

### 4.3 环境变量

| 变量 | 作用 |
|---|---|
| `<PRODUCT>_BASE_URL`（`MIRRICODE_BASE_URL`） | 托管服务地址覆盖 |
| `MIRRICODE_OAUTH_HOST`（+ 旧名 `KIMI_OAUTH_HOST` / `MIRRI_OAUTH_HOST`，两包各认其一） | 授权主机覆盖 |
| `MIRRICODE_HOME` | 用户数据根（凭证、设备 ID、配置） |
| `MIRRICODE_DISABLE_OAUTH_LOCK`（v1）/ `MIRRI_DISABLE_OAUTH_LOCK`（v2） | 关闭刷新文件锁 |
| `MIRRICODE_CUSTOM_HEADERS` | 最低优先级出站请求头（`Name: Value` 逐行） |

## 5. 存储

- **路径**：`<数据根>/credentials/<槽名>.json`；
- **权限**：目录 0700（递归建后二次收紧），文件 0600（open 时 + 写后 chmod 双保险）；
- **原子写**：`<目标>.tmp.<pid>.<rand>` → 全量 write → fsync → chmod → rename；失败 unlink 临时文件；
- **读取容错**：IO 错误 / JSON 损坏 / 非对象 → `undefined`；
- **名称净化**：basename 不等、空、隐藏前缀 → 拒绝（防路径逃逸）；
- **枚举**：`list()` 返回目录下全部 `.json` 的去后缀名。

## 6. 设备身份

- **设备 ID**：`<数据根>/device_id`，首用生成 UUID 持久化（0600），失败退化为内存值；首启回调供遥测。
- **身份头**：`X-Msh-Platform / X-Msh-Version / X-Msh-Device-Name / X-Msh-Device-Model / X-Msh-Os-Version / X-Msh-Device-Id`；设备型号 macOS 取 `sw_vers`（1s 超时），值经非 ASCII 清洗，空值回退 `unknown`；产品名/版本为必填（缺失抛错）。
- **User-Agent**：`<product>/<version> (suffix)` 形态；v2 登录请求使用完整默认头（UA + X-Msh-*），OAuth 主机以平台头区分客户端家族、以 UA 后缀区分运行面。
- **宿主声明平台**：各宿主（CLI / SDK / web / desktop）必须显式传平台值，不默认继承。v2 请求头按 vendor 声明分级：仅托管家族声明 `full` 的才获全套设备头，第三方端点只拿 User-Agent——防止设备身份泄漏给非托管端点。

## 7. 工具箱（MirriOAuthToolkit）

多 provider、多槽的工具箱，per `(存储名, 授权主机)` 缓存 OAuthManager 实例：

```
status / login / logout / ensureFresh / getCachedAccessToken / tokenProvider
getManagedUsage / getManagedUserInfo(v2) / submitFeedback / 反馈上传通道
```

- **登录管线**：有令牌 → 静默刷新，被拒 → 自动转设备码流；随后（有配置适配器时）执行**装配**：拉取托管模型目录 → 适配器 apply → 写回配置。装配被拒（401）时重试链：强制刷新 → 再被拒 → 设备码重登 → 再装配（两级回退，详见需求 US-I2）。
- **装配内容**（apply 形状，内容由 model-settings 域定义）：provider 条目（类型、base URL、空 API key + OAuth 引用）、追加式模型别名（不改动既有别名，尊重用户删除黑名单）、搜索/抓取服务引用、默认模型选择（保留有效选择 / 服务端首推，含三态思考强制）。
- **登出清理**：provider 条目、其模型别名、搜索/抓取服务引用、默认模型（指向被移除别名时清空）；思考配置清理仅 v2 登出路径执行（v1 清单不含思考配置）。
- **默认模型保留**：三态思考声明（`only/no/both`）覆盖陈旧的思考开关——always-thinking 模型不允许思考关、非思考模型不允许思考开。
- **tokenProvider**：返回稳定的 `getAccessToken({force})` 提供者，供请求装配层按需取新鲜令牌。

## 8. 服务编排层（v2 auth 域 / v1 services）

### 8.1 OAuthService（IOAuthService）

App 作用域服务，接口面：`startLogin / getFlow / cancelLogin / logout / status / refreshOAuthProviderModels / getManagedUsage / getManagedUserInfo / resolveTokenProvider / getCachedAccessToken`。机制见 §2.3；登录成功的 finalize 顺序：provision provider（写入 provider 配置）→ best-effort 模型目录刷新 → 置 `authenticated`（provision 失败仅告警，不推翻认证结论）。登出顺序（v2）：工具箱登出（删凭证）→ 取消在途流 → deprovision（分节清理配置，无变更零写）；v1 为先取消在途流再登出。

### 8.2 AuthSummaryService / AuthLegacyService

- `summarize()`：枚举全部带 OAuth 引用的 provider 逐个 `status()`（失败跳过并告警）；
- `ensureReady(modelOverride?)`：就绪门（重读配置 → 无 provider 抛「完成登录或配置 provider」→ 无默认模型抛「模型未解析」→ 凭据解析，有即通过，oauth 无缓存令牌抛「provider 无凭证」）。凭据解析链**两代形状不同**：v2 为「model 级 apiKey / model 级 oauth / provider 级 apiKey（含环境包推导，冲突抛错）/ provider 级 oauth」；v1 为**静态门**——仅 provider 级 apiKey（不探测环境变量、不做在线刷新）与 provider 级 oauth 缓存令牌，无 model 级层级；
- AuthLegacy：将 v2 provider/model/凭证状态投影为 v1 `AuthSummary` wire（`ready` 判定含托管 revoked 不可就绪），供 REST `GET /auth` 首屏探测。

### 8.3 REST 面（kap-server `/api/v1`）

```
POST   /oauth/login     startLogin → OAuthFlowStart（pending | authenticated 快速路径）
GET    /oauth/login     getFlow → 快照 | null
DELETE /oauth/login     cancelLogin（幂等）
POST   /oauth/logout    logout
GET    /oauth/usage     用量（camelCase 域模型 → snake_case wire 映射）
GET    /oauth/userinfo  账户资料（camelCase 契约直出）
GET    /auth            v1 就绪投影（AuthLegacy）
```

流协议 DTO 以 zod schema 为共享契约（transports 校验同一份定义）。

### 8.4 错误域

`auth.*` 错误码族（login_required / provisioning_required / token_missing / token_unauthorized / model_not_resolved），均公开、不可重试、附行动建议文案；协议库错误类（OAuthError 族）→ SDK 层映射为 `auth.login_required` / `provider.connection_error`，未识别错误**原样上抛**（防误导）。

## 9. 请求期凭据注入

- **v1**：provider-manager 在构造模型客户端时解析凭据——provider 带 oauth 引用时经注入的 `OAuthTokenProvider` 取令牌（401 时可选强制刷新重试一次）；apiKey 与 oauth 并存直接抛配置冲突。
- **v2**：`resolveModelAuthMaterial` 纯函数按「model.apiKey → model.oauth → provider.apiKey（配置或环境包推导）→ provider.oauth」优先级解析，同名冲突抛错，解析轨迹可记录（ResolutionTrace）；模型装配层的 token 端口（`IModelOAuthTokens`）委托 IOAuthService，无 provider 时抛 `auth.login_required`。

## 10. 机制与需求映射

| 需求 Epic | 承载机制（本文） |
|---|---|
| A 登录入口 | §2.2/§2.3 + §1 CLI/REST/ACP 面 |
| B 设备码协议 | §2.1/§2.2 |
| C 存储 | §5 + §4 凭证槽 |
| D 设备身份 | §6 |
| E 刷新/过期/多进程 | §3 |
| F 状态呈现/就绪门 | §8.2/§8.4 + §9 |
| G API key | §7 装配（开放平台路径）+ §9 |
| H 登出 | §8.1 deprovision + §7 |
| I 装配契约 | §7 登录管线 |
| J userinfo/usage | §7 工具箱 + §8.3 |
| K 非功能 | §2 超时、§3 并发、§5 权限、§6 身份诚实 |

## 11. ADR 记录

- **ADR-001（2026-09-12）**：文档初始架构描述（无设计变更，仅基线记录）。
