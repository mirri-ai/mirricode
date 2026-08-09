# Bun Runtime Migration — Feasibility Report (agent-core-v2)

> **Status**: Investigation report（可行性分析，待决策）
> **Date**: 2026-08-09
> **Scope**: `packages/agent-core-v2`（内核）+ 构建/发行链路影响面
> **Related**: `sqlite-session-index-design.md`（`node:sqlite` 现状）、`b4-coex-report.md`（v1/v2 共存验证）、§11 参考实现（cc-haha）

> 本文档盘点「将 agent-core-v2 内核从 Node/pnpm 运行时迁移到 Bun」的**挑战**与**预期收益**，
> 所有代码事实均标注文件位置（`path:line`），Bun 兼容性结论分「已验证 / 需实测 / 推断」三档，
> 不做过度的乐观或悲观假设。

---

## 1. 背景与目标

当前 monorepo 的运行时底座：

- **Node.js `>=24.15.0`**（根 `package.json` engines + `.nvmrc`，`.npmrc` 设 `engine-strict=true`）
- **pnpm `10.33.0`**（`packageManager` 字段）
- 内核构建用 **tsdown**（rolldown），发行打包走 **Nix**（`flake.nix` 的 `pnpmConfigHook` + 手写 `workspacePaths`/`workspaceNames`）
- 脚本执行依赖 **tsx** + 自定义 **raw-text-loader** 三个文件（`build/raw-text-loader.mjs`、`build/raw-text-plugin.mjs`、`build/register-raw-text-loader.mjs`）
- 终端层依赖原生插件 **node-pty**（`apps/mirri-code/scripts/native/`、`scripts/fix-node-pty-perms.mjs` 均围绕它）

本报告的讨论范围：把内核（及关联发行链路）切到 **Bun 运行时**（含 `bun install` / `bun test` / `bun build` 的连带切换），
评估硬阻塞、兼容风险、工程成本与收益，输出决策建议。

---

## 2. 现状盘点：runtime 依赖全景

### 2.1 Node 专属 API 使用点（agent-core-v2/src）

`node:` 内置模块 import 统计（按出现次数）：

| 模块 | 次数 | 典型使用点 | Bun 兼容性 |
|------|-----:|-----------|-----------|
| `node:fs` | 36 | hostFsService、workspaceFs、sessionExport | ✅ 良好 |
| `node:crypto` | 28 | 见 2.2 | ✅ 良好（仅基础 API） |
| `node:stream` | 20 | bash `process-task.ts`、`sessionExport/zip.ts`、`workspaceFs/internal/fsProcess.ts` | ✅ 良好 |
| `node:path` | 19 | 各处 | ✅ 良好 |
| `node:os` | 9 | hostEnvironmentService | ✅ 良好 |
| `node:child_process` | 3 | `_base/execEnv/environmentProbe.ts`、`agent/externalHooks/runner.ts`、bash 工具 | ✅ 良好 |
| `node:net` | 2 | `local-fetch-url.ts`（`BlockList`/`isIP`/`LookupFunction`） | ⚠️ 需实测 |
| `node:dns` | 2 | `local-fetch-url.ts`（`lookup`、`node:dns/promises`） | ⚠️ 需实测 |
| `node:sqlite` | 1 | `app/sessionIndex/sqliteSessionStore.ts`（`DatabaseSync`） | ❌ **未实现** |
| `node:readline` | 1 | `app/sessionExport/wire-scan.ts`（`createInterface`） | ✅ 良好 |
| `node:http` | 1 | `mcpCore/oauth/callback-server.ts`（`createServer`） | ✅ 良好 |
| `node:buffer` | 1 | 各处 | ✅ 良好 |

### 2.2 `node:crypto` 实际使用面

仅用到基础 API（全部 Bun 支持）：

- `randomUUID`：`sessionLifecycleService.ts`、`terminalService.ts`、`app/auth/authService.ts`、`app/file/fileServiceImpl.ts`、`agent/task/taskService.ts`、`agent/rpc/rpcService.ts`、`agent/skill/skillService.ts`、`agent/tools/skill/skillTool.ts`、`agent/toolResultTruncation/toolResultTruncationService.ts`、`toolDedupe/toolDedupeService.ts`、`app/telemetry/cloudAppender.ts`、`kosong/provider/bases/*`（openai/anthropic/google-genai 基类）等
- `randomBytes`：`mcpCore/oauth/provider.ts`、`app/telemetry/cloudTransport.ts`、`agent/task/taskService.ts`
- `createHash`：`os/backends/node-local/tools/rgLocator.ts`、`mcpCore/oauth/store.ts`、`agent/toolDedupe/toolDedupeService.ts`

无 `scrypt` / `hkdf` / `X509Certificate` / `createCipheriv` 等高风险 API。

### 2.3 第三方依赖属性（`packages/agent-core-v2/package.json`）

| 类别 | 依赖 | 说明 |
|------|------|------|
| **原生插件** | `node-pty@^1.1.0` | 唯一 native 依赖，Bun 不兼容 |
| wasm | `@jsquash/webp@^1.5.0` | base64 内嵌 `webp_dec.wasm`（`agent/media/webp-dec-wasm.ts`），Bun 支持 wasm |
| 纯 JS（预计无问题） | `@antfu/utils`、`chokidar`、`ignore`、`jimp`、`js-yaml`、`linkedom`、`pathe`、`picomatch`、`retry`、`smol-toml`、`socks`、`tar`、`ulid`、`yauzl`、`yazl`、`zod`、`ajv`、`ajv-formats`、`@mozilla/readability` | 不依赖 node 内部行为 |
| 网络层 | `undici@^7.27.1` | 见 §3.1.3，行为语义问题 |
| SDK | `openai`、`@anthropic-ai/sdk`、`@google/genai`、`@modelcontextprotocol/sdk` | 默认走全局 fetch，受 §3.1.3 影响 |
| workspace | `@mirri-ai/agent-profile`、`minidb`、`protocol`、`tree-sitter-bash`、`v2-oauth` | 需逐一验证 Bun 兼容 |

---

## 3. 挑战分析

### 3.1 硬阻塞（不换方案就过不去）

#### 3.1.1 `node-pty` — 终端层原生插件

- **位置**：`os/backends/node-local/hostTerminalService.ts:13`（`import type { IPty } from 'node-pty'`）、`:26`（惰性 `await import('node-pty')`）
- **问题**：node-pty 是 C++ 原生插件，依赖 libuv 的 PTY 功能；Bun 无法加载，也没有对应的 node 兼容实现。
- **替换方案（现成）**：`Bun.spawn({ pty: true })` 原生支持 PTY。该服务已走在 `IHostTerminalService` 接口 + `node-local` 后端 + 惰性加载的设计后面，加一个 `bun` 后端即可。
- **工作量**：需要把 `IPty` 的事件/写入语义完整映射到 Bun 的 pty API——`onData`/`onExit`/`write`/`resize`/`kill`，其中 `resize` 与 `kill` 的时序行为需要实测对齐（PTY 场景最容易出竞态）。
- **验证项**：TUI 终端、bash 工具里的交互式命令、`mcpCore/client-stdio.ts` 的 stdio 子进程（不依赖 pty，仅 `spawn`，风险低）。

#### 3.1.2 `node:sqlite`（`DatabaseSync`）— 不在抽象层内

- **位置**：`app/sessionIndex/sqliteSessionStore.ts:1`（`import { DatabaseSync } from 'node:sqlite'`）
- **问题**：Bun **未实现 `node:sqlite`**，只提供 `bun:sqlite`（API 不同：`DatabaseSync` vs `new Database()`，方法/事务语义也有差异）。
- **难点**：该依赖**不在 OS 抽象层后面**，是 app 层直接使用。`os/` 的五域接口（`IHostProcessService`/`IHostTerminalService`/`IHostFileSystem`/`IHostFsWatch`/`IHostEnvironment`）不覆盖数据库。
- **工作量**：抽一层 sqlite 接口（或条件导入 `node:sqlite` / `bun:sqlite`），`sqliteSessionStore.ts:62`（`hasStateMtimeColumn`）等 schema 探测逻辑与 `bun:sqlite` 的类型需对齐。
- **注意**：这是 v2 冷启动 session list 读路径的核心（见 `sqlite-session-index-design.md`），改动需保住其性能特性（复合索引倒序 + LIMIT）。

#### 3.1.3 undici 全局 dispatcher 与 Bun 原生 fetch 的语义断裂

- **位置**：
  - `app/web/providers/local-fetch-url.ts:15-21`：用 undici `Agent` + `node:dns` 自定义 `lookup` + `node:net.BlockList` 实现 **SSRF 防护**（本地 URL 抓取）
  - `_base/utils/proxy.ts:11-12, 219`：`setGlobalDispatcher` 设 **全局 HTTP 代理**
  - `mcpCore/client-stdio.ts:7`：仅用 proxy 的环境变量函数（`proxyEnvForChild`/`reconcileChildNoProxy`），风险低
- **问题**：**Bun 的原生 `fetch` 不理会 undici 的全局 dispatcher**。模型 SDK（openai/anthropic/genai）在 Bun 下会走 Bun 原生 fetch，于是：
  1. 代理配置对模型流量**静默失效**；
  2. SSRF 防护（`BlockList` 拦截本地地址）**静默失效**。
- **特性**：这是**行为差异而非 crash**，最容易漏测，且属于安全相关（SSRF）。
- **工作量**：给三个 SDK 基类显式注入自定义 fetch（或重构为统一 fetch 入口），并在 bun 后端重新实现代理/防护逻辑。

### 3.2 兼容性风险（大概率没问题，但必须实测）

| 项 | 位置 | 风险点 |
|----|------|--------|
| `node:stream` 事件语义 | bash `process-task.ts`（`removeListener`/`once`/`setEncoding`/`on`）、`sessionExport/zip.ts`、`workspaceFs/internal/fsProcess.ts` | Bun 兼容层较成熟，但 stream 的背压/事件时序需跑一轮 bash 工具 + zip 导出集成测试 |
| `node:child_process` | `environmentProbe.ts`（`execFile`）、`externalHooks/runner.ts`（`spawn`）、bash 工具 | Bun 支持良好；`loginShellPath.ts` 的 login shell 探测在 macOS 下的行为需验证 |
| `node:http` OAuth 回调 | `mcpCore/oauth/callback-server.ts` | Bun 的 http server 兼容较好，但回调端口绑定/关闭时序需实测 |
| `node:net.BlockList` / `node:dns.lookup` | `local-fetch-url.ts` | Bun 对 `BlockList` 与自定义 `lookup` 的支持需要实测（SSRF 防护正确性依赖于此） |
| `node:os` / `process` 全局 | `hostEnvironmentService.ts`、`globalThis.process.env`（`hostTerminalService.ts:35`） | Bun 有兼容层；`process.platform`/`arch`/`env` 正常，个别 `os` 方法有差异 |
| wasm 解码 | `agent/media/webp-decode.ts`（`WebAssembly.compile` + `init`） | Bun 支持 wasm，但 `init` 传入模块的细节需实测 |
| SDK 流式读取 | `kosong/provider/bases/openai/*`（SSE） | 依赖 fetch 的 `ReadableStream` 语义，Bun 实现与 undici 有差异，需跑模型流式集成测试 |

### 3.3 工程与发行成本

| 领域 | 现状 | 切换成本 |
|------|------|----------|
| 构建 | tsdown（rolldown）→ `dist` | Bun 可直接跑 TS；若仍要产物，需评估 `bun build` 替代 tsdown 或保留 rolldown |
| 脚本执行 | `tsx --import ../../build/register-raw-text-loader.mjs`（`gen:config-manifest` 等） | Bun 原生执行 TS + 原生文本导入，可删 tsx 与 loader 三件套——**这是收益不是成本** |
| 发行 | `flake.nix`：`pnpmConfigHook` + 手写 `workspacePaths`/`workspaceNames` + native-deps | nixpkgs 对 bun lockfile 的构建支持不如 `pnpmConfigHook` 成熟，**需要重写 Nix 构建链** |
| 原生依赖 | node-pty（`apps/mirri-code/scripts/native/`、`scripts/fix-node-pty-perms.mjs`） | 换 `Bun.spawn({pty})` 后整套可删——**收益** |
| 版本门槛 | engines / `.nvmrc` / `engine-strict` | 需要改为 bun 版本约束 + `bun.lock`，CI 换 `oven-sh/setup-bun` |
| 测试 | vitest（node 池）`vitest.config.ts` | 需 `pool: 'bun'` 或 `poolOptions`，`test/setup.ts` 的全局假设要过一遍；部分高级 mock（`vi.mock` 的 node 模块拦截）行为需验证 |
| 依赖治理 | pnpm 严格 node_modules + `lint:imports`（`scripts/check-import-boundaries.mjs`） | bun 是扁平 hoisting，安装层不再挡 phantom dependency，**依赖边界治理退化，需靠 lint 兜底** |

---

## 4. 预期收益分析

### 4.1 真实收益（与当前仓库强相关）

**R1. TS 免构建直跑，删除一层脚本基建**
- Bun 原生执行 TypeScript，`import x from './x.txt'` 原生支持文本导入。
- 可直接删除：`build/raw-text-loader.mjs`、`build/raw-text-plugin.mjs`、`build/register-raw-text-loader.mjs`；
- `package.json` 中 `gen:*` 脚本的 `tsx --import ...` 前缀全部去掉，`tsx` 依赖移除。

**R2. CLI 冷启动提速**
- `apps/mirri-code` 是交互式 CLI/TUI，每次调用都有进程启动开销。Bun 进程启动约快 5–10 倍，对 CLI 体感是实打实的提升；仓库内大量一次性脚本（`scripts/*.mjs`、包内 gen 脚本）同样受益。

**R3. 终端层解锁，去掉最脏的原生依赖**
- `Bun.spawn({ pty: true })` 原生支持 PTY，`node-pty` 及其配套（`apps/mirri-code/scripts/native/`、`scripts/fix-node-pty-perms.mjs`、native-deps 解析）可以整体移除——这是当前构建链里最易碎的一块（跨平台编译、权限修复脚本都是围绕它存在的）。

**R4. `bun:sqlite` 性能**
- `bun:sqlite` 直连 libsqlite（非线程池异步），session index 这种高频小查询场景通常比 `node:sqlite` 快；且是内置模块，少一个运行时版本漂移风险。同时保留 sqlite 方案对 v2 冷启动读路径的既有收益。

**R5. 单一可执行文件分发**
- `bun build --compile` 产出含运行时的单文件二进制，目标机器不再需要预装 node。配合 R3（去掉 node-pty 后无原生插件），发行物可以大幅简化，`flake.nix` 的复杂度有收敛空间。

**R6. 内置 API 省依赖**
- `Bun.spawn`（替代 `child_process` 封装）、原生 WebSocket 客户端（server 模式/远程会话）、`Bun.file`/`Bun.write`、`Bun.password`（OAuth PKCE 场景），以及 SDK SSE 流式读取对 Bun fetch 的依赖。

### 4.2 需打折的收益（别被营销带走）

**D1. 安装速度：对 pnpm 提升有限**
- pnpm 的硬链接 store 已是顶级方案，冷/增量安装在超大 monorepo 下与 bun 差距很小，磁盘复用上 pnpm 甚至更优。`pnpm → bun install` 不是核心收益。

**D2. 扁平 node_modules 削弱依赖严格性**
- bun 是 npm 式扁平 hoisting。本仓库有 `lint:imports`（`scripts/check-import-boundaries.mjs`）治理包边界，pnpm 的严格 node_modules 能在安装层挡住 phantom dependency；扁平化后这道防线失效，**治理成本上升**（需更依赖 lint 且 lint 要跟上）。

**D3. Nix 打包是短板**
- 现 `pnpmConfigHook` 成熟稳定；nixpkgs 对 bun lockfile 的构建支持仍在演进。发行链路切换的工作量大多在这，而不是在代码里。

**D4. 工具链生态仍住在 Node 上**
- vitest、tsdown、@changesets 自身跑在 node 兼容层。切 Bun 只是换宿主；`pool: 'bun'` 下部分高级 mock/模块拦截行为仍要验证。收益是「去掉 tsx + 提速」，不是「全栈革命」。

---

## 5. 收益-成本权衡

| 维度 | 挑战 | 收益 | 净判断 |
|------|------|------|--------|
| 终端/PTY | node-pty 不可用，需 bun 后端 | 移除最脆的原生依赖 | ✅ **净收益**（抽象层已备好） |
| session 索引 | `node:sqlite` 需抽象/条件导入 | `bun:sqlite` 更快、内置 | ✅ 净收益（改造成本一次） |
| 开发循环 | vitest 换 bun 池需验证 | 删 tsx + loader 三件套、脚本/CLI 启动快 | ✅ **净收益**（最大收益点） |
| 网络语义 | undici dispatcher 断裂（代理+SSRF） | — | ❌ **净成本**（安全相关，需重做 fetch 注入） |
| 依赖治理 | 扁平 node_modules | — | ❌ 净成本（lint 兜底） |
| Nix 发行 | 构建链重写 | 单文件二进制潜力 | ⚖️ 取决于发行目标 |
| 安装 | — | 与 pnpm 持平 | ➖ 中性 |

**总结**：收益集中在**开发循环**（R1/R2）与**简化原生依赖**（R3/R4）；成本集中在**网络语义**（3.1.3，安全相关）与**发行链路**（D3）。「内核代码切 bun 后端」与「整体切 bun 发行」是两个不同量级的工程，建议分开决策。

---

## 6. 影响面清单（文件级）

### 6.1 必须改动（硬阻塞）

| 文件 | 改动 |
|------|------|
| `packages/agent-core-v2/src/os/backends/node-local/hostTerminalService.ts` | 新增 `os/backends/bun-local/` 后端，用 `Bun.spawn({pty:true})` 实现 `IHostTerminalService` |
| `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionStore.ts` | 抽象 sqlite 接口或条件导入 `node:sqlite`/`bun:sqlite` |
| `packages/agent-core-v2/src/app/web/providers/local-fetch-url.ts` | 重新实现 SSRF 防护（不依赖 undici `Agent` 定制 lookup） |
| `packages/agent-core-v2/src/_base/utils/proxy.ts` | 代理注入改为显式传给 SDK 的自定义 fetch |
| `packages/agent-core-v2/src/kosong/provider/bases/*`（openai/anthropic/google-genai） | SDK 构造时注入自定义 fetch |
| `packages/agent-core-v2/src/index.ts` | OS 后端注册点，按运行时选择 `node-local`/`bun-local` |

### 6.2 构建/发行/测试

- `packages/agent-core-v2/package.json`（scripts 去 tsx/loader、依赖调整）
- `build/raw-text-loader.mjs`、`build/raw-text-plugin.mjs`、`build/register-raw-text-loader.mjs`（删除）
- `packages/agent-core-v2/vitest.config.ts`（bun 池）
- `packages/agent-core-v2/test/setup.ts`（全局假设核查）
- `flake.nix`（bun 构建链）、`apps/mirri-code/scripts/native/`、`scripts/fix-node-pty-perms.mjs`（node-pty 移除）
- `.nvmrc`、engines、`.npmrc`（`engine-strict`）、根 `package.json`（`packageManager`）、CI（`setup-bun`）
- 涉及 `@mirri-ai/*` workspace 依赖（`agent-profile`/`minidb`/`protocol`/`tree-sitter-bash`/`v2-oauth`）的 Bun 兼容验证

### 6.3 需回归验证（兼容风险区）

- `agent/tools/os/bash/*`（进程 + stream）
- `app/sessionExport/*`（zip/readline）
- `mcpCore/oauth/callback-server.ts`（http 回调）
- `agent/media/*`（wasm 解码、jimp 压缩）
- `os/backends/node-local/tools/rgLocator.ts`（`createHash` + ripgrep 定位）

---

## 7. 建议迁移路径（分阶段，待决策）

> 以下为建议路线，非既定计划；每阶段有独立验收标准，可单独叫停。

- **Phase 0 — 决策验证（约 1 天）**：在 Bun 下跑最小 smoke——vitest `pool: 'bun'` 跑 `test/` 关键套件；跑通 `gen:*` 脚本；验证 `bun:sqlite` 打开现有 session 索引文件。产出：兼容性实测数据，用于决策。
- **Phase 1 — 内核可运行（约 1 周）**：新增 `bun-local` OS 后端（terminal 优先）；sqlite 抽象；fetch/代理统一注入。验收：`agent-core-v2` 测试全绿（bun 池），bash 工具、zip 导出、OAuth 回调、SSRF 用例通过。
- **Phase 2 — 构建与发行（约 1 周）**：脚本基建清理（tsx/loader 删除）；评估 `bun build` 替代 tsdown；重写 `flake.nix`。验收：`bun build --compile` 产物可跑通核心会话流程。
- **Phase 3 — 全量测试与治理（约 3 天）**：全仓测试（含 `apps/mirri-code` e2e）、依赖边界 lint 强化、性能回归对比（冷启动/session list/PTY）。

---

## 8. 验证清单

- [ ] vitest `pool: 'bun'` 下 `packages/agent-core-v2` 全量测试通过
- [ ] `test/setup.ts` 无 node 专属全局假设
- [ ] bash 工具：非交互命令、交互式/PTY 命令（resize/kill 时序）、`loginShellPath` 探测
- [ ] `app/sessionExport`：zip 打包 + readline 扫描
- [ ] `mcpCore/oauth/callback-server`：回调起停、端口绑定
- [ ] `local-fetch-url`：SSRF `BlockList` 拦截仍生效（本地/内网地址被拒）
- [ ] HTTP 代理：配置代理后模型请求走代理（SDK 注入 fetch 生效）
- [ ] 模型流式：openai/anthropic/genai 三个基类 SSE 流式完整跑通
- [ ] `bun:sqlite` 打开既有 session 索引文件、`hasStateMtimeColumn` 迁移探测正常
- [ ] wasm WebP 解码 + jimp 图片压缩链路
- [ ] 全仓：`typecheck`、`lint:imports`、关键包测试
- [ ] 发行：`bun build --compile` 产物 + Nix 构建（若切发行）

---

## 9. 决策建议

1. **若目标 = 加速开发循环 + 简化原生依赖**：收益明确（R1/R2/R3/R4），成本集中在 §3.1.3（网络语义，安全相关）——建议**先做 Phase 0/1**，用实测数据背书后再推进。
2. **若目标 = 整体切 Bun 发行运行时**：收益主要在分发形式（R5），成本在 Nix 构建链重写（D3）+ 网络语义重做（3.1.3）——建议**单独立项**，与内核迁移解耦。
3. **无论哪种目标**：§3.1.3 的 undici→fetch 语义问题是**安全相关**且最容易静默失效的，应在任何切换中优先处理并补齐回归用例。

---

## 10. 附：参考资料

- `packages/agent-core-v2/package.json`（依赖清单）
- `packages/agent-core-v2/src/os/interface/*`（OS 抽象层五域接口）
- `.internal-docs/agent-core-v2/sqlite-session-index-design.md`（`node:sqlite` 冷启动索引设计）
- `scripts/check-import-boundaries.mjs`（依赖边界治理）
- `flake.nix`（发行构建链，`pnpmConfigHook`）

---

## 11. 参考实现：claude-code-local（cc-haha）

> 调研对象：`/Users/chengri/workspace/opensource/cc-haha`，一个 708K LOC（src 2561 文件）的 Claude Code 本地重实现，
> 整栈运行在 Bun 上。作为「同类型、同规模的 agent CLI 在 Bun 上生产运行」的实证样本，用于校准本报告的挑战与收益判断。

### 11.1 技术栈要点

| 维度 | 实现 |
|------|------|
| 运行时 | CLI 运行时 Bun 1.3.14 独占（`bun.lock` + `bunfig.toml` preload + `bun --no-env-file run`）；桌面端例外见下行 |
| TUI | Ink + React 19（与 mirri-code 同路线） |
| 测试 | 完全用 `bun test` 原生 runner，**无 vitest** |
| 网络 | axios + `HttpsProxyAgent` + 惰性 undici + ws + vscode-jsonrpc（LSP bridge） |
| 原生兼容 | `stubs/` + tsconfig `paths` 别名，把 Bun 下不可用的原生模块 stub 掉（`color-diff-napi`、`ant-claude-for-chrome-mcp`） |
| OS 辅助 | 原生部分用 Python 脚本（`runtime/mac_helper.py`、`win_helper.py`），非 Rust、非原生插件 |
| 桌面端 | **Electron 42**（main 进程跑 Electron 自带 Node，非 Bun）+ React 18 + Vite + **Tauri 2/Rust sidecar**（`portable-pty`/`reqwest`/shell）；Bun 仅做构建编排（`bun build --target node` + `bun run`） |
| 编译期特性开关 | `import { feature } from 'bun:bundle'`，200 个文件在用 |

### 11.2 对本报告的校准（正面）

- **可行性背书**：一个 708K LOC（比 `agent-core-v2` 大 7 倍）的生产 agent CLI 已在 Bun 1.3 上跑通 CLI + server + 流式 + 权限 + 工具全链路；测试基础设施全部换 `bun test`——**直接绕开 §3.2 的 vitest bun pool 顾虑**。
- **§3.1.3（undici 断裂）的现成答案**：它不用 undici `setGlobalDispatcher` 全局注入，改走 axios + `HttpsProxyAgent` 显式传 agent，undici 仅惰性加载用于特定场景；注释中明确记录 Bun fetch 语义差异（`keepalive:false` 池化行为在 Bun 下有效）。落地写法 = 显式注入 + 注释记录平台差异。
- **启动优化模式（对应 R2）**：大依赖惰性加载（AWS SDK ~929KB、undici ~1.5MB 全部 `await import()`），`bun:bundle` 编译期 flags 配合 `bun build` 死代码消除。启示：Bun 迁移后 CLI 冷启动还能再压一截，而不只是吃运行时启动优势。
- **原生模块降级模式**：`stubs/` + tsconfig `paths` 把不可用原生模块降级为空实现。mirri-code 的 node-pty 功能必需不能 stub，但该思路可用于迁移时按功能价值分级处理其余原生依赖。
- **测试/CI 实践（与运行时无关，可直接借鉴）**：
  - `check:impact` 导入感知测试路由（改动路由到所有 import 它的 surface，非仅本目录）
  - `check:agent-flow` 确定性 e2e（mock SDK 驱动真实 server + WebSocket，走完会话/流式/权限/工具失败/中断/重连全链路，零网络零凭据）
  - `sandbox.ts` 沙箱 config 目录（测试绝不触碰真实用户配置）
  - 测试原则：驱动转换而非手写状态、断言不变量而非当前输出、测连接点而非两端、丢弃/合并规则双向覆盖
- **Bun 特有坑已踩平**：`preload.ts` 注入版本宏、`--no-env-file` 规避 Bun 自动读 `.env` 的惊喜。

### 11.3 覆盖空白与反面教材

- **PTY 只在 Node/Rust 侧被验证**：CLI（Bun）无 PTY 功能；**桌面端用 node-pty（Electron 的 Node 进程内，`electron/main.ts` 的 `ElectronTerminalService`）+ portable-pty（Tauri/Rust sidecar）**——node-pty 在 Electron/Node 下正常（且复现了 mirri-code `fix-node-pty-perms.mjs` 同款的 spawn-helper 权限修复，见 `desktop/scripts/prepare-node-pty.ts`）。**Bun 运行时下的 node-pty 可用性仍无实证**——§3.1.1 的 terminal 硬点不因该参考实现消解。
- **「换 Bun」≠「去掉所有原生层」**：桌面端保留 Electron（Node）+ Tauri（Rust）双原生栈，OS 辅助另有 Python 脚本。mirri-code 若上 desktop 同理需单独规划原生层。
- **反面教材**：200 文件深度绑定 `bun:bundle` 专有 API + `main.tsx` 788KB 单文件。mirri-code 有上游同步约束，应避免业务逻辑写进 Bun 专有 API，防止迁移成本单向化。
- **架构自由度差异**：它是独立重实现，无上游同步负担；mirri-code 的约束不同，不能照搬"随意改架构"的路径。
