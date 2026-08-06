# kimi 生态插件清单兼容支持设计

**日期**：2026-08-06
**范围**：`packages/agent-core`（v1 引擎）、`packages/agent-core-v2`（v2 引擎）、`apps/mirri-code/scripts`、`docs/{zh,en}/customization/plugins.md`
**目标**：让 Mirri 能识别并安装只携带 kimi 生态清单名（`kimi.plugin.json` / `.kimi-plugin/plugin.json`）的插件，例如 `https://github.com/obra/superpowers`

---

## 1. 背景与问题

### 1.1 现象

用户执行插件安装（GitHub 仓库来源）时失败：

```
✗ Install failed: [internal] Cannot install plugin from https://github.com/obra/superpowers:
  No manifest at mirri-plugin.json, .mirricode-plugin/plugin.json, mirri.plugin.json, or .mirri-plugin/plugin.json
```

错误文本来自身 `packages/agent-core/src/plugin/manifest.ts`——插件清单搜索只认 4 个 Mirri 品牌文件名。

### 1.2 根因

- 目标插件 superpowers 仓库根目录只携带各 Agent 生态的清单，其中 kimi 生态是 **`.kimi-plugin/plugin.json`**（内容为 `skills: "./skills/"`、`sessionStart.skill: "using-superpowers"`、`skillInstructions` 等 kimi 风格字段）。
- fork/改名（git 提交 `14220bf` "complete kimi to mirri rebrand"）时，把 kimi 原生清单名从探测列表中删除，导致 kimi 生态插件在 Mirri 下全部识别失败。
- 改名前的 kimi-code 原生支持成对命名：`kimi.plugin.json`（root）与 `.kimi-plugin/plugin.json`（dir）。

### 1.3 现有代码面貌

| 文件 | 现有候选名 | manifestKind 枚举 |
|---|---|---|
| `agent-core/src/plugin/manifest.ts` + `archive.ts`（v1） | `mirri-plugin.json`、`.mirricode-plugin/plugin.json`、`mirri.plugin.json`、`.mirri-plugin/plugin.json` | `'mirri-plugin-root' \| 'mirri-plugin-dir'` |
| `agent-core-v2/src/app/plugin/manifest.ts` + `archive.ts`（v2） | `mirri.plugin.json`、`.mirri-plugin/plugin.json` | `'kimi-plugin-root' \| 'kimi-plugin-dir'` |
| `packages/klient/src/contract/global/plugins.ts`（wire 契约） | — | `'kimi-plugin-root' \| 'kimi-plugin-dir'` |
| `apps/mirri-code/scripts/plugin-manifest-version.mjs` | 已读 `mirri.plugin.json`、`.kimi-plugin/plugin.json`（顺序与真实探测规则不一致） | — |

安装链路（v1 为例）：`PluginManager.install()` → GitHub 源下载 zip → `archive.extractZip()` 内 `detectPluginRoot()`（只看 `mirri-plugin.json` / `.mirricode-plugin/plugin.json`）→ `parseManifest()`（4 名兜底搜索）。两处搜索集合不一致导致：即使 zip 内只有 `.kimi-plugin/plugin.json`，根目录探测和清单解析都会失败。

## 2. 目标与非目标

### 2.1 目标

- v1 / v2 两引擎的**清单发现**（`parseManifest`）与 **zip 根目录探测**（`hasManifest`）均能识别 kimi 两件套。
- 更新 "No manifest at ..." 错误文案，列出全部候选名。
- 更新 `plugin-manifest-version.mjs` 的探测顺序与真实规则一致。
- 补测试（两引擎的 manifest / archive 用例，覆盖优先级、kind、错误文案）。
- 更新用户文档（zh/en）。

### 2.2 非目标

- **不**改变 klient wire 契约的 `manifestKind` 枚举（已含 kimi 两个值）。
- **不**顺手修复「v1 引擎 kind 值（`mirri-plugin-*`）与 klient 契约（`kimi-plugin-*`）不一致」这一既有改名残留——与本次功能无关，保持改动聚焦。
- **不**添加其他 Agent 生态的清单别名（如 `.claude-plugin`、`.codex-plugin` 等），即使 superpowers 仓库也包含它们。
- 不改变镜像：kimi 清单仅在全部 Mirri 名缺失时才被采用。

## 3. 设计决策

### 3.1 清单候选名与优先级（用户已确认）

两引擎共用的 6 名全局优先级（各引擎按其现有候选集 + kimi 两件套计算）：

| 排名 | 清单文件 | 形态 | 说明 |
|---|---|---|---|
| 1 | `mirri-plugin.json` | root | 现行 |
| 2 | `mirri.plugin.json` | root | legacy |
| 3 | `.mirri-plugin/plugin.json` | dir | legacy |
| 4 | `.mirricode-plugin/plugin.json` | dir | 现行 |
| 5 | `kimi.plugin.json` | root | compat |
| 6 | `.kimi-plugin/plugin.json` | dir | compat |

规则：

- **root 形态一律优先于 dir 形态**（第 1–2、5 项 > 第 3–4、6 项），与 v2 现有“root 优先”逻辑一致。
- 品牌优先级：Mirri 名（1–4）优先于 kimi compat（5–6）。
- 找不到任一候选时错误文案列出全部 6 个文件名。
- 「shadowed」语义不变：仅在**同形态同名**成对（如 `mirri.plugin.json` 与 `.mirri-plugin/plugin.json` 并存）时报 shadowed；Mirri 与 kimi 并存不属于 shadowed，直接采用前者。

> 注：现有 v1 代码的搜索顺序是「按常量声明顺序交替排 root/dir」（`.mirricode-plugin/plugin.json` 排在 `mirri.plugin.json` 之前），本次以表为准调整。

### 3.2 manifestKind

- 清单被解析出的类型元数据，取值形如「品牌 + 形态」：`<brand>-plugin-root` / `<brand>-plugin-dir`。
- v1 枚举 `PluginManifestKind`（`agent-core/src/plugin/types.ts`）追加 `'kimi-plugin-root' | 'kimi-plugin-dir'` 两个值——这两个值已存在于 klient wire 契约，TUI 插件状态面板（`plugins-status-panel.ts` 的 `Manifest: <path> (<kind>)`）可直接显示。
- v2 枚举已包含 kimi 两个值，无需改动。
- 归类规则：选中 `kimi.plugin.json` → `kimi-plugin-root`；选中 `.kimi-plugin/plugin.json` → `kimi-plugin-dir`；其余沿用现值。

### 3.3 双引擎一致

两引擎的 `manifest.ts` + `archive.ts` 同步修改：

- 仅改 v1 则 v2 引擎启用后同一问题复发；
- 两引擎各自维持「现有候选集 + kimi 两件套」，按 3.1 全局优先级排序；**不合并 v1/v2 既有的候选集差异**（如 v1 多出的 `mirri-plugin.json` / `.mirricode-plugin/plugin.json`），避免无关重构。

### 3.4 发布脚本一致性

`apps/mirri-code/scripts/plugin-manifest-version.mjs` 目前按 `['mirri.plugin.json', '.kimi-plugin/plugin.json']` 顺序读取版本；改为与 3.1 一致的 6 项顺序（或至少补齐 `kimi.plugin.json`），并修正注释中“镜像 loader 优先级”的描述。

## 4. 变更点清单

| 文件 | 改动 |
|---|---|
| `packages/agent-core/src/plugin/manifest.ts` | 新增 2 个 kimi 常量；搜索/优先级；错误文案；kind 归类 |
| `packages/agent-core/src/plugin/types.ts` | `PluginManifestKind` 追加 kimi 两个值 |
| `packages/agent-core/src/plugin/archive.ts` | `hasManifest` 增加 `.kimi-plugin/plugin.json`（可复用 manifest.ts 导出的候选名列表） |
| `packages/agent-core-v2/src/app/plugin/manifest.ts` | 新增 2 个 kimi 候选常量；优先级；错误文案；kind 归类 |
| `packages/agent-core-v2/src/app/plugin/archive.ts` | `hasManifest` 增加 kimi 候选 |
| `apps/mirri-code/scripts/plugin-manifest-version.mjs` | 顺序对齐 3.1 |
| `docs/zh/customization/plugins.md`、`docs/en/customization/plugins.md` | 清单位置章节补充 kimi 兼容名 |

若两引擎都导出「候选文件名列表」供 `archive.ts` 复用，可避免搜索集合漂移（在实现时按仓库现状决定；不引入跨包共享）。

## 5. 测试计划

测试命名遵循 Given-When-Then，追加到既有测试文件（不新建文件）。

| 文件 | 用例 |
|---|---|
| `packages/agent-core/test/plugin/manifest.test.ts` | 解析 `.kimi-plugin/plugin.json` 成功且 kind 为 `kimi-plugin-dir`；解析 `kimi.plugin.json` 成功且 kind 为 `kimi-plugin-root`；Mirri 清单存在时优先于 kimi 清单；无任何清单时错误文案列出全部 6 个文件名 |
| `packages/agent-core/test/plugin/archive.test.ts` | zip 仅含 `.kimi-plugin/plugin.json` 时能正确探测插件根目录 |
| `packages/agent-core/test/plugin/manager.test.ts` | 安装在 `.kimi-plugin/` 清单的插件成功（数据仿 superpowers） |
| `packages/agent-core-v2/test/app/plugin/manifest.test.ts` | 对应 v1 用例（kimi root/dir、优先、错误文案） |
| `packages/agent-core-v2/test/app/plugin/archive.test.ts` | 对应 zip 根目录探测用例 |

## 6. 验证

- 改必跑：`packages/agent-core` 与 `packages/agent-core-v2` 各自 `typecheck` + 受影响测试文件。
- 集成冒烟（可选，需网络）：`/plugins install https://github.com/obra/superpowers`，确认安装成功、技能清单出现、`getPluginInfo` 的 manifestPath 指向 `.kimi-plugin/plugin.json` 且 kind 为 `kimi-plugin-dir`。