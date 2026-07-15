# Tool Integrations

当你连接一个 MCP 服务器（通过 [Model Context Protocol](./mcp.md) 暴露工具的程序）时，Agent 会将这些工具与 `Grep`、`Glob` 等内置工具一起展示。默认情况下，Agent 无法知道某个 MCP 工具与内置工具具有相同的用途——因此无法自动选择更优的那个。

`integrations.yaml` 解决了这个问题。你可以声明 MCP 服务器提供了什么**能力**（capability，例如代码搜索），以及它应该**优先于**哪些内置工具。声明完成后，Agent 会在 profile 激活时自动选择正确的工具，并在系统提示词中注入偏好提示，让模型知道应该优先使用哪个工具。

## 工作原理

每个工具可以声明一个或多个**能力标签**（capability tag）——类似 `code.explore` 这样的抽象标签，描述工具的功能。内置工具已经声明了自己的能力（例如 `Grep` 和 `Glob` 都声明了 `code.explore`）。当你在 `integrations.yaml` 中为 MCP 服务器添加相同的能力标签时，Agent 会将这些 MCP 工具视为内置工具的替代方案。

如果你同时设置了 `preferOver`，Agent 会调整优先级排序，使 MCP 工具排在偏好列表的前面。排序完全由显式的 `preferOver` 声明驱动——不存在隐式的 MCP 优先行为。

在 profile 激活时，Agent 会读取当前 profile 的工具列表，发现任何与 profile 所需能力匹配的额外工具，并将它们合并到活跃工具集中。如果 profile 没有声明 `capabilitiesRequired`，integration 元数据仍会记录在 registry 中，但不会自动注入工具。

Agent 还会生成一段简短的偏好提示并注入到系统提示词中：

```
Tool preference hints (derived from installed integrations):
- For `code.explore`, prefer `mcp__codebase-memory-mcp__search_graph` (falls back to: Grep, Glob).
```

这让模型知道应该优先使用哪个工具，无需用户手动调整提示词。

## 配置

### 文件位置

`integrations.yaml` 与 [`mcp.json`](./mcp.md#配置) 采用相同的两层模式：

- **用户级别**：`~/.mirri-code/integrations.yaml`（或 `$MIRRICODE_HOME/integrations.yaml`），所有项目共享
- **项目级别**：仓库根目录下的 `.mirri-code/integrations.yaml`，仅对当前项目生效

两个文件都是可选的。当两者同时存在时，相同 server name 的条目会合并——项目级别的条目会完全替换用户级别中同一 key 的条目。仅存在于某一范围的条目会被保留。

### Schema

根键为 `integrations`，是一个从 MCP server name 到 integration spec 的映射：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `capabilities` | `string[]` | 否 | 此服务器提供的能力标签。默认为 `[]`。 |
| `preferOver` | `string[]` | 否 | 此服务器应优先于的内置工具名。 |

两个数组中的字符串不能为空。

### 示例

一个代码智能 MCP 服务器，应在代码探索方面优先于 `Grep` 和 `Glob`：

```yaml
integrations:
  codebase-memory-mcp:
    capabilities:
      - code.explore
    preferOver:
      - Grep
      - Glob
```

仅声明能力、不指定优先于任何内置工具的服务器：

```yaml
integrations:
  brave-search:
    capabilities:
      - web.search
```

仅声明偏好（当能力已由内置工具声明时有用）：

```yaml
integrations:
  fast-search:
    preferOver:
      - Grep
```

## 可用能力

| 能力 | 说明 | 内置工具 |
|------|------|----------|
| `code.explore` | 搜索和探索代码（文件搜索、内容 grep、语义搜索） | `Grep`、`Glob` |
| `code.read` | 读取源码和文本文件（逐行、符号感知） | `Read` |
| `web.search` | 网页搜索 | `WebSearch` |
| `web.fetch` | 抓取并提取 URL 内容 | `FetchURL` |

随着工具生态系统的发展，会添加更多能力。你可以使用任意非空字符串作为能力标签——registry 会接受自定义值，但目前只有上表列出的能力有对应的内置工具。

## 子 Agent 集成

子 Agent（`explore`、`plan`）会自动接收其声明的 `capabilitiesRequired` 与 MCP 工具能力匹配的工具。`coder` 子 Agent 使用 `mcp__*` 直接访问所有 MCP 工具，因此不依赖能力匹配。

| 子 Agent | `capabilitiesRequired` |
|---------|----------------------|
| `explore` | `code.explore`、`code.read`、`web.search`、`web.fetch` |
| `plan` | `code.explore`、`code.read`、`web.search`、`web.fetch` |
| `coder` | （无 — 使用 `mcp__*` 获取所有 MCP 工具） |

这意味着，例如在 `integrations.yaml` 中声明了 `code.read` 能力的 MCP 服务器，会自动对 `explore` 和 `plan` 子 Agent 可用——无需手动编辑工具列表。

## 下一步

- [Model Context Protocol](./mcp.md) — MCP 服务器配置和连接方式
- [Agent 与子 Agent](./agents.md) — Agent profile、工具列表和 `capabilitiesRequired`
