# Mirri Desktop Lite — 现状盘点（Context）

- 快照日期：2026-10-27
- 用途：轻量壳设计的事实基础（代码路径、PoC 结论）。随现状演进更新。

## 1. 可复用资产清单

| 资产 | 位置（主仓） | 形态 |
|---|---|---|
| 桥契约（13 方法） | `apps/mirri-web/src/lib/desktopBridgeContract.ts` | 独立 DOM-free 模块 |
| 契约测试模板 | `apps/mirri-desktop/src/main/bridge-contract.test.ts` | 双向可赋值断言 |
| daemon 编排 | `apps/mirri-desktop/src/main/server/`（state/shell-env/daemon） | daemon.ts 含 electron import（`app.isPackaged`）与 server-binary 依赖 |
| open-in 注册表 + 检测 | `apps/mirri-desktop/src/main/open-in-apps.ts` | 单文件 |
| open-in launch 解析 | `apps/mirri-desktop/src/main/platform/open-in/`（contract/darwin/windows-linux） | 纯逻辑 |
| icns 图标提取 | `apps/mirri-desktop/src/main/app-icon.ts` | 纯 Node |
| 壳加载 URL 形态 | `${origin}/?mirri_desktop=1&platform=${platform}` | 字符串约定 |
| PoC 全量代码 | `../electrobun_poc/vue_bun_poc/`（仓库外） | 可运行参考实现 |
| 主壳 bundle id | `ai.mirri.desktop`（electron-builder.config.cjs:30） | 命名参照 |

## 2. PoC 已验证事实（2026-10-27）

- Electrobun 2.0.4：`defineRPC` 桥接 13 方法契约，结构校验 PASS
- preload 收脚本内容字符串（非路径），esbuild IIFE 打包注入成功
- drag region 零改动跟手（内置 `dragRegions.ts` 兼容层）
- 真实 web UI（Vue 3 + Vite，Chromium target 产物）在 WKWebView 零适配渲染正常
- daemon 复用 + 健康检查在 Bun 运行时通
- `openInApp finder` → `showItemInFolder`、`open -a "Visual Studio Code"` 实测打开成功

## 3. 已知坑（PoC 踩坑记录）

| 坑 | 处置 |
|---|---|
| preload 传路径静默失败 | 传 readFileSync 内容 |
| Vue runtime-only 构建不支持运行时模板 | 界面为 SFC，无此问题；仅 PoC 自身 |
| `open -a finder` 不是合法应用名 | finder → showItemInFolder 特判 |
| Hutch 期望 `icon.iconset` 在项目根 | 提供真实 iconset 否则告警（仍可构建） |
