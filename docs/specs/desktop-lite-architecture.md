# Mirri Desktop Lite — 架构设计（Architecture）

- 域：desktop-lite
- 需求：`desktop-lite-requirements.md`
- 技术选型：Electrobun 2.0.4（壳运行时）+ Hutch 0.28.1（构建链）+ Bun 1.4（主进程运行时）
- 可行性依据：`plan/electrobun-shell-poc-report.md`（PoC 实测记录）

## 1. 总体架构

```
┌─ Mirri Code Lite（Electrobun 壳）─────────────────────┐
│  主进程（Bun 运行时，TypeScript）                        │
│  ├─ 桥 handlers：实现 MirriDesktopBridge 13 方法         │
│  ├─ daemon 集成：复用/拉起 mirri 二进制 + 健康检查        │
│  ├─ open-in 注册表：应用检测 + icns 图标提取（搬运自主壳） │
│  └─ preload 脚本：Electroview 客户端 + window.mirriDesktop │
│                          │ RPC（defineRPC，加密环回 WS）  │
│  WKWebView（加载 daemon 的 web UI，零改动）               │
└──────────────────────────────────────────────────────────┘
```

关键决策与理由：

### ADR-001：契约单一定义源 = 界面侧（web）

沿用既有决策：`apps/mirri-web/src/lib/desktopBridgeContract.ts` 是 13 方法契约的唯一事实源。轻量壳以 **type-only import** 接入（与主壳的契约测试同款机制），编译期可加同款双向可赋值闸门。**不新建第二份契约。**
*拒绝的替代*：① 共享类型包——曾被明确否决（双壳不要相互依赖、不要共享包）；② 各自维护契约靠文档同步——漂移只能在运行时降级路径发现，已有教训。

### ADR-002：preload 传脚本内容而非路径

PoC 实测：Electrobun 的 BrowserWindow `preload` 参数收**脚本内容字符串**；传路径静默失败。preload 由 esbuild 打成单 IIFE（内含 Electroview 客户端 + 桥注入），主进程 `readFileSync` 后传入。
*拒绝的替代*：让 web UI 构建期打包桥客户端——违反 R4（界面零改动）。

### ADR-003：daemon 集成不复制，跨壳搬运

主壳 `apps/mirri-desktop/src/main/server/` 的 daemon 编排逻辑（复用探测 → spawn → 健康轮询）以**源码级搬运**（非包依赖）方式复用。
*事实修正（reviewer 核验）*：`server/daemon.ts` 并非纯 Node——它 import electron 的 `app`（`isPackaged` 控制 `--web-dir`）并依赖 `server-binary` 的 `devRepoRoot`。搬运时**剥离 electron 依赖，改为注入 `isPackaged`/`webDir` 参数**。
*拒绝的替代*：① 抽共享包——冻结主壳上开新 API，违背维护冻结约定；② 各写一套——语义漂移风险（R8 要求两壳行为一致）。

### ADR-004：open-in 注册表跨壳搬运（含图标提取）

搬运对象为两层（reviewer 核验的实际结构）：`apps/mirri-desktop/src/main/open-in-apps.ts`（注册表 + 检测）+ `apps/mirri-desktop/src/main/platform/open-in/`（launch 解析）。icns→dataURL 图标提取（`app-icon.ts`）同方式搬运，**首版可先砍图标**（`iconDataUrl` 可选字段，界面已有降级），但 finder/vscode 等注册表必须带（R2 核心流）。
*拒绝的替代*：运行时 `open -a` 硬编码——PoC 已证明可行但丢注册表可维护性与 Windows 扩展性。

### ADR-005：能力降级矩阵（第一阶段）

| 能力 | 轻量壳实现 |
|---|---|
| openPath / revealPath / openExternal / openInEditor / openInApp / listOpenInApps | 原生 API / `open -a` / 注册表 |
| notify | `Utils.showNotification`（无 tag 点击回调 → `{shown:true}`，点击事件降级） |
| setBadgeCount / dockBounce | 返回明确的 `false`/完成值——**不得假装成功** |
| setLoginItem / getLoginItem | 读写均返回 `{open:false}`（写不落盘，返回值真实反映未生效） |
| onDeepLink / onNotificationClick | 通道保留（schema 定义），主进程无触发源——事件永远不来，调用方无感知 |
| 深链协议注册 | 不注册（R3 明确降级） |

### ADR-006：应用身份与 daemon 归属

- 名称：`Mirri Code Lite`；bundle identifier：`ai.mirri.desktop-lite`（与主壳 `ai.mirri.desktop` 区分，reviewer 已核验）
- 图标：独立 iconset（复用主壳图标资产变色/加"Lite"角标，构建期生成）
- 服务复用：与主壳共用状态文件探测协议（同一默认端口），确保 R8
- **两壳同冷启动竞态（reviewer 提出，Major）**：状态文件单 pid 字段，双冷启动会双 spawn + 互相覆盖。处置：spawn 前加**原子锁文件**（`O_EXCL` 创建，持有方为 spawn 者）串行化冷启动；状态文件增加 `owner` 字段，退出清理仅杀自己 owner 的实例——非本壳拉起的 daemon 永不杀（R8）。

### ADR-007：monorepo 集成方式

- 目录 `apps/mirri-desktop-lite`，Hutch 工具链**目录内自治**（hutch.config.ts + electrobun.config.ts 与 lock 文件均在本目录），不污染根 pnpm workspace 脚本
- **pnpm glob 冲突（reviewer 提出，Major）**：根 `pnpm-workspace.yaml` 的 `apps/*` glob 会吞并本目录的 package.json，根 `pnpm install` 会为它装 node_modules，与 Hutch/Bun 自管 lockfile 冲突。处置：workspace 配置改为显式排除本目录（或等价机制），确保 pnpm 不接管；`flake.nix` 的 workspacePaths/workspaceNames **不**收录本包（Nix 构建不涉及它）
- 构建命令：`pnpm --filter @mirri-ai/mirri-desktop-lite run dev` / `build`（内部转发 hutch）

### ADR-008：分发形态（第一阶段）

- macOS 本地试用：构建 `.app` 不做开发者 ID 签名/公证——首次打开经 Gatekeeper 提示后放行（右键打开），R6 已约定此试用形态
- **不进** release 流水线（changesets→GitHub Release）：轻量壳不发布安装包，仅本地构建试用；正式分发（含签名公证）留待试用结论后的后续 SDR/ADR
*拒绝的替代*：ad-hoc 签名——本地试用无收益，反而引入身份混乱。

## 2. 模块划分（目录内）

```
apps/mirri-desktop-lite/
├─ hutch.config.ts / electrobun.config.ts / package.json
├─ src/
│  ├─ bun/index.ts          # 主进程入口：窗口 + 桥 + daemon 编排
│  ├─ bun/bridge.ts         # 13 方法 handlers（参数校验 + 降级矩阵）
│  ├─ bun/daemon.ts         # 搬运的 daemon 编排（R8 语义）
│  ├─ bun/open-in.ts        # 搬运的应用检测 + 图标提取（ADR-004）
│  ├─ preload/bridge.ts     # esbuild 入口：Electroview + window.mirriDesktop
│  └─ shared/schema.ts      # MirriSchema（RPC schema，type 关联 web 契约）
└─ icon.iconset/            # 应用图标
```

## 3. 关键流程

### 3.1 启动序

1. 主进程入口 → daemon 探测（复用 or 拉起 + 健康轮询，超时 20s）
2. 创建 BrowserWindow（hiddenInset，preload 注入）
3. 加载 `{origin}/?mirri_desktop=1&platform=darwin`
4. 日志：daemon 来源、桥注入结果（R9）

### 3.2 桥调用链

web UI `window.mirriDesktop.method(args)` → preload 的 RPC request → 主进程 handler（白名单校验）→ 原生 API / `open -a` / 注册表 → 返回结构化结果 → Promise resolve。

## 4. 测试策略

- **契约测试**（编译期）：与主壳同款双向可赋值断言，type-import web 契约——漂移在 CI 阶段即红
- **启动自检**（运行时脚本）：断言启动日志含 daemon 复用/拉起标记 + preload 注入标记
- **人工走查**：R2 的 5 项核心流（用户执行）

## 5. 风险与开放项

| 风险 | 缓解 |
|---|---|
| Hutch 工具链在 monorepo 内的水土不服 | PoC 已在独立目录验证工具链自治；构建不进根 CI（试用阶段） |
| Bun spawn 退出竞争（应用退出 vs daemon 存活） | R8 语义：非本壳拉起的 daemon 不杀；自拉起的退出时发 SIGTERM 后即退 |
| WKWebView 兼容回归 | PoC 已全页实测；界面零改动原则下风险归界面自身 |
| 通知点击事件缺失 | schema 通道保留，界面已有降级（浏览器通知无点击回调路径） |

## 6. 变更记录（ADR）

| 日期 | ADR | 内容 |
|---|---|---|
| 2026-10-27 | ADR-001~008 | 初始架构。核心决策：契约唯一源在 web（type-only import）；preload 传内容；daemon/open-in 源码级搬运（daemon 剥离 electron 依赖改参数注入）；降级矩阵（读写均不假装成功）；独立应用身份 + daemon 原子锁防双冷启动竞态；monorepo 内 Hutch 自治（pnpm glob 显式排除）；分发不签名不进 release 流水线 |
