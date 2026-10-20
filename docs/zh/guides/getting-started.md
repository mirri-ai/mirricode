# 开始使用

## Mirri Code 是什么

Mirri Code 是一个运行在终端中的 AI Agent，帮助你完成软件开发任务和日常的终端操作——阅读和修改代码、执行 Shell 命令、搜索文件、抓取网页，并在执行过程中根据反馈自主规划和调整下一步行动。

它适用于以下场景：

- **编写和修改代码**：实现新功能、修复 bug、完成重构
- **理解项目**：探索陌生的代码库，解答架构和实现层面的问题
- **自动化任务**：批量处理文件、运行构建与测试、串联多个脚本

CLI 以 TypeScript 编写。官方安装脚本分发的是自包含的原生可执行文件，无需预装任何运行时；npm 渠道已停止更新（见下文迁移说明）。

## 安装

Mirri Code 以桌面应用为主推分发渠道，同时提供官方安装脚本安装 CLI。npm 渠道已停止更新，仅存量用户可用。

::: tip 安装之前
Mirri Code 为全交互式 TUI 应用，推荐在支持真彩色与连字的现代终端中运行以获得最佳体验，例如 [Kitty](https://sw.kovidgoyal.net/kitty/) 或 [Ghostty](https://ghostty.org/)。
:::

### 桌面应用（推荐）

从 [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) 页面下载对应平台的安装包（macOS `.dmg` 或 `.zip`、Windows `.exe`、Linux `.AppImage` / `.deb`）并安装，macOS 安装包已签名并公证。

### 脚本安装

在终端中安装 CLI，无需预装 Node.js：

- **macOS / Linux**：

```sh
curl -fsSL https://mirricode.com/install.sh | bash
```

- **Windows（PowerShell）**：

```powershell
irm https://mirricode.com/install.ps1 | iex
```

> Windows 用户首次启动前还需要安装 [Git for Windows](https://gitforwindows.org/)，Mirri Code 会使用其中的 Git Bash 作为 Shell 环境。如果 Git Bash 安装在非标准路径，请把 `MIRRICODE_SHELL_PATH` 设为 `bash.exe` 的绝对路径。

脚本会自动下载最新版本、校验 checksum，并把 `mirri` 可执行文件放到你的 `PATH` 中。

::: warning 注意
脚本按 `v` 前缀定位 GitHub Release。在首个 `vX.Y.Z` Release 发布之前，最新版本解析会 404——这是预期行为，请先使用桌面安装包；指定旧版本号（如 `1.10.0`）同样会 404，因为旧 Release 使用 scoped tag 且大概率没有 `manifest.json`。
:::

### npm 存量用户迁移

npm 包（`@mirri-ai/mirri-code`、`@mirri-ai/mirri-node`）在最后一次发布后已停止更新：

- **历史版本可以继续使用**，npm 上的安装与运行不受影响；但不会再发布新版本，`mirri upgrade` 也不会提示更新。
- **建议迁移到桌面应用**：从 [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) 下载桌面安装包，或改用[脚本安装](#脚本安装)安装 CLI。迁移后如需保留历史配置，数据默认存放在 `~/.mirri-code/`（见[数据存放在哪里](#数据存放在哪里)），可直接沿用。
- npm 卸载方式：`npm uninstall -g @mirri-ai/mirri-code`。

## 升级与卸载

安装完成后，验证可执行文件是否就绪：

```sh
mirri --version
```

**升级**：脚本安装的用户运行 `mirri upgrade`，CLI 会检查更新并给出提示。注意：在线版本信息已随 npm 停发而冻结，`mirri upgrade` 不会提示 GitHub Release 上的新版本——需要新版本时，重新运行安装脚本覆盖安装即可。桌面应用用户从 [GitHub Releases](https://github.com/mirri-ai/mirricode/releases/latest) 下载新版本安装包覆盖安装。

**卸载**：脚本安装的用户删除 `mirri` 可执行文件（默认在 `~/.mirri-code/bin/mirri`）即可；npm 存量用户执行：

```sh
npm uninstall -g @mirri-ai/mirri-code
```

## 第一次启动

进入项目目录后直接运行 `mirri` 启动交互界面：

```sh
cd your-project
mirri
```

只想执行一条指令而不进入交互界面时，使用 `-p`：

```sh
mirri -p "帮我看一下这个项目的目录结构"
```

继续上一次会话加 `-c`：

```sh
mirri -c
```

首次启动时需要配置模型供应商。在交互界面中输入 `/provider` 打开供应商管理器：

```
/provider
```

使用 `/provider` 添加供应商及其 API 密钥（例如 Mirri AI Open Platform），或添加自定义 registry，然后选择默认模型。再次运行 `/provider` 可查看、删除或重新配置供应商与凭证。

::: tip 使用其他 AI 供应商
如果你想接入 Anthropic、OpenAI、Google 等其他供应商，需要直接编辑 `~/.mirri-code/config.toml` 配置 API 密钥，详见[平台与模型](../configuration/providers.md)。配置项完整说明见[配置文件](../configuration/config-files.md)、[环境变量](../configuration/env-vars.md)和[配置覆盖](../configuration/overrides.md)。
:::

## 第一个对话

配置好模型后，用自然语言描述任务即可。先让它熟悉当前项目：

```
帮我看一下这个项目的目录结构，简单介绍一下每个目录是做什么的
```

Mirri Code 会自动调用文件读取、搜索等工具浏览相关内容后给出回答。只读操作默认自动执行无需确认；对于会修改文件或执行 Shell 命令的操作，默认会在执行前征求确认。

也可以直接描述更具体的任务：

```
在 src/utils 里新增一个函数，用来把任意字符串转成 kebab-case，并补一个单元测试
```

Mirri Code 会规划步骤、修改代码、运行测试，并在每一步告诉你它做了什么。

::: tip 不知道能做什么？输入 `/help`
随时在输入框输入 `/help`，可以打开内置的命令和快捷键面板，按 `↑`/`↓` 翻看，`Esc` 关闭。退出时输入 `/exit`，或按 `Ctrl-C` 两次，或在输入框为空时按 `Ctrl-D`。
:::

## 常用命令与快捷键速查

第一次使用时，记住下面这些就够了：

**会话相关命令**

| 命令 | 说明 |
| --- | --- |
| `/new` | 开启新会话，清空当前上下文 |
| `/sessions` | 浏览历史会话，选择恢复 |
| `/model` | 切换当前使用的模型 |
| `/compact` | 手动压缩上下文，释放 token |
| `/fork` | 派生当前会话，保留历史独立继续 |

**最常用快捷键**

| 快捷键 | 说明 |
| --- | --- |
| `Esc` | 中断流式输出 / 关闭弹窗 |
| `Ctrl-C` | 中断输出；空闲时连按两次退出 |
| `Shift-Tab` | 切换 Plan 模式 |
| `Ctrl-S` | 输出中途插入消息，无需等待结束 |
| `Ctrl-O` | 折叠 / 展开工具输出和压缩摘要 |

想看完整列表，输入 `/help` 或访问[斜杠命令参考](../reference/slash-commands.md)和[键盘快捷键](../reference/keyboard.md)。

## 数据存放在哪里

Mirri Code 的本地数据默认保存在 `~/.mirri-code/` 下，包含配置文件、会话记录、日志和更新缓存。如需迁移到别处，通过 `MIRRICODE_HOME` 环境变量指定新路径。完整说明见[数据路径](../configuration/data-locations.md)和[环境变量](../configuration/env-vars.md)。

## 下一步

- [交互与输入](./interaction.md) — 输入框操作、审批流程、Plan 模式和 YOLO 模式详解
- [会话与上下文](./sessions.md) — 恢复会话、上下文压缩、导出会话
- [常见使用案例](./use-cases.md) — 典型任务的 prompt 示例
