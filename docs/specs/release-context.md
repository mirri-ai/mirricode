# 发布域 现状盘点

> 快照日期：2026-10-18。本文件只记录当前状态与痛点，不含目标与决策。
> 目标架构见 [`release-architecture.md`](./release-architecture.md)。

## 版本与发布机制现状

- 版本源：`apps/mirri-code/package.json`（`@mirri-ai/mirri-code` 1.11.0，public）
- `rust/scripts/sync-version.mjs` 以 npm 包版本为**源**，单向同步写入 rust
  workspace / Cargo.lock，含 `--check` 漂移检测（CI 运行）
- 发布入口：push 到 main 触发 `.github/workflows/release.yml`
- changesets/action 负责 release PR（`ci: release packages`）与 npm publish；
  `.changeset/config.json` ignore 了 4 个包（server-e2e、vis、vis-server、vis-web）
- 当前会被 npm 发布的包仅两个：`@mirri-ai/mirri-code`（CLI）、`@mirri-ai/mirri-node`
- 另有 `.github/workflows/pkg-pr-new.yml` 向 pkg.pr.new 发布 PR 预览 npm 包
  （不依赖 npm registry）

## 发布触发链现状

- `apps/mirri-code/scripts/native/resolve-release.mjs`：仅当 `publishedPackages`
  包含 `@mirri-ai/mirri-code` 时输出 `should_publish=true` 与 release tag
- `native-artifacts` job 调用 `_native-build.yml` —— **Node.js SEA 管道**
  （`apps/mirri-code/scripts/native/01-bundle.mjs` ~ `05-verify.mjs`），产出
  `mirri-code-<target>.zip` 上传 Release；**6 目标矩阵**（含 linux-arm64、
  win32-arm64）
- rust 二进制管道 `.github/workflows/rust-native-build.yml`（经
  `rust/scripts/release-package.mjs` 打 zip）只有 workflow_call/dispatch 触发，
  **release.yml 中无调用方，不在发布链上**
- `desktop-artifacts` job（`desktop-build.yml`）→ 桌面安装包，**gate 在 npm 发布
  成功上**（`needs release` 且 `mirri_native_release == 'true'`）；4 目标矩阵
- `publish-native-assets` job：仅上传 CLI 原生 zip + `manifest.json` 到 GitHub
  Release；**桌面安装包无任何分发渠道**，只作为 CI artifact 保留 7 天
- `deploy-docs` job（`docs-deploy.yml`）**gate 在 npm 发布事件上**；其版本端点
  （`latest.json`，部署于 GitHub Pages）读取 `apps/mirri-code/package.json` 的
  version——CLI 自更新与 install 脚本的在线版本源

## 桌面打包现状

- `apps/mirri-desktop`（`@mirri-ai/mirri-desktop` 0.2.0，private）：
  - `scripts/build-backend.mjs`：构建 web SPA → `rust/scripts/stage-web.mjs` 嵌入 →
    `cargo build --profile dist -p mirri-cli` → 拷贝 `mirri` 二进制到
    `apps/mirri-desktop/build/backend/<platform>-<arch>/`
  - `scripts/before-pack.cjs` + `electron-builder.config.cjs`：二进制经 extraResources
    打进 Electron 包（`<resources>/bin/`），运行时由 `src/main/server-binary.ts` 解析
- 支持矩阵：macOS（签名 + notarize，与 CLI 共用 Apple secrets）、Windows、Linux；
  Win/Linux 未签名
- 产物命名当前为内部测试格式（`MCD-beta-alpha-crazy-internal-v50-…`）：无版本号、
  含构建日期，不可直接对外分发
- 自动更新：`electron-updater` 仅为 devDep，`publish` 配置为占位 URL
  （`https://updates.invalid/mirri-desktop`），`src/main/updater.ts` 标注
  「仅技术就绪，无真实通道」——此为先前刻意留下的决策门

## ACP 现状

- rust 侧 `rust/crates/mirri-acp`：产出 `mirri-acp` 二进制（另有测试专用
  `mirri-acp-echo-mcp`），**无任何发布/打包链路**
- TS 侧 `@mirri-ai/acp-adapter`（0.3.9）、`@mirri-ai/acp-server`（0.1.0）均为
  private，从未发布过 npm

## npm 存量用户现状

- 存量 npm 安装的 CLI 定期检查在线版本端点（`mirricode.com/latest.json`，由
  docs-deploy 产出），检测到新版本后提示 `npm install -g @mirri-ai/mirri-code@<version>`
  （按安装来源给出 npm/pnpm/yarn/bun 对应命令，见
  `apps/mirri-code/src/cli/update/preflight.ts`）
- install 脚本（`docs/public/install.sh` / `install.ps1`）的 tag 定位硬编码
  `包名@版本` 前缀，但对 `v` 前缀 tag 存在 fallback 分支；manifest 消费依赖
  `platforms.<target>.filename/checksum` 结构

## 痛点清单

1. 桌面安装包无分发渠道（CI artifact 7 天即逝），且产物命名不可对外
2. 桌面构建被 CLI npm 发布节奏 gate，无法独立成发布物
3. 版本语义倒挂：产品重心已是桌面+rust，版本源却挂在即将退役的 npm CLI 包上
4. `mirri-acp` 二进制无打包与分发
5. rust 二进制管道未接入发布链（当前发的是 Node SEA 产物）
6. 桌面版本 0.2.0 与 npm 1.11.0 / rust workspace 脱节，无统一版本语义
7. 自动更新无真实通道（占位 URL）
8. Win/Linux 安装包未签名（SmartScreen / 系统警告）
9. 文档站部署与在线版本端点 gate 在 npm 发布事件上，npm 停发即静默断链
