# `mirri` 命令

`mirri` 是 Mirri Code 的引擎二进制。它在本地运行 Agent，并对外提供接口——桌面应用、内置 Web UI 和 ACP 客户端连接的都是同一个引擎。二进制**不提供交互式终端界面**，不带参数运行 `mirri` 只会打印帮助信息。要在浏览器里使用，用 [`mirri web`](#mirri-web)；要在脚本里跑单次提问，用 [`mirri print`](#mirri-print)。

```sh
mirri <command> [options]
```

::: tip 从 2.0 之前的版本升级过来？
早期版本提供终端界面。Rust 引擎替换了它，引擎不再提供交互式终端模式——`mirri web` 与桌面应用才是交互入口。运行 [`mirri migrate`](#mirri-migrate) 迁移已有会话。
:::

## 命令一览

| 命令 | 用途 |
| --- | --- |
| [`mirri web`](#mirri-web) | 启动本地服务、提供 Web UI 并在浏览器中打开 |
| [`mirri serve`](#mirri-serve) | 单独启动本地服务（API 优先，供前端与 IPC 客户端使用） |
| [`mirri print`](#mirri-print) | 非交互执行单次 prompt，把回复流式写到 stdout |
| [`mirri migrate`](#mirri-migrate) | 把旧 TypeScript 引擎的会话复制到 Rust 引擎的存储根目录 |
| [`mirri session-list`](#mirri-session-list) | 列出本机保存的会话 |
| [`mirri config-show`](#mirri-config-show) | 打印引擎实际解析出的配置 |
| [`mirri doctor`](#mirri-doctor) | 按引擎 schema 校验 `config.toml` |

全局选项：`-h` / `--help` 查看二进制或子命令的帮助，`-V` / `--version` 打印引擎版本。

## `mirri web`

启动本地服务，并在默认浏览器中打开 Web UI。想在终端里使用 Mirri Code 时，用这个命令。

```sh
mirri web                 # 启动服务并打开浏览器
mirri web --no-open       # 启动服务但不打开浏览器
mirri web --port 8080     # 换一个端口
```

服务在前台运行：命令持续挂在当前终端上，按 `Ctrl-C` 即停止。

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--host <host>` | `127.0.0.1` | 绑定地址，接受 IP 或主机名 |
| `--port <port>` | `58627` | 绑定端口。端口已被占用时命令以绑定错误退出，请显式换一个端口 |
| `--workspace <dir>` | 当前目录 | 要打开的工作区根目录。当前目录是文件系统根（打包应用可能继承 `/`）时回落到用户主目录 |
| `--web-dir <dir>` | 内嵌 UI | 指定一个已构建的 Web 前端目录，替代二进制内置的那份 |
| `--no-open` | 关闭 | 不启动浏览器 |
| `--log-level <level>` | `info` | 日志过滤级别，`RUST_LOG` 风格 |
| `--auth-token <token>` | `<mirri-home>/server.token` | HTTP API 的 Bearer 令牌，文件不存在时首次启动自动创建 |
| `--dangerous-bypass-auth` | 关闭 | 完全关闭 Bearer 令牌鉴权 |

Web UI 来自二进制内嵌的副本。未内嵌前端的构建会在可执行文件旁边探测已暂存的前端目录，而 `--web-dir` 会覆盖这两种来源。

## `mirri serve`

启动与 `mirri web` 相同的服务，但去掉 Web UI 的便利行为：不打开浏览器，且只有传入 `--web-dir` 时才提供 SPA。当前端由别的东西负责时用它——桌面应用、远程客户端，或你自己的工具。

```sh
mirri serve
mirri serve --ipc-path /tmp/mirri.sock --port 58627
```

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--host <host>` | `127.0.0.1` | 绑定地址 |
| `--port <port>` | `58627` | 绑定端口 |
| `--ipc-path <path>` | 关闭 | IPC 客户端使用的 Unix socket 路径，省略即关闭 IPC |
| `--workspace <dir>` | 最近打开的工作区 | 要服务的工作区根目录。不传该参数时，服务覆盖注册表中的所有工作区，并把最近打开的一个设为默认；注册表为空则回落到当前目录 |
| `--web-dir <dir>` | 关闭 | 随 API 一起提供已构建的 Web 前端 |
| `--log-level <level>` | `info` | 日志过滤级别，`RUST_LOG` 风格 |
| `--auth-token <token>` | `<mirri-home>/server.token` | HTTP API 的 Bearer 令牌 |
| `--dangerous-bypass-auth` | 关闭 | 完全关闭 Bearer 令牌鉴权 |

## 鉴权

两个服务命令默认都启用 Bearer 令牌鉴权。省略 `--auth-token` 时，令牌从 `<mirri-home>/server.token` 读取，该文件在首次启动时自动创建——桌面应用和 Web UI 用的是同一个文件。令牌就位后，除 `/api/v1/healthz` 与 `/api/v1/meta` 之外的所有路由都要求 `Authorization: Bearer <token>`。

`--dangerous-bypass-auth` 会关掉这项检查。除 `/api/v1/shutdown`（仍只允许回环对端）之外的所有路由都变成免鉴权。

::: danger
只在可信网络中使用 `--dangerous-bypass-auth`，或把它放在自行完成鉴权的反向代理之后。否则任何能访问该端口的人都对你的会话、文件和 Shell 拥有完全权限。
:::

## `mirri print`

发送单次 prompt，把回复流式写到 stdout 后退出。适用于脚本、CI 和快速的一次性提问。

```sh
mirri print "总结当前仓库的状态"
mirri print --model claude-sonnet-4-5 "解释最新的 diff"
mirri print --provider openai --model gpt-5 "列出 src/ 里的 TODO 注释"
```

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `<PROMPT>` | 必填 | 要发送的 prompt |
| `--model <model>` | 引擎默认模型 | 本次使用的模型 ID |
| `--provider <type>` | `anthropic` | 供应商类型：`anthropic` 或 `openai`。同时必须传 `--model` |

供应商凭证来自环境变量：`anthropic` 读取 `ANTHROPIC_API_KEY`（可选 `ANTHROPIC_BASE_URL`），`openai` 读取 `OPENAI_API_KEY`（可选 `OPENAI_BASE_URL`）。

Assistant 文本写到 stdout 并随生成逐步刷新；审批与提问提示写到 stderr。轮次失败时命令以 `1` 退出。会话保存在内存中，进程退出即丢弃，因此 `mirri print` 不会写入你的会话历史。

::: warning
`mirri print` 没有安装审批应答器。需要审批的工具调用会一直阻塞，直到引擎 300 秒的轮次超时。涉及写文件或执行 Shell 命令的任务，请改用 `mirri web`。
:::

## `mirri migrate`

把已退役的 TypeScript 引擎写下的会话复制到 Rust 引擎的存储根目录，使这些会话能正常恢复。迁移后的会话会被登记，引擎在下次服务启动后即可路由到它们。

```sh
mirri migrate                                  # 迁移全局主目录下的会话
mirri migrate --workspace ~/projects/app       # 同时迁移该工作区的会话
mirri migrate --dry-run                        # 只报告会复制什么
```

| 选项 | 说明 |
| --- | --- |
| `--workspace <dir>` | 同时迁移 `<dir>/.mirri/sessions/`。省略则只迁移 `<mirri-home>/sessions/` |
| `--dry-run` | 只报告将要迁移的内容，不做任何修改 |

迁移是复制而非移动——旧目录仍可被旧引擎读取——并会校验每一份副本。命令是幂等的：新根目录中已存在的会话会被跳过并报告。线协议版本按需升级，只在会话真正被恢复时进行。

## `mirri session-list`

列出本机保存的会话，最新的在前，同时读取 Rust 引擎的存储根目录和旧的 TypeScript 目录。

```sh
mirri session-list
mirri session-list --workspace ~/projects/app
```

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--workspace <dir>` | 当前目录 | 要查看的工作区目录 |

## `mirri config-show`

打印引擎实际解析出的配置，用于确认当前生效的内容。

```sh
mirri config-show
mirri config-show --workspace ~/projects/app
```

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--workspace <dir>` | 当前目录 | 要查看的工作区目录 |

不存在 `config.toml` 时，命令会明确提示，并说明使用的是内置默认值。

## `mirri doctor`

按引擎 schema 校验 `config.toml`，不启动任何东西，也不修改文件。

```sh
mirri doctor                             # 校验默认的 config.toml
mirri doctor --config ./candidate.toml   # 在替换正式配置前先校验候选文件
```

| 选项 | 说明 |
| --- | --- |
| `--config <path>` | 校验该文件，而不是默认的 `<mirri-home>/config.toml` |

默认文件不存在时报告为跳过，因为内置默认值仍然生效。通过 `--config` 显式指定的文件必须存在。所有检查项有效或被跳过时命令以 `0` 退出，文件无效或显式指定的文件缺失时以 `1` 退出。

## 相关可执行文件

`mirri` 不是项目提供的唯一可执行文件：

- **`mirri-acp`** —— IDE 作为子进程启动的 ACP 入口，见 [`mirri-acp`](./mirri-acp.md)。
- **桌面应用** —— 打包同一个引擎，额外提供窗口、原生菜单与应用集成。

## 下一步

- [配置文件](../configuration/config-files.md) —— 模型、权限模式等启动参数的持久化配置
- [数据路径](../configuration/data-locations.md) —— 会话、日志和配置在磁盘上的位置
- [Agent Skills](../customization/skills.md) —— 技能文件格式与发现目录
