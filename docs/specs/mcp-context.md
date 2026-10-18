# MCP 域现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-08（全节刷新；2026-09-04 快照中的部分论断已随 P1/P2 与热更新内核落地失效）·
> 配套设计文档：[mcp-architecture.md](mcp-architecture.md)（RFC）· [mcp-requirements.md](mcp-requirements.md)（US）· [mcp-settings-page-requirements.md](mcp-settings-page-requirements.md)（页面需求）· 执行状态与开放项：[mcp-ROADMAP.md](mcp-ROADMAP.md)（唯一状态家）·
> 本文不描述目标、不含裁决——终态与裁决在设计文档。

## 1. 配置源与合并（已存在）

| 源 | 位置 | 层级 |
|---|---|---|
| user | `$MIRRICODE_HOME/mcp.json` | 最低 |
| project-root | `<git-root>/.mcp.json` | 中 |
| project | `<cwd>/.mirri-code/mcp.json` | 最高 |
| plugin | 插件 manifest 的 `mcpServers` 字段 | 追加 |

合并优先级 `user < project-root < project`（`agent-core-v2/src/workspace/workspaceMcpConfig/internal/config-loader.ts:7`；project 层锚定 cwd，`config-loader.ts:37-54`）。per-server 配置字段：`enabled`、`enabledTools`、`disabledTools`、`startupTimeoutMs`、`toolTimeoutMs`、transport（stdio/sse/http）。

plugin 层现状（`agent-core-v2/src/app/plugin/manager.ts`、`rust/crates/mirri-plugin-host/src/manager.rs`）：插件 server 以运行时名 `plugin-<pluginId>:<serverName>` 进入统一 server 集（`enabledMcpServers()`），与 user/project 同名天然不冲突。**per-server 启用总开关与工具级过滤均已实现**（P1 批次 3，2026-09-04）：TS `setMcpServerToolFilter` 写 `$MIRRICODE_HOME/plugins/installed.json` 的 capability state（`manager.ts:224-259`，完整状态语义：present 替换、absent 清空），rust 对应（`manager.rs:503-516`）；经 `mcp.config.setToolFilter` 的 plugin 分支消费（`mcp_config_handlers.rs:350-383`，写后 `sync_plugin_mcp_connection` 热跟随）。manifest 永不落用户改动。

环境变量展开现状：配置字符串值支持 `${VAR}` / `${env:VAR}` 语法、未定义展开为空串、单遍不递归——双端已实现且语义一致（TS `workspaceMcpConfig/internal/env-expand.ts`，rust `mirri-mcp/src/config.rs:382`）。两个已知差异/缺口：① rust 在解析前的原始 JSON 文本上展开（键名含 `${...}` 的极端场景也会被展开，TS 为值级展开、键不动）；② **插件 manifest 的 mcpServers 双侧均不展开**（TS `manager.ts` 的 `enabledMcpServers()` 与 rust `manager.rs` 均无展开调用）——与 mcp.json 三层行为不一致。P1 批次 3 曾对生态内 manifest 的 `${` 出现率做过 R9 扫描（结论记录在 rust-core-design/ROADMAP.md 批次 3 行）。

## 2. 连接模型（rust 侧三池已拆；TS 侧仍 workspace 级单池）

- **rust 侧（P2 批次，2026-09-04 落地）**：
  - **UserPool**：`mirri-embedded/src/user_mcp_pool.rs`——包装一个 `McpConnectionManager` + connect-once 守卫（`claim_connect`）+ 进程级注册表 `shared_user_mcp_pool_for_home`（同 home 每 server 连一次，强引用保活）；desktop/NAPI 组合根换入共享池（`mirri-node/src/engine_bridge.rs:97-115`，N-workspace 复用）；CLI serve 单 workspace 进程用私有池（`user_mcp_pool.rs:39` 注释明言）。
  - **workspace 池**：即既有 workspace 级 `McpConnectionManager`（`embedded_server/mod.rs` 自述 "this is the workspace pool"）。
  - **无独立 PluginPool 类型**：plugin server 以运行时名连进 workspace 池，仅 per-server 状态存插件 user state；同名遮蔽用共享 shadow 集。
  - 写后对账已按双池路由（user 写 → UserPool，见 §7）。
- **TS 侧（agent-core-v2）**：仍是 per-workspace 单 manager（`workspaceMcp/workspaceMcpService.ts:1-12` 头注释 "handler-wide McpConnectionManager"）——用户池拆分为 conditional followup（rust-core-design/ROADMAP.md 1.2/1.3：随 TS-v2 退役线重估，大概率随退役消化）。
- 两侧会话零连接不变（TS `sessionMcpHandle.ts:6` "no per-session connections"；rust `mirri-mcp/src/connection.rs:176` workspace-level shared）。
- 残留缺口：`connect_all` 急切连接，无懒连接、无 tools/list 缓存（P4 未开工）；启动缓解仍只有 fast path——`mcp.disableFirstStepFastPath` flag 默认关闭 = MCP 就绪推迟到第一个 LLM step（创建不阻塞，第一步仍然等）。

## 3. wire 面（2026-09-08 刷新）

| 方法 | serve registry（`mirri-proto/src/rpc/registry.rs`） | klient 契约（`packages/klient/src/contract/global/mcp.ts`） |
|---|---|---|
| `mcp.list` | `both`（:471），含 pool/source 字段 | ✅（`mcpContract`） |
| `mcp.startupMetrics` | `both`（:472） | ❌ 无契约（rust client 裸映射 `sdk-rpc-client-rust.ts:288-290`） |
| `mcp.reconnect` | `both`（:473），`reconnectAndJoin` 防并发 | ✅ |
| `mcp.config.list/set/remove/setToolFilter` | `both`（:477-480，P1 批次 3） | ✅（`mcpConfigContract`，scope 枚举 `user`/`workspace`） |
| `mcp.config.test` | `both`（:485，P2 批次 4；scratch manager `__test_<uuid>`；TS 侧 test 时展开——A5 语义） | ✅ |
| `mcp.status` | `both`（:486，无遮蔽全局视图） | rust client 映射（`sdk-rpc-client-rust.ts:281`）；**node-sdk v2 client 未接线**（落 NOT_IMPLEMENTED） |
| `mcp.config.preview` / `mcp.tools` / `mcp.sync` / `mcp.auth.start/complete` | **空白**（ADR-004/005/006/011 待落） | 空白 |
| `plugin.setPluginMcpServerEnabled` | TS v2 引擎面已实现 | — |

- **事件面**：klient `contract/global/events.ts` 的 `KlientEventPayloads` 无任何 MCP 事件条目；实际存在的是 **`event.mcp.server.status`**（`mirri-core/src/session/events.rs:526-537` → `event_projection.rs:654-672`）——session 级事件流，发射点为 session overlay 监听器与池级 `publish_pool_status_event` fan-out 到 live session 事件总线（`embedded_server/mod.rs:2932-2977`）；**无 app 级订阅面**（无会话上下文的消费方拿不到事件）。RefreshToolFilter 分支不补发该事件（开放项 F-T1-4）。
- **v1 退役面**：`mcp.enable/disable/enableTool/disableTool/toggleState`（:489-493）与 `mcp.global.*` ×12（:496-507）均为 `emb_retired`（可用性 = Embedded，仍在 embedded dispatch、计划 B6 批次移除）。**serve wire 门禁**（`rust/crates/mirri-server/src/state.rs:255-263`）：可用性非 Server/Both 的方法直接回 method-not-found——这是 §6 工具弹窗断链的机制。
- MCP 配置读/写、连接测试、状态查询的 engine 侧实现均已存在（见 §1/§7 与上表）；剩余空白集中在事件契约、preview、`mcp.tools`、`mcp.sync`、`mcp.auth.*`。

## 4. OAuth（rust 侧已接线；wire 面未开；两侧布局不互通）

- **rust**：`mirri-mcp/src/oauth/` 完整（RFC 9728/8414/7591 发现 + 动态注册、PKCE S256、loopback callback、token 落 `<mirri-home>/mcp-tokens/<server>.json`、`ServerTokenProvider` 连接时挂载 + 过期透明刷新、断连检测惰性 + 自动重连指数退避 1s→30s ×8）。**授权发起面不是 wire 方法**：needs-auth 时向 live agent 注入合成工具 `mcp__<server>__authenticate`（`embedded_server/mod.rs:3047,3096`，2026-09-07 T8 批）驱动完整 flow——与架构 §4.5 设计的 `mcp.auth.start/complete` 存在形状分歧（收敛裁决见 ADR-011）。
- **TS**：`mcpCore/oauth/service.ts`（McpOAuthService，per workspace）+ `app/mcpConfig/oauthStore.ts`（App 级共享 `IMcpOAuthStore`），由 `WorkspaceMcpService` 构造时挂进 manager（`workspaceMcpService.ts:50-58`）。
- **令牌磁盘布局两侧不互通**：rust `<mirri-home>/mcp-tokens/` vs TS `<homeDir>/credentials/mcp/<key>-*.json`——与域需求 US6.2/N4 的跨引擎兼容要求冲突（开放项见 [mcp-ROADMAP.md](mcp-ROADMAP.md) F-8）。

## 5. US 差距依据（散在需求文档各处的现状事实）

- **US1.5（插件工具开关）**：**差距已闭合**——per-server 工具过滤已实现（见 §1），需求侧不再有缺口
- **US1.6（环境变量引用）**：双端展开已实现（见 §1）；两个缺口仍在——未定义变量只报 schema 校验失败（`env-expand.ts` 无 undefined 诊断，诊断不带变量名）、解析值呈现的 preview 出口未实现（§3）
- **US1.7（工具清单查看）**：`mcp.tools` 未实现（§3）；web 端现依赖退役 `mcp.global.tools` 且被 serve 门禁拦截（§6 F-1）
- **US3.2（打开 workspace 不等待）**：现状 fast path 只把 MCP 就绪推迟到第一个 LLM step（创建不阻塞、第一步仍等；§2）

## 6. 设置页面（web · rust 后端）现状盘点

> 快照：2026-09-08（全节刷新）·
> 配套需求：[mcp-settings-page-requirements.md](mcp-settings-page-requirements.md)（设置页面功能形态事实源）·
> 本节只陈述现状，不裁决。

- **页面主面已迁移新 wire**：`mcp.config.list`（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:1990`）、`mcp.status`（:2001）、`mcp.config.set`（:2065/:2069/:2105）、`mcp.config.remove`（:2074）、`mcp.config.setToolFilter`（:2138）、`mcp.reconnect`（:2192）均可用（web 经 mirri-server `/api/v1/rpc`，`ws-rpc-api.ts:3`）；`listGlobalMcpServers()` 名字保留但内部已是 `mcp.status` + `mcp.config.list` join（:1991-2026）；启停经 `mcp.config.set` 读改写 `enabled` 字段实现（非独立开关方法）。
- **工具清单弹窗断链（开放项 F-1）**：`McpServersPanel.vue:154` 调 `api.listGlobalMcpTools()` → `ws-rpc-api.ts:2044-2063` 直调 `mcp.global.tools`（2026-09-07 T4 接线）——但该方法 `emb_retired`（可用性 = Embedded，`registry.rs:497`），serve wire 门禁（`state.rs:255-263`）回 method-not-found，弹窗在 serve 后端为空态。embedded 引擎侧仍有真实现（`mirri-embedded/src/embedded_server/routes/mcp.rs:105-108` `dispatch_mcp_tools`）。rust-core-design/ROADMAP.md P0c 行的"已修"仅指客户端接线完成（FU-T4-1 登记 B6 三方随批迁移）。
- **「Reload all」无后端且文案双重过时（开放项 F-12）**：web 的 `reloadMcpServers()` 是故意抛错的 stub（`ws-rpc-api.ts:2077-2084`）——其错误消息与注释断言"配置变更在下次引擎启动生效"，但**写后增量 reconcile 已于 2026-09-07 落地、即时生效**（§7）——文案与实际行为两不符。终局入口 = `mcp.sync`（架构 ADR-006，未实现）。
- **文件监听对账池错位（日志风暴根因，开放项 F-2）**：`mirri serve` 的 CLI watcher（`rust/crates/mirri-cli/src/watcher.rs:512-615` `reload_mcp_servers`）以"三层合并配置 vs workspace pool"对账，而三池模型下 user 层 server 归 UserPool——user 层 `mcp.json` 每次原子写盘后，全部 user server 被判"新增"重复连入 workspace pool，每次 Connected 转换对 live agent 执行工具 remove-then-re-add（日志行 `removed MCP tools from live agents`，`embedded_server/mod.rs:3129`）。embedded 侧 `mcp.config.remove` 等的 reconcile 本身按名字增量、正确。
- **监听覆盖面**：`spawn_config_watchers` 仅由 `mirri serve` 启动路径挂载（`rust/crates/mirri-cli/src/main.rs:308-329`）；其它宿主形态无 MCP 文件监听。
- **解析值预览降级**：rust wire `mcp.config.list` 的 `resolved` 与 `source` 相同（不展开，对齐裁决 A5）；v1 TS 后端曾返回展开值（web 的眼睛图标预览在 v1 有真实展开效果）；`mcp.config.preview` 未实现（§3）。

## 7. rust 侧 MCP 配置实现现状补充（需求-架构追溯验证产出）

> 快照：2026-09-08（刷新）·
> 配套：[mcp-architecture.md](mcp-architecture.md) ADR-005/006/007（针对本节缺口的架构裁决）·
> 本节只陈述代码事实，不裁决。handler 路径统一为 `rust/crates/mirri-embedded/src/embedded_server/mcp_config_handlers.rs`。

- **写后增量 reconcile 已落地（2026-09-07，T1 批）**：`mcp.config.set/remove/setToolFilter` 落盘后引擎级后台对账（`mcp_config_handlers.rs:170-174/:188/:405` → `spawn_mcp_pool_reconcile` `:470-609`）——mirri-mcp `diff_pool_ops` 纯函数 diff（Connect/Disconnect/Reconnect/RefreshToolFilter 四类，`mirri-mcp/src/reconcile.rs:19-23`），user/workspace 双池同语义路由，`mcp_reconcile_lock` 串行化，在途调用 10s 排水后强断（R2 裁决）；事件复用 `mcp.server.status`。**解析失败时保守保持现状**（对齐 ADR-007 契约：`:622-646` 解析失败保持连接仅日志）；CLI watcher 降级为文件面兜底并存（其错位问题见 §6）。
- **并发写无版本机制**：`mcp.config.set/remove/setToolFilter` 均无 revision 参数、`mcp.config.list` 响应无 revision 字段；全部 last-writer-wins，仅靠进程内 reconcile 互斥串行化（`mcp_config_handlers.rs:163-190`、`:344-413`、`:85-156`）。
- **合并为整条遮蔽**：`load_mcp_config` 用 `HashMap::extend` 逐层整条替换（`mirri-mcp/src/config.rs:231-243`，模块文档自述 "the last layer wins entirely (the entry is replaced, not deep-merge)"）；无字段级合并。
- **setToolFilter 无目标层参数且会写 project-root**：目标层解析固定为"project-local → project-root → user 中第一个定义该名字的层"（`mcp_config_handlers.rs:387-408`），**包含本应只读的 project-root `.mcp.json`**（与 `mcp.config.set/remove` 走 `writable_scope_path`（`:438`）的只读约束不一致）。
- **解析失败：list 整体失败**：`mcp.config.list` 遇坏层文件整个 RPC 以内部错误失败（`mcp_config_handlers.rs:89`，错误含路径）；可解析文件内的坏条目被容错（`:894-913` 回退 `{"enabled": true}`）。
- **非连接绑定字段变更不断连接**：reconcile 的 diff 四类为 `Connect/Disconnect/Reconnect/RefreshToolFilter`；timeout-only 变更落 `RefreshToolFilter`——连接保持、更新 server 配置（`mirri-mcp/src/reconcile.rs:19-23`、`:76-78`、`:107-120`，`mcp_config_handlers.rs:584-585`）。

## 8. 设置页面工具级治理与来源呈现现状（SDR-S005 配套，2026-10-17）

> 本节只陈述代码事实，不裁决。缺陷对应的页面级验收见 [mcp-settings-page-requirements.md](mcp-settings-page-requirements.md) PS3.2/PS3.3（SDR-S005）。

- **禁用工具在页面工具清单中不可见（P0 缺陷）**：rust `mcp.tools` RPC 返回全部工具并逐个携带 `enabled` 标志（`rust/crates/mirri-embedded/src/embedded_server/mcp_config_handlers.rs:674-714`），但 web 端 WS-RPC 适配层在返回前丢弃 `enabled === false` 的工具（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts` `listGlobalMcpTools`，约 :2794）——工具一经禁用即从设置页清单消失，无法从界面重新启用。HTTP 后端（`apps/mirri-web/src/api/daemon/client.ts:1508-1516`）无此过滤，两后端行为分叉。
- **卡片计数与清单内部不一致**：卡片"n/m 已启用"计数从落盘的禁用偏好推导（含禁用工具），工具弹窗只显示启用工具——同一页面两处口径不一致。
- **来源徽章与同名遮蔽提示已落地**（2026-10-17，feat/latest）：`listGlobalMcpServers` 在 web 端合并双池同名条目（优先级 project > projectRoot > user > plugin）并携带 source/shadowedPools；分区浏览（Tab）未实现。
- **插件来源 server 的工具级启停**：引擎通过 filter-only override 支持（偏好写用户层，插件本体不动，`mcp.config.setToolFilter` → `upsert_filter_override_entry`，`rust/crates/mirri-mcp/src/config.rs:1036-1074`）——与文件来源无能力差异；页面验收此前未覆盖。
- **server 整体禁用时的工具清单**：页面当前对禁用 server 未提供工具清单入口（无"只读可见"呈现）；落盘的 `disabledTools`/`enabledTools` 数组在重启后存活，可作为只读清单的数据来源。

## 9. setToolFilter 作用域分派对团队共享来源的覆盖缺口（SDR-S005 实现排期参考，2026-10-17）

> 本节只陈述代码事实，不裁决。对应页面需求见 mcp-settings-page-requirements.md PS3.3（SDR-S005"任一来源均可操作"验收）。

- **缺口**：`mcp.config.setToolFilter` 的作用域分派对"定义在项目根(团队共享层)的 server"存在行为分叉——
  - 带 `scope: user` 调用、名字却定义在项目根时:报 `mcp.server_not_found`(mcp_config_handlers.rs `dispatch_mcp_config_set_tool_filter` 的 User 分支只查用户层文件);
  - 不带 `scope` 调用、名字定义在项目根时:报 `mcp.config.read_only`(None 分支显式拒绝,A2 只读保护);
  - 仅 `scope: workspace` 分支提供正路:名字不在项目本地层时,向项目本地层写 filter-only override(遮蔽项目根/用户层定义)。
- **页面影响**:web 端 `setMcpToolDisabled`(apps/mirri-web/src/api/daemon/ws-rpc-api.ts `setMcpToolDisabled`)当前调 `setToolFilter` 不传 `scope`——团队共享来源 server 的工具开关被 read-only 直接拒绝,不满足 PS3.3 的"任一来源均可操作"验收。
- **修复方向(供架构裁决,非结论)**:统一为"filter 类变更不落定义层,一律向个人可写层写 override"或"None 分支复用 workspace 分支的 override 路径";涉及 rust 侧 `mcp_config_handlers.rs` 改动。

## 10. 团队共享来源（项目根 .mcp.json）的维护模型与防呆现状（2026-10-17）

> 本节陈述现状事实与设计假设的差距。产品裁决见 mcp-settings-page-requirements.md D-S5 / SDR-S005；后两点（防呆、脚手架）尚无需求承载。

- **概念**：项目根 = git 仓库根目录，由引擎向上探测 git 工作树发现。项目根下的 `.mcp.json` 是团队共享的 MCP 配置——随仓库分发，clone 即得，与 `.editorconfig` 类似定位（`.mcp.json` 也是 MCP 生态的官方约定路径）。三个位置的共享意图：项目根 `.mcp.json`（团队维护，进版本库）/ 项目本地 `.mirri-code/mcp.json`（个人，不应进版本库）/ 用户全局 `~/.mirri-code/mcp.json`（个人，随用户）。
- **预期维护模型**：团队共享文件的**定义**变更走正常版本控制协作流（手动编辑 → 提交 → review → 合并），因此设置页将其定为只读资产（PS1.2）；个人的**启停偏好**通过 filter-only override 写入个人层，不触碰共享文件（域 A1 同义）。
- **防呆缺口**：项目本地 `.mirri-code/mcp.json` 无任何机制提醒"应进 .gitignore"——用户在个人层配置含凭据的 server 后 `git add .` 即泄漏；反向同理，想在个人层加配置的用户可能误写入共享 `.mcp.json` 提交给全团队。
- **脚手架缺位**：无 `mcp init` 类脚手架生成初始共享文件、无文档指引哪些字段适合共享（命令类适合；凭据类应使用 `${env:VAR}` 引用）。
