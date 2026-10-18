# 发布域 ROADMAP

> 目标架构见 [`release-architecture.md`](./release-architecture.md)，当前状态盘点见
> [`release-context.md`](./release-context.md)。本文件只做执行状态管理。

## 阶段划分

### 阶段 1：版本锚落地

- [x] 新建 `packages/release`（`@mirri-ai/release`，private，version 从 1.11.0 起跳）
      与 README（说明用途：版本锚，不是可安装包）
- [x] `flake.nix` 手工补 `workspacePaths` / `workspaceNames`（glob 已覆盖
      `pnpm-workspace.yaml`，nix 侧必须手补；check 脚本查不出锚包这种闭包外包）
- [x] 版本同步脚本改造：读锚包，写 rust workspace + desktop，保留漂移检测
- [x] `.changeset/config.json` 增加 `privatePackages: { version: true, tag: false }`
- [x] 存量/进行中 changeset 的指向改为锚包

### 阶段 2：npm 退役与触发器重造

- [x] `.changeset/config.json`：`@mirri-ai/mirri-code`、`@mirri-ai/mirri-node` 移入 ignore
- [x] npm 发布面退役清单：
  - `apps/mirri-code` 摘除 publishConfig / provenance
  - 根 `package.json` 的 `publish` script（changeset publish 链）与 `lint:pkg`
    （publint/attw 检查）退役
  - `release.yml` 的 `id-token: write`（npm OIDC）与 changesets/action 的
    `publish:` 入参摘除
  - `.github/workflows/pkg-pr-new.yml` 退役（预览 npm 包的前提已消失）
- [x] 发布触发判定改造：锚包版本变化检测（含冷启动规则：无历史 tag 不触发）
- [x] Release 创建：tag `vX.Y.Z`，notes 取锚包 CHANGELOG 当次条目，已存在则复用
- [x] `deploy-docs` 脱离 npm gate，改为锚包版本变化触发；版本端点 `latest.json`
  冻结在最后一个 npm 版本（不再前进，ADR-005）

### 阶段 3：产物矩阵补全

- [x] CLI 原生产物切换为 rust 二进制（ADR-004）：release 编排改调
      `rust-native-build.yml`，SEA 管道退出发布链
- [x] `mirri-acp` 打包脚本（对齐 zip+sha256 模式）+ 构建矩阵接入 release 编排
- [x] `desktop-artifacts` 脱离 npm gate，改为锚包版本变化触发
- [x] 桌面产物命名规范化（含版本与 target，替换内部测试命名）
- [x] `publish-native-assets` 扩展：上传桌面安装包 + ACP zip
- [x] `manifest.json` 扩展为三类产物清单（`platforms` 段 schema 冻结，新产物走
      新增顶层段）
- [x] install.sh / install.ps1 的 TAG_PREFIX 显式改为 `v` 前缀（并保留旧格式
      fallback）；版本检查改读 Release manifest

### 阶段 4：文档与指南更新

- [x] 面向贡献者：根 `README` / `CONTRIBUTING` 的下载与安装指引改为桌面优先；
      发布流程说明（changeset → release PR → 产物）
- [x] 面向 agents：根 `AGENTS.md` 发布相关条目改写（npm 停发、版本锚、
      「新引擎特性必须 rust first」的发布侧对应）
- [x] 用户文档：`docs/zh` / `docs/en` 的安装/下载页指向 GitHub Release 桌面包；
      增加 npm 存量用户迁移指引（走 `translate-docs` skill 同步双语）

### 验收（端到端）

- [ ] 一次真实发布演练：合入一个指向锚包的 changeset → release PR → 合入 →
      GitHub Release 出现桌面 + rust CLI + ACP 全部产物 + manifest.json
- [ ] 版本漂移检测在 CI 常规检查中绿
- [ ] 存量 npm 安装的 CLI 不再收到指向不存在版本的升级提示（版本端点冻结生效）

## 范围外跟踪项（另立门户）

- 桌面自动更新通道（electron-updater 真实 feed）
- Windows / Linux 代码签名
- npm 告别版（deprecated 标记）
