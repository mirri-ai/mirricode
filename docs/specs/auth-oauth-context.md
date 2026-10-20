# 认证（auth-oauth）现状盘点

> **快照日期：2026-10-12**。本文是认证域的代码事实清单与覆盖差异矩阵，随现状变化更新；不含目标与裁决（设计文档单向引用本文）。文档基线 = `auth-oauth-requirements.md` + `auth-oauth-architecture.md`。
>
> **2026-10-13 更新（TS 退役收尾）**：`packages/oauth`（npm `@mirri-ai/mirri-code-oauth`）与 `packages/kimi-code-oauth`（npm `@mirri-ai/v2-oauth`）已删除，TS 侧 managed OAuth 全链（§0–§10 所列工具库、引擎服务、REST 面、CLI/TUI/web 消费端与 SDK 门面）全部退役；下文各表为退役前现状的历史记录（文件:行引用保留不改），不再反映当前代码。当前仅存面：v1 wire 契约 `packages/protocol/src/rest/auth.ts` 的 `managed_provider` 字段（wire-compat 缓释保留）。Rust 侧定位与进度见 `rust-core-design` 计划与 `auth-oauth-ROADMAP.md`。

## 0. 源码地图

| 区域 | 位置 | 说明 |
|---|---|---|
| v1 协议/工具箱库 | `packages/oauth`（npm `@mirri-ai/mirri-code-oauth` v0.2.2） | CLI/TUI、v1 引擎、node-sdk 消费 |
| v2 协议/工具箱库 | `packages/kimi-code-oauth`（npm `@mirri-ai/v2-oauth` v0.4.0） | v2 引擎、kap-server、acp-server 消费；同源 fork |
| v1 引擎服务 | `packages/agent-core/src/services/{auth,oauth,authSummary}` | daemon 侧编排（v1） |
| v2 引擎服务 | `packages/agent-core-v2/src/app/auth/` + `app/authLegacy/` | v2 主实现（**主基线**） |
| REST 面 | `packages/kap-server/src/routes/{oauth,auth}.ts` | `/api/v1/oauth/*`、`/api/v1/auth` |
| v1 wire 契约 | `packages/protocol/src/rest/auth.ts` | AuthSummary schema |
| CLI/TUI | `apps/mirri-code/src/{cli/sub/login*,cli/sub/acp*,tui/commands/auth.ts,tui/components/chrome/*}` | 登录/登出 UX |
| web | `apps/mirri-web/src/{components/dialogs/LoginDialog.vue,api/daemon/*,composables/useAuthGate.ts}` | 登录对话框与门禁 |
| desktop | `apps/mirri-desktop/src/main/` | Electron 外壳（无自有认证逻辑） |
| ACP 广告 | `packages/acp-server/src/auth-methods.ts` | terminal-auth 方法 |
| SDK 门面 | `packages/node-sdk/src/auth.ts`、`oauth-error.ts` | CLI `harness.auth` 实现 |

**关键结论**：CLI/TUI 的 `harness.auth` 即便在 v2 引擎开启时仍走 v1 链路（`packages/node-sdk/src/sdk-rpc-client-v2.ts:357` 委托 `MirriAuthFacade`，基于 `@mirri-ai/mirri-code-oauth`）；web/desktop 走 v2 REST。

## 1. 设备码协议（RFC 8628）

| 事实 | 位置 |
|---|---|
| 三端点（device_authorization / token×2 grant），POST form，无状态 | `packages/oauth/src/oauth.ts:1-11`（v2 同构 `kimi-code-oauth/src/oauth.ts`） |
| OAuth host 默认 `https://auth.kimi.com`，client_id `17e5f671-d194-4dfb-9706-5516cb48c098` | `packages/oauth/src/constants.ts:3-12`；v2：`kimi-code-oauth/src/constants.ts` |
| env 覆盖：v1 认 `MIRRICODE_OAUTH_HOST`/`KIMI_OAUTH_HOST`；v2 认 `MIRRICODE_OAUTH_HOST`/`MIRRI_OAUTH_HOST` | 同上两文件 |
| 单请求超时 30s（`DEFAULT_HTTP_TIMEOUT_MS`），`AbortSignal.timeout` 与调用方 signal 合成 | `packages/oauth/src/oauth.ts:57,66-70` |
| fetch 失败 cause 链展开为可读消息（不显示裸 "fetch failed"） | `packages/oauth/src/oauth.ts:106-115` |
| 响应强校验：设备码三元组（user_code/device_code/verification_uri_complete）缺失即抛 | `packages/oauth/src/oauth.ts:137-148` |
| 令牌三元组（access_token/refresh_token/expires_in>0）缺失/非法即抛，不落盘残缺 | `packages/oauth/src/oauth.ts:29-54` |
| 轮询分类：pending/slow_down（v2 同）；expired_token；access_denied；5xx 抛；其余抛 | `packages/oauth/src/oauth.ts:184-211` |
| 错误信息提取（error_description/message/detail/嵌套 error/errors 数组，择深） | `packages/oauth/src/api-error.ts:6-42` |
| 设备码流驱动：interval 默认 5s；slow_down 永久 +5s；expired_token 外层重新出码；15min 墙钟预算；AbortSignal → "Login aborted by caller" | `packages/oauth/src/oauth-manager.ts:400-457,466-470` |
| login 保存 token 到存储槽后返回 | `packages/oauth/src/oauth-manager.ts:428-430` |
| 设备头惰性注入（工厂） | `packages/oauth/src/oauth-manager.ts:130-148` |

## 2. 令牌存储与凭证槽

| 事实 | 位置 |
|---|---|
| 存储：`<home>/credentials/<name>.json`，目录 0700（建后 chmod 收紧）、文件 0600（open+写后双 chmod） | `packages/oauth/src/storage.ts:41-57,90-116` |
| 原子写：`<target>.tmp.<pid>.<rand>` → write → fsync → rename；失败 unlink | `packages/oauth/src/storage.ts:93-116` |
| 读容错：IO/JSON/结构 → undefined；list() 枚举 | `packages/oauth/src/storage.ts:72-88,129-137` |
| 名称净化（basename 不等/空/隐藏前缀拒绝） | `packages/oauth/src/storage.ts:59-70` |
| wire snake_case ↔ camelCase 转换 | `packages/oauth/src/types.ts:57-87` |
| 三态分类 valid/revoked/missing + 墓碑构造（access_token 置空） | `packages/oauth/src/token-state.ts:23-45` |
| 凭证槽推导：默认组合 → `oauth/mirri-code`；其它 → `oauth/mirri-code-env-<sha256[:16]>` | `packages/oauth/src/managed-mirri-code.ts:291-313` |
| 槽位单一事实源注释（登录/装配/运行时同源） | `packages/oauth/src/managed-mirri-code.ts:315-333` |
| 运行时解析（env 优先，配置槽不一致以期望为准）/ 登录期解析（请求 > env > 配置） | `packages/oauth/src/managed-mirri-code.ts:335-390` |
| 存储名净化（`oauth/` 前缀剥离，非法键抛） | `packages/oauth/src/toolkit.ts:440-454` |
| `MIRRICODE_HOME` 覆盖数据根（缺省 `~/.mirri-code`） | `packages/oauth/src/toolkit.ts:456-460` |
| `storage: 'file' | 'keyring'` 类型面存在，**仅 file 实现**（OAuthStorageBackend = 'file'） | `packages/oauth/src/managed-mirri-code.ts:73-83`；`packages/oauth/src/types.ts:11` |

## 3. 刷新与多进程

| 事实 | 位置 |
|---|---|
| 懒刷新阈值 `max(300s, expires_in × 0.5)` | `packages/oauth/src/oauth-manager.ts:27-36` |
| 单进程 in-flight 合并（force 语义：非强制可搭车；强制等非强制落定后另起） | `packages/oauth/src/oauth-manager.ts:111-119,254-282` |
| 跨进程锁：proper-lockfile，`{configDir}/oauth/{name}.lock`；120×0.5-1s 重试，5s stale；fail closed | `packages/oauth/src/oauth-manager.ts:175-235,308-314` |
| 锁后重读：非强制阈值短路；强制在他进程已变更时复用 | `packages/oauth/src/oauth-manager.ts:316-355` |
| 401 竞态恢复：sleep 100ms 重读，refresh_token 已变 → 采用他进程结果并回报成功 | `packages/oauth/src/oauth-manager.ts:368-383` |
| 无恢复 → 写墓碑 + 回报 unauthorized | `packages/oauth/src/oauth-manager.ts:384-389` |
| 401/403/invalid_grant → OAuthUnauthorizedError；429/500/502/503/504 → 指数退避 1s/2s ×3 | `packages/oauth/src/oauth.ts:215-290`（`RETRYABLE_STATUSES` :23） |
| Windows / `MIRRICODE_DISABLE_OAUTH_LOCK=1`（v2: `MIRRI_DISABLE_OAUTH_LOCK`）跳过锁 | `packages/oauth/src/oauth-manager.ts:182-187` |
| `onRefresh` 观察者（异常吞掉） | `packages/oauth/src/oauth-manager.ts:166-173` |
| 用户可见错误文案：`No token for "{name}". Run /login to authenticate.` / `Stored token … was rejected; re-login required.` / `Token … has no refresh_token; re-login required.` | `packages/oauth/src/oauth-manager.ts:284-297,329-331,357-360` |
| 错误类族：OAuthError / OAuthUnauthorizedError / OAuthConnectionError / DeviceCodeExpiredError / DeviceCodeTimeoutError / RetryableRefreshError | `packages/oauth/src/errors.ts:18-56` |

## 4. 设备身份

| 事实 | 位置 |
|---|---|
| 设备 ID：`<home>/device_id`，UUID 持久化 0600，首启回调，失败退化内存 | `packages/oauth/src/identity.ts:35-68` |
| X-Msh-* 头组装；hostname/release/deviceModel；非 ASCII 清洗，空回退 `unknown` | `packages/oauth/src/identity.ts:70-82,149-173` |
| macOS 型号 `sw_vers -productVersion`（1s 超时） | `packages/oauth/src/identity.ts:158-168` |
| 平台常量：v1 包 `MIRRICODE_PLATFORM='kimi_code_cli'`；v2 包 `'mirri_code_cli'` | `packages/oauth/src/identity.ts:18`；`kimi-code-oauth/src/identity.ts`（diff 确认） |
| v2 identity 要求宿主显式传 `platform`（必填非空 ASCII，无默认） | `kimi-code-oauth/src/identity.ts`（toolkit diff :23-29 注释） |
| User-Agent：`product/version (suffix)`；v2 登录请求用完整默认头（UA+X-Msh-*，web 后缀 `(web)`） | `packages/oauth/src/identity.ts:84-106`；`kimi-code-oauth/src/toolkit.ts:425-431` |
| `MIRRICODE_CUSTOM_HEADERS` 最低优先级出站头 | `packages/oauth/src/identity.ts:108-138` |
| v2 请求头分级：vendor 声明 `hostHeaders: 'full'` 才获全套 X-Msh-* | `packages/agent-core-v2/src/kosong/model/hostRequestHeaders.ts:1-13` |
| CLI 侧：`userAgentProduct: 'mirri-code-cli'`、`CLI_PLATFORM='mirri_code_cli'`（注释要求与 v2-oauth 匹配） | `apps/mirri-code/src/constant/app.ts:8,19`；`apps/mirri-code/src/cli/version.ts:58-63` |
| v2 SDK 自报平台 `'mirri_code_sdk'` | `packages/node-sdk/src/sdk-rpc-client-v2.ts:364-379` |

## 5. v2 引擎 auth 域（主基线）

| 事实 | 位置 |
|---|---|
| IOAuthService / IOAuthToolkit / IAuthSummaryService 契约 | `packages/agent-core-v2/src/app/auth/auth.ts:41-90` |
| 错误类与文案（provisioning/token_missing/model_not_resolved） | `packages/agent-core-v2/src/app/auth/auth.ts:92-136` |
| `auth.*` 错误码域（5 码，public + action） | `packages/agent-core-v2/src/app/auth/errors.ts:7-47` |
| OAuthService：flowId=`oauth_<uuid>`；单在途（abort+cancelled 后新建）；终态保留 5min 后 GC（unref） | `packages/agent-core-v2/src/app/auth/authService.ts:84-100,122-146,239-262,441-447` |
| fast path：已登录时 toolkit.login 无设备码直接 resolve → 竞速返回 `authenticated` | `packages/agent-core-v2/src/app/auth/authService.ts:170-215` |
| 登录失败分类 classifyFailure（timeout→expired；aborted→cancelled；其余→denied） | `packages/agent-core-v2/src/app/auth/authService.ts:668-674` |
| finalize 顺序：provision provider（type 'kimi', apiKey ''，oauth ref）→ best-effort 目录刷新 → authenticated（失败仅告警） | `packages/agent-core-v2/src/app/auth/authService.ts:461-514` |
| provider 配置变更取消相关在途流 | `packages/agent-core-v2/src/app/auth/authService.ts:449-459` |
| 登出 deprovision：分节清理（providers/models/services/defaultModel/thinking），无变更零写 | `packages/agent-core-v2/src/app/auth/authService.ts:233-242,516-544` |
| 目录刷新 refreshOAuthProviderModels：refreshChain 串行化；结果分类 changed/unchanged/failed；发布 `event.model_catalog.changed` | `packages/agent-core-v2/src/app/auth/authService.ts:292-382` |
| ensureReady 就绪门（三错误类；凭据解析 apiKey/oauth） | `packages/agent-core-v2/src/app/auth/authService.ts:623-665` |
| OAuthToolkitService extends MirriOAuthToolkit（bootstrap homeDir + clientIdentity） | `packages/agent-core-v2/src/app/auth/authService.ts:857-865` |
| 流协议 zod schema（status 五态；usage/userinfo 契约） | `packages/agent-core-v2/src/app/auth/oauthProtocol.ts:15-148` |
| AuthLegacy：v1 AuthSummary 投影（ready 判定含托管 revoked 不可就绪） | `packages/agent-core-v2/src/app/authLegacy/authLegacyService.ts:33-64` |
| 请求期凭据解析 resolveModelAuthMaterial（model.apiKey→model.oauth→provider.apiKey/env→provider.oauth，冲突抛错） | `packages/agent-core-v2/src/kosong/model/modelAuth.ts:49-97` |
| 模型装配 token 端口（IModelOAuthTokens；无 provider 抛 auth.login_required） | `packages/agent-core-v2/src/kosong/model/modelOAuth.ts:16-28`；`app/kosongConfig/oauthTokenAdapter.ts:18-40` |
| v1 wire：ManagedProviderStatus 枚举含 expired/revoked（**实现只产生 authenticated/unauthenticated**） | `packages/protocol/src/rest/auth.ts:12-18`；v1/v2 两服务实现 |

## 6. v1 引擎服务（v1-source）

| 事实 | 位置 |
|---|---|
| managedAuth 门面（toolkit + 配置适配器：read/write/apply/remove 桥 MirriConfig） | `packages/agent-core/src/services/auth/managedAuth.ts:48-67` |
| 登录：resolveLoginAuth → toolkit.login（provisionConfig: true）；结果缺 provision 抛错 | `packages/agent-core/src/services/auth/managedAuth.ts:68-96` |
| OAuthService（v1）：REST 化设备码流（deferred 捕获、单在途、5min GC、错误分类）；flowId=`oauth_<ulid>` | `packages/agent-core/src/services/oauth/oauthService.ts:39-304`；接口 `oauth.ts:73-104` |
| AuthSummaryService（v1）：GET /v1/auth 投影；就绪门（静态，不探测在线刷新）；reload: true 强制重读（否则登录后 ready 永不翻转） | `packages/agent-core/src/services/authSummary/authSummaryService.ts:36-121` |
| provider-manager：oauth 引用 → OAuthTokenProvider 注入；apiKey+oauth 并存抛互斥错；401 强刷重试 | `packages/agent-core/src/session/provider-manager.ts:162-225` |
| CLI harness.auth 实现（v1 链路） | `packages/node-sdk/src/auth.ts:1-26`；`packages/node-sdk/src/sdk-rpc-client-v2.ts:357` |
| 协议错误映射：OAuthUnauthorizedError → AUTH_LOGIN_REQUIRED（`OAuth provider "{name}" requires login…`）；连接 → provider.connection_error；未识别原样上抛 | `packages/node-sdk/src/oauth-error.ts:25-41` |

## 7. REST 面（kap-server）

| 事实 | 位置 |
|---|---|
| 路由：POST/GET/DELETE `/oauth/login`、POST `/oauth/logout`、GET `/oauth/usage`、GET `/oauth/userinfo`（前缀 `/api/v1`） | `packages/kap-server/src/routes/oauth.ts:4-8,69-201` |
| 全部委托 v2 IOAuthService；usage 做域→wire snake_case 映射（extra_usage booster 钱包） | `packages/kap-server/src/routes/oauth.ts:81,103,124,149,171,192,204-243` |
| `GET /auth` → AuthLegacy 投影（200 恒返回，就绪与否在 data 内） | `packages/kap-server/src/routes/auth.ts:32-46` |
| 服务访问鉴权（**非本域**）：bearer/密码中间件、40101、`--dangerous-bypass-auth` | `packages/kap-server/src/middleware/auth.ts`、`src/services/auth/authTokenService.ts:1-16` |

## 8. CLI / TUI 消费端

> **2026-10-12 更新（SDR-004）**：managed login 全线退役，本表下方涉及 `/login`、`/logout`、CLI `mirri login`、ACP terminal-auth、平台选择器、设备码盒、`OAUTH_LOGIN_REQUIRED_STARTUP_NOTICE` 与启动登录门的行仅作退役前现状的历史记录，对应源码已删除；常驻文案现指向 `/provider`。

| 事实 | 位置 |
|---|---|
| `mirri login` 子命令（无参，"Authenticate with Mirri Code via the device-code flow."） | `apps/mirri-code/src/cli/sub/login.ts:13-19`；挂载 `cli/commands.ts:90` |
| runLoginFlow：SIGINT→abort；**先打印回退再开浏览器**；stderr 文案四行；成功 `Logged in to {provider}.` exit 0；取消 `Login cancelled.` / 失败 `Login failed: {msg}` exit 1 | `apps/mirri-code/src/cli/sub/login-flow.ts:13-62` |
| openUrl：darwin `open` / win32 `cmd /c start` / `xdg-open`，fire-and-forget | `apps/mirri-code/src/utils/open-url.ts:3-11` |
| ACP `--login`（acp/acp-native/acp-legacy 三入口同流）；`MIRRICODE_HOME` 转发进 authMethods[0].env；argv[1] 作 legacy _meta 命令 | `apps/mirri-code/src/cli/sub/acp-native.ts:27-63`；`acp.ts:11-20` |
| terminal-auth 广告（一等 type:'terminal' args:['--login'] + _meta 回退 {command,args:['login'],env}） | `packages/acp-server/src/auth-methods.ts:31-70` |
| TUI `/login`（别名 `login-with-kimi`）与 `/logout`（别名 disconnect）注册 | `apps/mirri-code/src/tui/commands/registry.ts:371-382` |
| 平台选择器：'Select a platform'；`Mirri Code (OAuth)` + OPEN_PLATFORMS | `apps/mirri-code/src/tui/components/dialogs/platform-selector.ts:5-19` |
| OAuth 登录流（status 预查 alreadyLoggedIn；onDeviceCode→showLoginAuthorizationPrompt；refreshConfigAfterLogin 失败提示；遥测 login{method:oauth}；Already logged in 提示） | `apps/mirri-code/src/tui/commands/auth.ts:43-100` |
| 设备码盒（圆角边框、title/hint/URL/code）+ 等待 spinner（`Waiting for authorization…`；✓/✗ 收尾） | `apps/mirri-code/src/tui/components/chrome/device-code-box.ts:21-78`；`mirri-tui.ts:2213-2252` |
| API key 登录：掩码输入、验证 fetchOpenPlatformModels、401 引导 hint、模型选择、`Setup complete:` | `apps/mirri-code/src/tui/commands/auth.ts:102-180`；`tui/commands/prompts.ts:113-117` |
| `/logout`：枚举（OAuth 残留 + API key providers）、'Nothing to logout.'、当前 provider 登出→refreshConfigAfterLogout+clearActiveSessionAfterLogout（closeSession 'logged out'）、`Logged out from {label}.` | `apps/mirri-code/src/tui/commands/auth.ts:182-242`；`tui/controllers/auth-flow.ts:105-115` |
| Ctrl-C 取消：cancelInFlight 挂钩，editor onCtrlC 优先消费 | `apps/mirri-code/src/tui/controllers/editor-keyboard.ts:123-130` |
| 欢迎页警示（`Run /login or /provider to get started.` 等） | `apps/mirri-code/src/tui/components/chrome/welcome.ts:28-77` |
| 常驻文案两常量（LLM_NOT_SET / NO_ACTIVE_SESSION，均指向 `/provider`；`OAUTH_LOGIN_REQUIRED_STARTUP_NOTICE` 已随 SDR-004 删除） | `apps/mirri-code/src/tui/constant/mirri-tui.ts:5-6` |
| 会话 401：OAUTH_LOGIN_REQUIRED_CODE → showError 提示；启动失败 → enterLoginRequiredStartupState | `apps/mirri-code/src/tui/controllers/session-event-handler.ts:852-855`；`mirri-tui.ts:770-774` |
| headless 无模型报错文案 | `apps/mirri-code/src/cli/run-prompt.ts:377-384` |
| oauth_refresh 遥测（success / error+reason） | `apps/mirri-code/src/cli/run-prompt.ts:120-126`；`run-shell.ts:64-73` |
| 托管 provider 不进 provider 管理器（只经登录/登出管理） | `apps/mirri-code/src/tui/components/dialogs/provider-manager.ts:10-11` |
| 登录后配置刷新（refreshConfigAfterLogin / replaceConfigSections 单次原子写） | `apps/mirri-code/src/tui/controllers/auth-flow.ts:117-227` |

## 9. web / desktop 消费端

| 事实 | 位置 |
|---|---|
| LoginDialog 状态机 starting/device-code/success/expired/error；props 注入三回调 | `apps/mirri-web/src/components/dialogs/LoginDialog.vue:35-64` |
| 主路径新标签页开 verificationUriComplete；回退裸 URI + 等宽设备码 + Copy/Copied；**无二维码** | `LoginDialog.vue:231-267,421-428` |
| 轮询 interval 秒；连续 3 次失败（MAX_CONSECUTIVE_POLL_FAILURES=3）→ pollError 失联态 | `LoginDialog.vue:88,148-181` |
| authenticated → 1.2s 后自动关闭并 emit success | `LoginDialog.vue:166-172` |
| 关闭对话框 best-effort 取消 | `LoginDialog.vue:200-207` |
| REST 客户端方法与 wire DTO | `apps/mirri-web/src/api/daemon/client.ts:1527-1595`；`wire.ts:606-638` |
| **web 不消费 /oauth/userinfo 与 /oauth/usage**（端点已提供、UI 未接） | 全 src 无引用（recon 确认） |
| checkAuth 三态 proceed/retry/server-auth-required；首载重试 2000ms（防瞬时失败误判未登录） | `apps/mirri-web/src/composables/client/useWorkspaceState.ts:461-505` |
| auth gate：!authReady 整页登录页 + history.replaceState('/login') + 回原址 | `apps/mirri-web/src/composables/useAuthGate.ts:17-47` |
| 登录入口三处 + 登录成功 checkAuth()+load() + 登出流 | `App.vue:677,811-814,590-595,1109-1235`；`apps/mirri-web/src/composables/client/useWorkspaceState.ts:2589-2599` |
| Account 标签只显示 provider 名/默认模型/登出（无头像用户名用量）；managedProviderStatus 无 UI 消费 | `apps/mirri-web/src/components/settings/SettingsView.vue:492-505`；`apps/mirri-web/src/composables/client/useWorkspaceState.ts:467` |
| 中英双语文案（login/authPage/sidebar） | `apps/mirri-web/src/i18n/locales/{en,zh}/{login,app,sidebar}.ts` |
| **服务访问凭证（非本域，边界声明用）**：URL fragment token 7 天 TTL、401/40101 弹 ServerAuthDialog、dangerousBypassAuth | `apps/mirri-web/src/api/daemon/serverAuth.ts:21,34-51,278-287`；`http.ts:20,385-400` |
| desktop：Electron 外壳无认证逻辑；消费 `mirri web`（v2）；server.token 以 `#token=` 注入；授权页经 shell.openExternal 系统浏览器打开；无 OAuth 回调深链 | `apps/mirri-desktop/src/main/index.ts:119-160,203-209`；`ensure-server.ts:266-332`；`deep-link.ts:11-47` |

## 10. 用量 / 账户资料 / 反馈（工具箱附带能力）

| 事实 | 位置 |
|---|---|
| 用量：`GET {base}/usages`，8s 超时；宽松解析（used/remaining、reset_at 多拼写）；booster 钱包（分单位） | `packages/oauth/src/managed-usage.ts:37-302` |
| 401 hint `'Authorization failed. Please check your API key (try /login).'`；404 `'Usage endpoint not available. Try Kimi For Coding.'` | `packages/oauth/src/managed-usage.ts:281-289` |
| 默认 base URL：v1 包 `https://api.kimi.com/coding/v1`；v2 包 `https://api.mirricode.com/coding/v1` | 两包 `managed-usage.ts`（recon diff 确认） |
| userinfo（**仅 v2 包**）：`GET {base}/me`，8s 超时，lenient 解析，camelCase zod 契约 | `packages/kimi-code-oauth/src/managed-userinfo.ts:45-202` |
| toolkit getManagedUserInfo（v2） | `kimi-code-oauth/src/toolkit.ts:311-333` |
| 反馈提交/上传（token 供给面） | `packages/oauth/src/managed-feedback.ts`、`managed-feedback-upload.ts`；`toolkit.ts:305-370` |

## 11. 覆盖差异矩阵（文档 vs TS 现状）

| # | 能力 | 文档覆盖 | TS 现状 | 差异定性 |
|---|---|---|---|---|
| 1 | 设备码流全链（B） | ✅ | 完整 | 一致 |
| 2 | 凭证槽多环境隔离（C.5-C.7） | ✅ | 完整 | 一致 |
| 3 | 懒刷新 + 锁 + 墓碑（E） | ✅ | 完整 | 一致 |
| 4 | REST 流编排 + fast path（A5） | ✅ | v2 完整 | 一致 |
| 5 | CLI/TUI/web 登录登出 UX（A1-A3, H） | ✅ | 完整 | 一致 |
| 6 | userinfo/usage 端点（J） | ✅ 引擎级 | v2 引擎+REST 有；**web UI 未消费** | 回归暴露面：端点可用但无消费方（文档按引擎级记录，UI 消费列为增强提案 P-5） |
| 7 | managed_provider.status 四态（F.1） | ✅（注明保留位） | wire 枚举四态，实现只产两态 | 契约宽于实现（记录，不算回归） |
| 8 | keyring 存储 | 提案 P-1 | 类型面预留、无实现 | 提案 |
| 9 | 授权码回调流 / 二维码 / 后台预刷新 / 多账户 | 提案 P-2/P-3/P-4/P-6 | 无 | 提案 |
| 10 | MCP 服务器 OAuth | 边界声明 | 存在于 `agent-core-v2/src/mcpCore/oauth/*`（归 mcp 域） | 边界正确划出 |
| 11 | 服务访问鉴权 | 边界声明 | kap-server middleware + web serverAuth | 边界正确划出 |

## 12. 现状缺口与不一致（代码观察，未修复）

1. **命令名文案漂移（已收敛，2026-10-12，SDR-003）**：历史现象——内置命令 `/login-with-kimi`（无别名）与 `welcome.ts:35-77`、`config.ts:404`、`run-prompt.ts:381`、oauth 包错误消息（`oauth-manager.ts:284`）多处 `/login` 文案漂移，用户敲 `/login` 会落到消息分支发给模型。现状：内置命令收敛为 `/login`（别名 `login-with-kimi`，`registry.ts:377-382`），常驻文案三常量（`mirri-tui.ts:5-10`）与错误消息统一指向 `/login`。→ 需求 P-7（已关闭，SDR-003）。
2. **ServerAuthDialog 密码变量名不一致**：文案 `MIRRICODE_CODE_PASSWORD`（`ServerAuthDialog.vue:44-69`）vs 实现注释 `MIRRICODE_PASSWORD`（`serverAuth.ts`）。→ 需求 P-8。
3. **v1/v2 平台标识不一致**：v1 oauth 包设备头 `X-Msh-Platform: kimi_code_cli`，v2 包 `mirri_code_cli`；CLI 常量 `CLI_PLATFORM='mirri_code_cli'`（app.ts:19 注释要求与 v2-oauth 匹配）。同一授权主机会看到两种平台值。
4. **默认 base URL 不一致**：v1 包 `api.kimi.com/coding/v1` vs v2 包 `api.mirricode.com/coding/v1`（同槽场景 `SHARED_DEFAULT_BASE_URLS` 仅收编 v1 默认，v2 fork 的默认值不在收编集内——跨包场景凭证槽可能分裂，v1/v2 各自内部一致）。
5. **env 旧名分裂**：v1 认 `KIMI_OAUTH_HOST`、v2 认 `MIRRI_OAUTH_HOST`（同组新名 `MIRRICODE_OAUTH_HOST` 两包都认）。
6. **web 未消费 userinfo/usage**（见矩阵 #6）。
7. **web 不识别 denied 流状态**：REST 快照状态枚举含 `denied`（`oauthProtocol.ts`），web 轮询 wire 类型只认四态（`apps/mirri-web/src/api/daemon/wire.ts:630-633`），对话框 `else` 分支继续轮询（`LoginDialog.vue:174-178`）——拒绝最终以「守护进程失联」收场；同类：服务端换码后 web 对话框不刷新已展示设备码（`flow.value` 仅在 POST 响应时渲染一次）（需求 A3.5b/B.5 / 提案 P-9）。
8. **v1 GET /v1/auth 的 `reload: true` 依赖**：登录后 ready 翻转依赖显式重读（`authSummaryService.ts:112-121` 注释）；v2 经 providerService.ready 消除该坑。

## 13. v1-source 能力标记

以下能力由 v1 链路承载（v2 无对应或形态不同），按 skill 规则标记：

- `[v1][cli]` CLI/TUI 全部登录登出 UX（US-A1/A2/G/H 的 TUI 呈现）——`harness.auth` = node-sdk `MirriAuthFacade`（`@mirri-ai/mirri-code-oauth`）。
- `[v1][cli]` provider-manager 的 OAuthTokenProvider 401 强刷重试（`provider-manager.ts:181-198`）。
- `[v1][daemon]` v1 OAuthService（ulid flowId / DisposableMap 流表）——kap-server 已切 v2，v1 服务仅存于 agent-core，无活跃 REST 挂载方。
- `[v1][cli]` 开放平台（API key）登录的模型目录装配细节在 TUI 命令内完成（v2 需另经 provider 服务）。

其余（设备码协议、存储、刷新、凭证槽、REST 面、web 登录、v2 auth 域）为 v2 主基线或双包同构。
