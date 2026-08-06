# Agent 与子 Agent

Mirri Code CLI 中的每次会话都由一个**主 Agent** 驱动。主 Agent 理解用户意图、规划步骤、调用工具，并在需要时向外派发**子 Agent** 处理更聚焦的子任务——例如探索一个陌生代码库、并行审阅多处实现、或在不触碰主上下文的情况下规划一次大型重构。

子 Agent 接受主 Agent 给出的任务描述，在自己的独立上下文里工作，最后把结论返回。它不会与用户直接对话，中间的思考和工具调用记录也不会混入主 Agent 的历史。

## 内置子 Agent

Mirri Code CLI 内置三种子 Agent，开箱即用，分别面向不同任务形态：

- **`coder`**：默认子 Agent，通用软件工程助手，可以读写文件、执行命令、搜索代码并落地具体改动。
- **`explore`**：代码库探索专用，只做只读操作，不修改任何文件。适合在不改动文件的前提下快速搜索、阅读和总结仓库。
- **`plan`**：实现规划与架构设计专用，连 Shell 命令都不提供，专注于"想清楚怎么做"而不是"动手做"。

除必需的 `agent` profile 外，内置子 Agent 可通过 `config.toml` 中的 `disabled_agents` 或设置界面禁用。详见[启用和禁用 Agent](#启用和禁用-agent)。

`coder` 子 Agent 与主 Agent 共享大部分工具集：可以在后台执行 Shell 命令、维护待办列表、进入 Plan 模式、调用 Agent Skills，也可以在任务自然拆解时继续派发自己的嵌套子 Agent。如果它结束自己的轮次时仍有后台任务在运行，那么只有在这些后台任务全部落定后，这次运行才会回报完成——主 Agent 拿到结果时，背后的工作也已经真正完成。

## 调用方式

子 Agent 由主 Agent 自动调度——根据任务复杂度、上下文消耗和子任务的独立性，在适当时机派发，无需用户手动指定。

每次派发都会在终端以审批请求的形式呈现（除非命中 allow 规则或处于 YOLO 模式），方便你审视任务描述。你也可以在对话中直接指示主 Agent 使用特定子 Agent，例如"先用 explore 把相关文件梳理一遍再动手"。

子 Agent 支持在后台运行：完成后结果自动回到主 Agent，无需手动轮询。也可以唤回已有的子 Agent 实例继续推进同一任务。

## 上下文隔离与资源开销

每个子 Agent 拥有完全独立的上下文窗口，只能看到主 Agent 显式传入的任务描述，看不到主 Agent 的对话历史。子 Agent 自己的中间思考和工具调用记录不会回流，只有最终结果会出现在主 Agent 的上下文里。

这种隔离带来两个好处：

- **主 Agent 上下文保持精炼**，长会话中不会被大量探索性日志撑满。
- **多个子 Agent 可以并行运行**，互不干扰。

需要注意的是，每个子 Agent 都会独立消耗模型 token。简单任务没有必要派发子 Agent，主 Agent 直接处理更经济。

## 权限继承

子 Agent 的权限规则继承自主 Agent：主 Agent 通过 `/permission` 或在审批中接受的"始终允许"规则，会自动覆盖到它派发出的所有子 Agent，子 Agent 不需要重新审批同类工具调用。`Agent` 工具本身默认放行，因此主 Agent 可以在不打断用户的前提下完成多次委派。

如果需要某类工具在子 Agent 中始终不可用，应收紧主 Agent 的权限规则。

## 自定义 Agent Profile

你可以通过 Markdown 文件（YAML frontmatter + 正文）定义自定义 Agent profile。系统会自动发现并与内置 profile 合并。旧格式 `.yaml`/`.yml` 仍可读取以向后兼容，但新建 profile 一律保存为 `.md`。

### 文件位置

自定义 profile 按以下优先级顺序加载（同名 profile 以第一个为准）：

1. **项目级**：`<项目根目录>/.mirri-code/agents/*.md`
2. **用户级**：`$MIRRICODE_HOME/agents/*.md`（默认：`~/.mirri-code/agents/`）
3. **额外目录**：`config.toml` 中 `extra_agent_dirs` 指定的目录

同一目录下同名的 `.md` 和 `.yaml` 并存时，`.md` 优先。

与内置 profile 同名的自定义 profile 会**部分合并**到内置定义中——只有你声明的字段会覆盖内置值。这样你只需修改一个字段（如 `defaultModel`）而无需复制整个内置定义：

```markdown
---
# ~/.mirri-code/agents/coder.md
name: coder
defaultModel: gpt-4o
---
```

这里只覆盖了 `defaultModel`；`tools`、`whenToUse`、系统提示词等全部保留内置 coder 的定义。当新版本更新内置 agent 时，你的 override 会自动继承这些更新。

### Markdown 格式

每个文件定义一个 Agent profile——YAML frontmatter 放结构化元数据，Markdown 正文放系统提示词：

```markdown
---
name: reviewer
description: 代码审查专家
defaultModel: claude-sonnet
whenToUse: 用于代码审查任务
tools:
  - Read
  - Grep
  - Glob
---

你是代码审查专家。专注于安全、性能和风格一致性。
```

#### 字段说明

| 字段 | 位置 | 类型 | 说明 |
|------|------|------|------|
| `name` | frontmatter | string（必填） | Profile 名称，必须为 kebab-case（`[a-z0-9-]+`） |
| `description` | frontmatter | string（必填） | 在界面中显示的简短描述 |
| `whenToUse` | frontmatter | string | 指导主 Agent 何时选择此子 Agent |
| `defaultModel` | frontmatter | string | `config.toml` 中 `models` 定义的 model alias |
| `tools` | frontmatter | string[] | 此 Agent 可用的工具名称列表（省略 = 全部工具） |
| `subagents` | frontmatter | string[] | 此 profile 可派发的子 Agent 名称列表（省略 = 全部） |
| `capabilitiesRequired` | frontmatter | string[] | 必需能力（控制哪些工具可见） |
| *(正文)* | Markdown body | string | 系统提示词模板——`---` 闭合标签之后的 Markdown 内容 |

### 子 Agent 的模型选择

当主 Agent 通过 `Agent` 或 `AgentSwarm` 工具派发子 Agent 时，可以指定 `model` 参数覆盖默认模型。模型按以下优先级解析：

1. **显式指定** — LLM 在工具调用中传入的 `model` 参数
2. **Profile 默认** — Agent profile 中声明的 `defaultModel` 字段
3. **继承父级** — 主 Agent 当前使用的 model alias

工具描述中会列出可用的 agent 类型及其默认模型，让 LLM 知道何时不需要覆盖。此外，在 `config.toml` 中设置了 `description` 字段的模型会出现在"可用模型"列表中，为 LLM 提供每个模型的能力信息：

```toml
[models."sonnet"]
provider = "anthropic"
model = "claude-sonnet-4-20250514"
max_context_size = 200000
description = "balanced, strong at multi-file coding tasks"

[models."haiku"]
provider = "anthropic"
model = "claude-haiku-3-5-20241022"
max_context_size = 200000
description = "fast, best for simple lookups and quick tasks"
```

没有设置 `description` 的模型仍可用于 `defaultModel`，但不会作为 LLM 可选择的选项出现。这使模型列表保持简洁——只有你特意为 Agent 派发挑选的模型才会出现。

如果 LLM 传入了无效的 model alias，工具会返回错误并列出可选项，让它在下一轮中修正选择。

## 启用和禁用 Agent

内置子 Agent（必需的 `agent` profile 除外）和自定义 Agent 可以启用或禁用。

### 通过 config.toml

```toml
disabled_agents = ["explore", "plan"]
```

`agent` profile 是必需的，无法禁用 — `disabled_agents` 中对 `agent` 的条目会被忽略。

### 通过设置界面

Web 和 Desktop 设置页面列出所有 Agent profile，每个 profile 都有启用/禁用开关。必需 profile 的开关不可操作。

### 通过 TUI

使用 `/agents` 斜杠命令列出 profile 并切换其状态。

## 指令文件

全局 Mirri 专属指令可放在 `$MIRRICODE_HOME/AGENTS.md`（默认：`~/.mirri-code/AGENTS.md`）。当你用 `MIRRICODE_HOME` 移动数据根时，这份全局指令文件也会一起移动。跨工具通用指令仍可放在真实 OS home 下的 `~/.agents/AGENTS.md`，项目级指令仍放在项目目录中，例如 `.mirri-code/AGENTS.md` 或 `AGENTS.md`。

## 会话目录中的存储位置

子 Agent 的运行状态持久化到当前会话目录的 `agents/` 子目录下，每个子 Agent 实例对应一个独立目录，其中包含按时间顺序记录提示词、消息历史与最终状态的 `wire.jsonl` 文件。后台子 Agent 还会通过 `tasks/` 子目录暴露生命周期状态。

::: warning 注意
会话目录、wire 文件和任务记录都属于本地调试材料，可能包含用户 prompt、命令输出、仓库路径、工具返回内容或凭证痕迹。不要把这些文件直接提交到公开仓库、issue 或聊天记录里；如确需分享，请先脱敏。
:::

## 下一步

- [Hooks](./hooks.md) — 在子 Agent 完成等关键节点触发本地脚本通知或拦截
- [Agent Skills](./skills.md) — 给子 Agent 注入专业知识和工作流程
