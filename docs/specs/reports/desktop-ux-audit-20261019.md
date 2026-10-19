# Desktop 用户体验审计报告（2026-10-19）

> 一次性调查报告：盘点 desktop 端可优化的用户体验点，按「影响 × 成本」给出优先级与落地顺序建议。
> 基于 2026-10-19 的代码现状调研，未修改任何代码；不构成设计文档，不承载需求决策。
> 报告路径引用规范：所有仓库内路径均为相对仓库根目录的完整路径，引用前已经工具确认存在。

## 0. 核心结论（先读这段）

- **desktop 是一个极薄的 Electron 壳，自身不承载任何 UI**。它 spawn 内嵌的 Rust `mirri` 二进制跑 `web` 服务器，然后开 `BrowserWindow` 加载 `apps/mirri-web` 的 Vue 3 SPA。因此"桌面体验优化"必然是**壳层（`apps/mirri-desktop`）+ web 面（`apps/mirri-web`）双端**的事，且全部 7 个优化项均可落在这两处，不触碰 rust 引擎，符合引擎门卫约束。
- 最硬的四个缺口：**自动更新、错误屏/诊断、命令面板/快捷键、长会话虚拟滚动**。
- 壳层测试覆盖扎实（纯逻辑全部有 vitest），但 UX 层（启动屏、错误屏、更新流、托盘交互）无 E2E/集成测试。

## 1. 架构现状（30 秒版）

| 层 | 位置 | 职责 |
|---|---|---|
| Electron 壳 | `apps/mirri-desktop/src/main/` | 进程管理、窗口、托盘、通知、深链、系统文件打开、启动登录项 |
| Rust 引擎 daemon | `rust/crates/`（spawn 的 `mirri web` 子进程） | 全部引擎能力，端口 58827，同源 REST + WS（`/api/v1`） |
| Web UI | `apps/mirri-web/src/` | 全部用户可见 UI：会话、聊天、设置（12 个 tab）、onboarding、i18n 双语 |

关键机制：

- 壳通过 `apps/mirri-desktop/src/main/ensure-server.ts` spawn daemon 并两阶段轮询 `/api/v1/healthz`（`isListening` code:0|1 → 返回即 loadURL → 后台等到 code:0）。
- SPA 直连 daemon（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts`），preload 只暴露原生能力桥（`window.mirriDesktop`，`apps/mirri-desktop/src/main/preload.ts`）。
- 关窗驻留托盘；退出时主进程 SIGTERM 自己的 daemon（`apps/mirri-desktop/src/main/index.ts`）。

## 2. 体验薄弱点清单（按建议落地顺序）

### 第一梯队：信任与安全感

| # | 优化项 | 现状证据 | 影响 | 成本 |
|---|---|---|---|---|
| 1 | 自动更新 | `apps/mirri-desktop/src/main/updater.ts` 技术就绪但默认关闭；publish URL 为占位符 `updates.invalid`（`apps/mirri-desktop/electron-builder.config.cjs`）；README 明确列为 "not done"；`docs/specs/non-functional-requirements.md` 已把"更新检查后台静默、断网不报错"列为 P0 验收项 | 用户只能手动去 GitHub Release 下载；后面所有改进到不了存量用户手里 | 中 |
| 2 | 错误屏与诊断改造 | 错误屏为内联 HTML、中文硬编码、无错误码、无自救建议（`apps/mirri-desktop/src/main/index.ts` 启动/错误屏部分）；排障只能看 `server-desktop.log` | 出问题时排障成本全压在用户身上，新手恐慌 | 中 |

### 第二梯队：效率

| # | 优化项 | 现状证据 | 影响 | 成本 |
|---|---|---|---|---|
| 3 | 命令面板 + 快捷键体系 | web 面全局仅 Cmd/Ctrl+B 切侧栏（`apps/mirri-web/src/App.vue`）；无 Ctrl+K 命令面板、无快捷键帮助表、无键盘导航覆盖；已有局部键：审批卡片数字键、Escape 关面板 | 高频用户效率低；功能可发现性差 | 中高 |
| 4 | 长会话虚拟滚动 | `docs/specs/non-functional-context.md` 明确记录"web 无虚拟滚动，长会话靠分页"；分页加载在 `apps/mirri-web/src/components/chat/ConversationPane.vue` | 超长会话滚动卡顿 | 中 |

### 第三梯队：打磨

| # | 优化项 | 现状证据 | 影响 | 成本 |
|---|---|---|---|---|
| 5 | Onboarding 强化 | `apps/mirri-web/src/components/settings/Onboarding.vue` 只覆盖语言 + 配色 + 强调色，无工作流/能力介绍 | 新手不知道能干什么，可发现性弱 | 低中 |
| 6 | 通知偏好设置 | 有声音通知（`apps/mirri-web/src/composables/useSoundNotification.ts`）与 tag 机制，但无声音/横幅/免打扰的配置入口 | 通知行为不可控 | 低 |
| 7 | 无障碍补齐 | 大量交互组件（审批卡、状态面板、侧栏）无 aria 角色/焦点管理；移动端组件无专门处理 | 边缘用户体验受损 | 中 |

### 附：壳层 i18n 缺失（跨切项）

`apps/mirri-desktop/src/main/index.ts` 的加载/错误屏与原生菜单项（"重试连接""打开服务日志"）均为硬编码中文，不随系统/web 语言（`apps/mirri-web/src/i18n/` 已有 en/zh 全量双语）。建议随优化项 2 一并解决。

## 3. 范围外事项（明确不混入）

- **MCP 配置来源误标**：desktop daemon 以 `cwd=/` 启动导致 user 层 MCP 配置被误标为 project 来源。属 rust 引擎 bug，已有独立计划 `plan/mcp-layer-overlap-fix.md`，受"引擎改动 rust-first"门卫约束，不在本清单内。
- **Windows/Linux 签名缺失**（README v1 "not done"）：属发布/分发域（`docs/specs/release-ROADMAP.md`），与优化项 1 相关但决策归 release 域。

## 4. 测试现状

- 壳层纯逻辑覆盖扎实：`apps/mirri-desktop/src/main/` 下 `deep-link.test.ts`、`external-links.test.ts`、`file-launch.test.ts`、`ipc.test.ts`、`ipc-handlers.test.ts`、`ensure-server.test.ts`、`server-binary.test.ts`、`tray-menu.test.ts`、`updater.test.ts`；主流程（`index.ts`、`register-ipc.ts`、`tray.ts`）按设计刻意不测。
- web 桥接有三级降级链测试（desktop IPC → 引擎 RPC → copy-path 兜底）。
- **空白**：UX 层（启动屏、错误屏、更新流）与快捷键/无障碍/加载状态无专门测试。落地任一优化项时应按 Given-When-Then 补测试。

## 5. 诚实的局限

这是从代码结构推断出的薄弱点清单，其中"onboarding 太浅""无障碍不完整"这类判断缺少真实用户反馈佐证。若手上有用户抱怨数据或实际使用痛点，优先级应以其为准，覆盖本报告的排序。
