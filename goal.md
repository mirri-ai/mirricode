# Mirri Code — 重构目标文档

> 创建时间：2026-07-07
> 状态：进行中

---

## 1. 背景

Mirri Code 是一个 AI 编程助手产品。为了快速获得与 kimi-code 同等的用户体验和功能完整度，决定以 kimi-code 代码库为基础进行 fork，而非从零开发。

### 决策记录

| 决策项 | 选择 | 理由 |
|--------|------|------|
| 重构方式 | Fork kimi-code | 最快路径，~2-4 周 vs 3-6 个月 |
| 功能范围 | 完全对齐 kimi-code | 全功能 parity |
| 现有代码 | 归档到 mirricode-legacy/ | 保留参考价值 |
| 运行时 | Node.js + pnpm（对齐 kimi-code） | 放弃 Bun，统一工具链 |
| TUI 框架 | pi-tui（对齐 kimi-code） | 放弃 Ink，统一体验 |
| 架构 | Monorepo（packages/ + apps/） | 对齐 kimi-code 包结构 |
| 独有功能 | 移除 | IM 适配器、Computer Use、Bridge、高级记忆系统暂不保留 |
| Kimi provider | 作为第三方 provider 保留 | 用户可通过 `/provider add` 添加 |

---

## 2. 已完成的工作

### 2.1 代码库迁移

- [x] 归档旧 mirricode → `mirricode-legacy/`
- [x] Clone kimi-code → `mirricode/`（保留 git 历史）
- [x] 设置 remote → `https://github.com/mirri-ai/mirricode.git`
- [x] 重建 .git 目录（独立于 kimi-code 历史）

### 2.2 品牌重命名

- [x] 包作用域：`@moonshot-ai/*` → `@mirri-ai/*`
- [x] 应用名称：`kimi-code` → `mirri-code`、`kimi-web` → `mirri-web`、`kimi-desktop` → `mirri-desktop`
- [x] CLI 二进制：`kimi` → `mirri`
- [x] 配置路径：`.kimi/` → `.mirricode/`、`~/.kimi/` → `~/.mirricode/`
- [x] 环境变量：`KIMI_CODE_*` → `MIRRICODE_*`
- [x] 显示文本：`Kimi Code` → `Mirri Code`、`Moonshot AI` → `Mirri AI`
- [x] GitHub URL：`MoonshotAI/kimi-code` → `mirri-ai/mirricode`
- [x] Plugin manifest：`kimi-plugin.json` → `mirri-plugin.json`
- [x] 目录重命名：`apps/kimi-*` → `apps/mirri-*`

### 2.3 Kimi Provider 处理

- [x] 从 kosong 移除内置 Kimi provider（删除 kimi.ts、kimi-schema.ts、kimi-files.ts）
- [x] 更新 ProviderTypeSchema 移除 'kimi' 类型
- [x] 更新 provider factory 移除 kimi 分支
- [x] 更新 catalog.ts 移除 kimi wire type
- [x] 将 Kimi/Moonshot 平台改为 'openai' 类型（OpenAI 兼容 API）
- [x] Kimi 作为已知第三方 provider 保留（通过 `/provider add` 添加）

### 2.4 OAuth 包适配

- [x] 重命名 `managed-kimi-code.ts` → `managed-mirri-code.ts`
- [x] 更新 `ManagedKimiCodeProtocol` 类型：`'kimi'` → `'openai'`
- [x] 更新 `applyOpenPlatformConfig` 使用 'openai' 类型
- [x] 更新 `refreshProviderModels` 检查 'openai' 类型

### 2.5 Node SDK 适配

- [x] 重命名 `kimi-code-model-provider.ts` → `mirri-code-model-provider.ts`
- [x] 重命名 `kimi-harness.ts` → `mirri-harness.ts`
- [x] 更新 `KimiForCodingProvider` 使用 'openai' 类型
- [x] 更新所有 import 路径

### 2.6 Agent Core 适配

- [x] 更新 `env-model.ts`：`KIMI_MODEL_*` → `MIRRICODE_MODEL_*`
- [x] 简化 `kimi-env-params.ts` 中的 Kimi 特定函数为 no-op
- [x] 更新 `llm-request-recorder.ts` 移除 KimiChatProvider 引用
- [x] 更新 `provider-manager.ts` 移除 kimi provider 分支
- [x] 更新 `modelCatalogService.ts` 移除 kimi 分支

### 2.7 构建系统

- [x] 创建 `build.sh` quality gate 脚本
- [x] 更新 `.npmrc` 使用官方 npm registry
- [x] 配置 Electron 缓存（`~/Library/Caches/electron/`）
- [x] 更新 `electron-builder.config.cjs`：MCD 命名
- [x] 更新 `before-pack.cjs`：mirri-code 路径和 mirri 二进制名

### 2.8 文档

- [x] 更新 `AGENTS.md` 适配新 monorepo 结构
- [x] 更新技术栈描述（Node.js + pnpm + Vitest + pi-tui）

### 2.9 测试修复

- [x] 修复 test files 中的 provider type：`'kimi'` → `'openai'`
- [x] 修复 env-model tests 使用 `MIRRICODE_MODEL_*` env vars
- [x] 修复 config-state tests 期望 no-op 行为
- [x] 删除 kosong 中的 kimi 专用测试文件

### 2.10 验证结果

| 组件 | 状态 |
|------|------|
| `pnpm install` | ✅ 通过 |
| CLI 构建 | ✅ 通过（13.43MB） |
| Native SEA 构建 | ✅ 通过（157MB） |
| Desktop DMG 构建 | ✅ 通过（147MB） |
| `./mirri --version` | ✅ 输出 `0.23.1` |
| 测试 | ⚠️ 102 failed / 9123 passed |
| 类型检查 | ⚠️ api-extractor 兼容性问题 |

---

## 3. 剩余工作

### 3.1 修复测试（102 个失败）— 高优先级

**目标：** `pnpm run test` → 0 failed

| 类别 | 数量 | 原因 | 修复方式 |
|------|------|------|---------|
| kosong 测试 | 3 | finish-reason、select-tools、toolchain-bridges 依赖已删除的 Kimi provider | 删除或改写为通用 provider 测试 |
| kimi-sdk 测试 | 4 | OAuth + provider identity 测试仍期望 `type: 'kimi'` | 更新期望值为 `type: 'openai'` |
| agent-core agent 测试 | ~76 | agent loop、permission、plan、turn 测试中 mock 数据仍引用 Kimi | 批量替换 mock 数据中的 provider 类型 |
| Snapshot 失败 | 19 | 输出格式变化导致 snapshot 不匹配 | `pnpm run test -- --update` 更新 |

**验证命令：**
```bash
cd /Users/chengri/workspace/opensource/mirri-ai/mirricode
pnpm run test
```

### 3.2 修复类型检查 — 中优先级

**目标：** `pnpm run typecheck` → 0 errors

- api-extractor 7.58.7 与 TypeScript 6.0.2 不兼容
- 影响：`packages/node-sdk` 的 DTS 生成失败
- 修复方式：升级 api-extractor 或降级 TypeScript

**验证命令：**
```bash
cd /Users/chengri/workspace/opensource/mirri-ai/mirricode
pnpm run typecheck
```

### 3.3 清理残留 Kimi 引用 — 中优先级

**目标：** 源码中无 Moonshot/Kimi 残留引用（OAuth 包除外）

需要清理的位置：
- 文档中的 "Kimi Code"、"Moonshot" 引用
- 注释和变量名中的 kimi 命名
- `.changeset/` 目录下的旧 changeset 文件
- GitHub Actions workflow 中的旧引用

**验证命令：**
```bash
grep -r "moonshot\|MoonshotAI\|Kimi Code" \
  --include="*.ts" --include="*.md" --include="*.json" \
  packages/ apps/ | grep -v node_modules | grep -v oauth
```

### 3.4 配置默认 Provider 体验 — 中优先级

**目标：** 无 API key 时启动 CLI 显示友好引导

需要实现：
- 启动时检测环境变量（`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`MIRRICODE_API_KEY` 等）
- 如果无配置，显示提示引导用户通过 `/provider add` 添加
- 支持 `MIRRICODE_API_KEY` + `MIRRICODE_BASE_URL` 快速配置 OpenAI 兼容 provider

**验证方式：** 清除所有 API key 环境变量，启动 CLI，确认显示友好提示

### 3.5 更新 CI/CD — 低优先级

**目标：** GitHub Actions pipeline 可以跑通

需要更新：
- `.github/workflows/ci.yml` — 适配新 monorepo 结构
- `.github/workflows/desktop-build.yml` — 适配 MCD 命名
- `.github/workflows/release.yml` — 适配新包名

**验证方式：** Push 到 GitHub 后 CI 自动跑通

---

## 4. 执行顺序

```
Phase 1: 质量保障
  ├── 3.1 修复测试（阻塞其他所有工作）
  └── 3.2 修复类型检查

Phase 2: 代码质量
  └── 3.3 清理残留引用

Phase 3: 用户体验
  └── 3.4 配置默认 Provider

Phase 4: 自动化
  └── 3.5 更新 CI/CD
```

---

## 5. 关键文件索引

| 文件 | 用途 |
|------|------|
| `packages/kosong/src/providers/index.ts` | Provider 工厂（已移除 Kimi） |
| `packages/kosong/src/catalog.ts` | Wire type 定义 |
| `packages/agent-core/src/config/schema.ts` | ProviderTypeSchema |
| `packages/agent-core/src/config/env-model.ts` | MIRRICODE_MODEL_* 环境变量 |
| `packages/oauth/src/open-platform.ts` | Kimi 第三方平台定义 |
| `packages/oauth/src/managed-mirri-code.ts` | Managed provider 配置 |
| `packages/node-sdk/src/mirri-code-model-provider.ts` | KimiForCodingProvider |
| `apps/mirri-desktop/electron-builder.config.cjs` | Desktop 构建配置 |
| `apps/mirri-desktop/scripts/before-pack.cjs` | Desktop 打包脚本 |
| `build.sh` | Quality gate 脚本 |
| `AGENTS.md` | AI agent 工作规范 |

---

## 6. 环境变量参考

| 变量 | 用途 | 示例 |
|------|------|------|
| `MIRRICODE_API_KEY` | 快速配置 OpenAI 兼容 provider | `sk-xxx` |
| `MIRRICODE_BASE_URL` | 快速配置 API base URL | `https://api.openai.com/v1` |
| `MIRRICODE_MODEL_NAME` | 快速配置模型名 | `gpt-4o` |
| `MIRRICODE_MODEL_API_KEY` | 模型专用 API key | `sk-xxx` |
| `MIRRICODE_MODEL_PROVIDER_TYPE` | Provider 类型 | `openai` / `anthropic` / `google-genai` |
| `ANTHROPIC_API_KEY` | Anthropic provider | `sk-ant-xxx` |
| `OPENAI_API_KEY` | OpenAI provider | `sk-xxx` |
| `GOOGLE_API_KEY` | Google GenAI provider | `AIza-xxx` |
| `ELECTRON_CACHE_DIR` | Electron 下载缓存 | `~/Library/Caches/electron` |
| `ELECTRON_MIRROR` | Electron 下载镜像 | `https://npmmirror.com/mirrors/electron/` |
