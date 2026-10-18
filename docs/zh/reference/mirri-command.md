# mirri 命令

`mirri` 是 Mirri Code CLI 的主命令，用于在终端中启动一次交互式会话。不带任何参数运行时，它会在当前工作目录下开启一个新会话；配合不同的 flag，可以续上历史会话、跳过审批、从 Plan 模式开始，或者指定自定义的 Skills 目录。

```sh
mirri [options]
mirri <subcommand> [options]


## 主命令选项

所有 flag 都是可选的，直接运行 `mirri` 即可进入交互式会话：

| 选项 | 简写 | 说明 |
| --- | --- | --- |
| `--version` | `-V` | 打印版本号并退出 |
| `--help` | `-h` | 显示帮助信息并退出 |
| `--session [id]` | `-S` | 恢复一个会话。带 ID 时直接打开指定会话；不带 ID 时进入交互式选择器 |
| `--continue` | `-c` | 继续当前工作目录下最近一次的会话，无需手动指定 ID |
| `--model <model>` | `-m` | 为本次启动指定模型别名。省略时新会话使用配置文件中的 `default_model` |
| `--prompt <prompt>` | `-p` | 非交互执行单次 prompt，并把 Assistant 输出流式写到 stdout。该模式不会打开 TUI |
| `--output-format <format>` | | 设置非交互输出格式，支持 `text` 与 `stream-json`。仅可与 `--prompt` 一起使用，默认 `text` |
| `--yolo` | `-y` | 自动批准普通工具调用，跳过审批请求 |
| `--auto` | | 以 auto 权限模式启动；工具审批自动处理，Agent 不会向用户提问 |
| `--plan` | | 以 Plan 模式启动新会话，AI 会优先使用只读工具进行探索和规划 |
| `--skills-dir <dir>` | | 从指定目录加载 Skills，替换自动发现的用户和项目目录。可重复传入 |
| `--add-dir <dir>` | | 为本次会话添加额外的工作目录。相对路径按当前工作目录解析。可重复传入 |

`-r` / `--resume` 是 `--session` 的隐藏别名；`--yes` 和 `--auto-approve` 是 `--yolo` 的隐藏别名，在帮助信息中不显示。

::: warning 注意
`--yolo` 会跳过普通工具调用的人工确认，包括文件写入和 Shell 命令执行，请只在受信任的工作目录下使用。Plan 模式的退出审批不会被 `--yolo` 跳过；Plan 模式下的 `Bash` 按普通放行规则处理。
:::

### flag 冲突规则

以下组合会在启动时被拒绝：

- `--continue` 与 `--session` 互斥——两者都表示"恢复历史会话"
- `--yolo` 和 `--auto` 互斥——两种权限模式互斥
- `--prompt` 不能与 `--yolo`、`--auto` 或 `--plan` 同时使用——非交互模式固定使用 `auto` 权限
- `--output-format` 只能与 `--prompt` 一起使用

恢复会话时，可以通过 `--auto`、`--yolo` 或 `--plan` 覆盖原会话保存的权限或计划模式。例如，`mirri --continue --auto` 会恢复最近会话并切换到 auto 权限模式。

## 典型用法

直接运行开启新会话：

```sh
mirri


从上次中断的地方继续（自动找到当前目录最近的会话）：

```sh
mirri --continue


从历史会话列表中挑选，或直接指定已知 ID：

```sh
mirri --session
mirri --session 01HZ...XYZ


跳过审批确认，适合已知安全的批处理任务：

```sh
mirri --yolo


让 Agent 自行处理一切，不再向用户提问：

```sh
mirri --auto


先阅读代码、产出实现计划，而不是立刻动手修改文件：

```sh
mirri --plan


### 自定义 Skills 目录

有两种方式指定 Skills 目录，语义不同：

- **`--skills-dir <dir>`**（CLI flag）：**替换**自动发现的用户和项目目录，仅对本次启动生效。可重复传入以叠加多个目录：

  ```sh
  mirri --skills-dir /path/to/team-skills --skills-dir ./local-skills
  

- **`extra_skill_dirs`**（`config.toml`）：**叠加**到自动发现的目录之上，长期生效，适合配置团队共享 Skills。详见 [Agent Skills](../customization/skills.md)。

## 非交互执行

在脚本或 CI 中运行单次 prompt 时，使用 `-p`：

```sh
mirri -p "Summarize the current repository status"


输出采用 transcript 样式：thinking 内容和 Assistant 正文都以 `• ` 开头，换行后两个空格缩进。Assistant 正文输出到 stdout；thinking、工具进度和"恢复会话"提示输出到 stderr。`-p` 模式不会请求人工审批，普通工具调用按 `auto` 权限策略处理，静态 deny 规则仍然生效。

临时切换模型：

```sh
mirri -m mirri-code/kimi-for-coding -p "Explain the latest diff"


需要结构化读取输出时，使用 `stream-json` 格式——stdout 每行都是一个 JSON 对象：

```sh
mirri -p "List changed files" --output-format stream-json


`stream-json` 模式下，普通回复输出 Assistant 消息；模型调用工具时，先输出带 `tool_calls` 的 Assistant 消息，再输出对应的 Tool 消息，最后继续输出后续 Assistant 消息。thinking 内容不会写入 JSONL；工具进度和恢复会话提示仍写到 stderr。

## 子命令

`mirri` 提供以下子命令：`acp`（ACP IDE 模式）、`server`（已弃用，仅保留 `kill` 清理旧守护进程）、`web`（前台运行本地服务并打开 web UI）、`doctor`（校验配置文件）、`export`（导出会话）、`upgrade`（检查更新）、`provider`（管理供应商）。

### `mirri acp`

把 Mirri Code CLI 切换到 ACP（Agent Client Protocol）模式，在标准输入/输出上以 JSON-RPC 形式与 IDE 对话，让编辑器直接驱动 mirri 的会话和工具调用。通常不需要手动运行——IDE 会把它作为子进程入口启动。配置方式见[在 IDE 中使用](../guides/ides.md)，技术细节见 [mirri acp 参考](./mirri-acp.md)。

```sh
mirri acp


### `mirri server`（已弃用）

`mirri server` 已弃用，不再提供功能。任何调用（无论是否带旧的子命令或 flag）都会在 stderr 打印弃用提示并以退出码 `1` 结束。

唯一保留的子命令是 `mirri server kill`：用于清理 0.28.0 之前版本留下的后台守护进程（这些进程记录在旧版单实例锁文件中）。当前版本的服务都以前台方式运行（`Ctrl-C` 即停止），不会留下后台进程。

```sh
mirri server kill   # 清理 0.28.0 之前版本留下的后台服务
```

本地 REST + WebSocket 服务与 web UI 现在由 [`mirri web`](#mirri-web) 提供。

### `mirri web`

在浏览器中打开 Mirri 的图形会话界面，作为终端 TUI 的替代入口。

`mirri web` 在前台运行本地服务（v2 引擎 / kap-server）——命令保持挂在当前终端，按 `Ctrl-C` 即停止服务——服务健康后用默认浏览器打开 web UI。每次调用都会启动一个新的服务实例（默认端口被占用时绑定下一个空闲端口）；只有终端 TUI 的 `/web` 命令会复用已运行的服务。

设置 `MIRRICODE_RUST_ENGINE=1` 时，`mirri web` 改为启动 Rust 引擎的服务二进制，Web UI 的行为不变。

```sh
mirri web                 # 前台运行服务并打开浏览器
mirri web --no-open       # 不打开浏览器，服务保持挂在当前终端
```

| 选项 | 说明 |
| --- | --- |
| `--port <port>` | 绑定端口；默认 `58627` |
| `--host [host]` | 绑定地址。省略时仅绑定本机（loopback）；只传 `--host` 绑定所有网卡（token 会打印在启动输出中）；传 `--host <host>` 绑定指定地址 |
| `--allowed-host <host...>` | 额外允许通过 DNS 重绑定检查的 Host 头。可重复传或逗号分隔；前导点匹配域名后缀（如 `.example.com`） |
| `--insecure-no-tls` | 允许非 loopback 绑定在不经 TLS 反向代理的情况下运行（默认允许；仅对非 loopback 绑定相关） |
| `--allow-remote-shutdown` | 非 loopback 绑定时保留 `POST /api/v1/shutdown`（默认该路由禁用，返回 404） |
| `--allow-remote-terminals` | 非 loopback 绑定时保留 PTY 终端路由（默认禁用；远程 shell 风险高） |
| `--dangerous-bypass-auth` | 关闭所有 REST 与 WebSocket 路由的 bearer token 鉴权，使 web UI 无需 token 即可连接；仅用于可信网络或自有鉴权反向代理之后 |
| `--log-level <level>` | 服务日志级别；省略时不输出服务日志 |
| `--debug-endpoints` | 挂载 `/api/v1/debug/*` 调试路由（默认关闭） |
| `--no-open` | 不自动打开浏览器 |

::: danger 警告
`--dangerous-bypass-auth` 会彻底关闭鉴权。任何能访问该端口的人都能完全控制你的会话、文件系统和 shell。请仅在可信网络或自有鉴权反向代理之后使用。
:::

服务运行时，`GET /openapi.json` 返回 REST OpenAPI 文档，`GET /asyncapi.json` 返回本地 WebSocket 协议的 AsyncAPI 文档。服务默认开启 bearer token 鉴权；非 loopback 绑定时 token 会打印在启动输出中，可用 `mirri web rotate-token` 轮换。

#### `mirri web rotate-token`

生成新的持久化服务 token 并写入 `<MIRRICODE_HOME>/server.token`。旧 token 立即失效——正在运行的服务会在下次鉴权时读取新文件，轮换无需重启。

### `mirri doctor`

校验 `config.toml` 和 `tui.toml`，不会启动 TUI，也不会修改任一文件。默认检查 `MIRRICODE_HOME` 下的文件；未设置该环境变量时检查 `~/.mirri-code`。默认路径缺失时会显示为跳过，因为内置默认值仍可生效。

```sh
mirri doctor


| 命令 | 说明 |
| --- | --- |
| `mirri doctor` | 校验默认 `config.toml` 和 `tui.toml` |
| `mirri doctor config [path]` | 只校验 `config.toml`；传入 `path` 时使用该文件而不是默认文件 |
| `mirri doctor tui [path]` | 只校验 `tui.toml`；传入 `path` 时使用该文件而不是默认文件 |

显式传入路径时，文件必须存在。所有被检查的文件都有效或被跳过时，退出码为 `0`；任何指定文件缺失或配置无效时，退出码为 `1`。

```sh
# 检查默认配置文件
mirri doctor

# 只检查默认运行时配置
mirri doctor config

# 替换正式 TUI 配置前，先检查候选文件
mirri doctor tui ./tui.toml


### `mirri export`

把一个会话打包成 ZIP 文件，便于分享、归档或提交问题反馈。

```sh
mirri export [sessionId] [options]


| 参数 / 选项 | 简写 | 说明 |
| --- | --- | --- |
| `sessionId` | | 要导出的会话 ID。省略时自动选择当前工作目录下最近一次的会话，并要求确认 |
| `--output <path>` | `-o` | 输出 ZIP 文件路径。省略时写入当前目录下的默认文件名 |
| `--yes` | `-y` | 跳过默认会话的确认提示，直接导出 |
| `--no-include-global-log` | | 不打包全局诊断日志。默认包含 |

导出包含目标会话目录内的所有文件。全局诊断日志（`~/.mirri-code/logs/mirri-code.log`）默认包含，因为它可能含有其他会话或项目的事件；不想分享时加 `--no-include-global-log`。

```sh
# 导出当前工作目录最近一次会话，跳过确认
mirri export -y

# 导出指定会话到自定义路径
mirri export 01HZ...XYZ -o ./bug-report.zip

# 排除全局诊断日志
mirri export 01HZ...XYZ -o ./bug-report.zip --no-include-global-log


### `mirri upgrade`

立即检查最新版本并展示更新提示，选择操作后退出。也可以使用别名 `mirri update`。

```sh
mirri upgrade


对全局 npm、pnpm、yarn、bun 以及 macOS / Linux native 安装，`mirri upgrade` 会展示更新选项；选择 `Install update now` 后运行对应的前台安装命令。当前安装方式无法自动升级时（如 Windows native 安装），改为打印手动更新命令。

### `mirri vis`

在浏览器中启动会话可视化工具，直观查看一次会话的全过程。命令会启动一个指向本地会话的进程内服务器，打印访问地址并打开浏览器，持续运行直到你按下 `Ctrl-C`。

```sh
mirri vis [sessionId] [options]


| 参数 / 选项 | 说明 |
| --- | --- |
| `sessionId` | 直接打开指定会话的可视化页面。省略时打开列出所有会话的首页 |
| `--port <number>` | 绑定的端口。默认自动挑选一个空闲端口 |
| `--host <host>` | 绑定的主机。默认 `127.0.0.1` |
| `--no-open` | 不自动打开浏览器，仅打印访问地址 |

```sh
# 启动可视化工具并在浏览器中打开首页
mirri vis

# 直接打开指定会话
mirri vis 01HZ...XYZ

# 绑定固定主机和端口且不打开浏览器（例如在远程主机上）
mirri vis --host 0.0.0.0 --port 8123 --no-open


### `mirri provider`

在 shell 中管理供应商，相当于 TUI 中 `/provider` 的非交互版本。适合脚本化部署、CI 初始化，以及在新机器上一行完成配置。

```sh
mirri provider <action> [options]


包含五个动作：

#### `mirri provider add <url>`

从自定义 registry（`api.json`）批量导入所有供应商。命令会拉取 registry，为每个条目创建 `[providers.<id>]` 和 `[models.<alias>]`，并写入 `source` 元数据，使 TUI 下次启动时自动刷新同一 registry 地址下的供应商和模型。

| 参数 / 选项 | 说明 |
| --- | --- |
| `<url>` | Registry 地址 |
| `--api-key <key>` | 访问 registry 时携带的 Bearer token。未传时回退到环境变量 `MIRRICODE_REGISTRY_API_KEY`，必填 |

```sh
mirri provider add https://registry.example.com/v1/models/api.json --api-key YOUR_KEY

# 或通过环境变量（适合 CI / .envrc）
MIRRICODE_REGISTRY_API_KEY=YOUR_KEY mirri provider add https://registry.example.com/v1/models/api.json


如果某个 provider id 已存在，会先删除再重新写入。不会自动设置默认模型，后续可用 `-m` 或 TUI 内的 `/model` 选择。

#### `mirri provider remove <providerId>`

删除指定供应商及其所有模型 alias。如果被删除的供应商正好是 `default_model` 所属，则同时清空 `default_model`。

```sh
mirri provider remove kohub


#### `mirri provider list`

按行打印每个已配置的供应商，含类型、模型数量、来源。加 `--json` 可输出原始的 `providers` 和 `models` 表，便于程序化处理。

```sh
mirri provider list
mirri provider list --json | jq '.providers | keys'


#### `mirri provider catalog list [providerId]`

在不修改任何配置的情况下浏览公开的 [models.dev](https://models.dev/) 模型目录。不传参数时列出所有供应商及协议类型和模型数量；传 `providerId` 时列出该供应商下所有模型的上下文窗口和能力。

| 参数 / 选项 | 说明 |
| --- | --- |
| `[providerId]` | 可选，要查看的供应商 id |
| `--filter <substring>` | 按 id 或 name 大小写不敏感子串过滤 |
| `--url <url>` | 覆盖 catalog 地址，默认 `https://models.dev/api.json` |
| `--json` | 以 JSON 形式输出匹配片段 |

```sh
mirri provider catalog list
mirri provider catalog list --filter anthropic
mirri provider catalog list anthropic


#### `mirri provider catalog add <providerId>`

按 id 从 catalog 直接导入一个已知供应商，协议类型、base URL、模型信息均由 catalog 提供，只需提供 API key。

| 参数 / 选项 | 说明 |
| --- | --- |
| `<providerId>` | catalog 中的供应商 id，如 `anthropic`、`openai` |
| `--api-key <key>` | 供应商 API key。未传时回退到 `MIRRICODE_REGISTRY_API_KEY`，必填 |
| `--default-model <modelId>` | 可选，导入后把 `default_model` 设为 `<providerId>/<modelId>` |
| `--url <url>` | 覆盖 catalog 地址，默认 `https://models.dev/api.json` |

```sh
mirri provider catalog list anthropic          # 先看可选的模型
mirri provider catalog add anthropic --api-key sk-ant-... --default-model claude-opus-4-7


## 下一步

- [斜杠命令](./slash-commands.md) — 交互式 TUI 内的控制命令速查
- [配置文件](../configuration/config-files.md) — `default_model`、权限模式等启动参数的持久化配置
- [Agent Skills](../customization/skills.md) — `--skills-dir` 加载的 Skill 文件格式
