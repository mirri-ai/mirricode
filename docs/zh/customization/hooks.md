# Hooks

Hooks（钩子）是一种自动触发机制：你预先告诉 Mirri Code CLI"每当发生 X，运行这个脚本"。脚本在你的本机执行，你可以在里面写任何逻辑。典型的使用场景：

- **安全拦截**：Agent 要执行 Shell 命令前，检查是否包含危险操作（如 `rm -rf`），包含则阻断执行
- **桌面通知**：后台任务完成时，弹出系统通知提醒你回来查看结果
- **自动检查**：每次用户提交消息时，自动在上下文里附加一些背景信息（如当前 Git 分支）

## Hooks 是怎么工作的

配置一条 hook 规则，需要指定三件事：**在什么事件上触发**、**匹配哪些目标**、**运行哪个脚本**。

触发时，CLI 会把事件的详细信息（触发原因、工具名称、命令内容等）打包成 JSON（一种结构化文本格式），通过**标准输入**（stdin，程序运行时用来接收外部数据的通道）传给你的脚本。脚本读取这些信息后，决定怎么响应。

脚本的响应结果由两样东西决定：

- **退出码**（exit code，程序结束时向操作系统报告的状态数字）：`0` 表示放行，`2` 表示阻断，其他数字默认放行
- **标准输出**（stdout，就是你用 `console.log` 或 `print` 打印出来的内容）：可以附带说明文字

即使脚本报错、超时，CLI 也**不会因此中断你的工作**——这种"出错就放行"的设计叫 fail-open（失败开放），避免 hook 异常变成绊脚石。

::: warning 注意
正因为 fail-open，Hooks 适合做提醒和轻量拦截，但**不应作为唯一的安全防线**。对真正高风险的操作，仍需依赖权限审批和人工确认。
:::

## 快速上手：一个最简单的 hook

下面这条 hook 会在每次后台任务完成时，在终端标题栏闪一下通知（macOS 需要安装 `terminal-notifier`）：

```toml
# 写在 ~/.mirri-code/config.toml 里
[[hooks]]
event = "Notification"           # 触发时机：后台任务状态变化时
matcher = "task\\.completed"     # 只关心"已完成"的通知
command = "terminal-notifier -title Mirri -message 'Task done'"
```

保存配置、重开会话，下次后台任务完成时就会弹出通知。

## 配置

所有 hook 规则写在 `~/.mirri-code/config.toml` 的 `[[hooks]]` 数组里，每一项是一条规则：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `event` | `string` | 是 | 触发事件名，必须是下文「事件一览」表中的某一项 |
| `matcher` | `string` | 否 | 用正则表达式（一种字符串匹配语法）过滤事件目标；不填则匹配全部 |
| `command` | `string` | 是 | 触发时要运行的 Shell 命令 |
| `timeout` | `integer` | 否 | 超时秒数，范围 1–600；默认 30 秒 |
| `failClosed` | `boolean` | 否 | 设为 `true` 时，hook 崩溃/超时/返回无效 JSON 会阻断操作而非放行；默认 `false`（fail-open）。仅对可阻断事件（`PreToolUse`、`Stop`、`UserPromptSubmit`）有效 |

`[[hooks]]` 只允许这五个字段，多写会导致配置文件加载失败。

**同一事件匹配多条规则时**，所有命中的 hook 并行运行；`command` 完全相同的多条规则只运行一次。

Hook 命令的工作目录是当前会话的项目目录。非 Windows 平台上，hook 进程放在独立进程组里，超时时先发信号让它有机会善后，之后才强制终止。

### 事件数据格式

每次触发时，CLI 都会把以下基础信息通过 stdin 传给脚本：

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project"
}
```

具体事件还会附带额外字段（如工具名称、命令内容、token 用量等）。所有字段名使用下划线命名（snake_case）。每个事件的完整 stdin payload 见下方[事件 payload 详情](#事件-payload-详情)。

## 返回值

脚本结束后，CLI 根据退出码判断 hook 的意图：

| 退出码 | 含义 | CLI 怎么处理 |
| --- | --- | --- |
| `0` | 正常结束，放行 | 继续执行；如果 stdout 是合法 JSON，会解析其中的结构化输出（见下文） |
| `2` | 主动阻断 | 停止当前操作；stderr 内容作为阻断原因 |
| 其他非零值 | 脚本出错 | 默认放行（fail-open） |
| 超时或崩溃 | 脚本异常 | 默认放行（fail-open） |

退出码 `2` 是最简单的阻断方式——只需通过 stderr 输出原因即可。但如果你需要更精细的控制（注入上下文、改写工具参数、同时返回阻断原因），可以通过 stdout 返回 JSON。

### Hook 输出能力

Hook 通过 stdout JSON 返回结构化结果。只有 **blocking hooks** 会消费输出；**观察型事件**（fire-and-forget）的输出被完全忽略。

`hookSpecificOutput` 里的字段不是通用的——每个字段只对特定事件有效，对应三种能力：

| 能力 | 字段 | 适用事件 |
| --- | --- | --- |
| 注入上下文 | `message` | `UserPromptSubmit` |
| 拦截操作 | `permissionDecision` + `permissionDecisionReason` | `UserPromptSubmit`、`PreToolUse`、`Stop` |
| 改写工具参数 | `updatedInput` | `RewriteToolInput` |

#### 1. 注入上下文

仅 `UserPromptSubmit` 支持此能力。

```json
{
  "hookSpecificOutput": {
    "message": "当前 Git 分支: feature/hooks，有 3 个未提交的改动"
  }
}
```

`message` 内容会被追加到用户的 prompt 中，LLM 会看到它。仅在 hook 放行（退出码 0 且未 deny）时生效。顶层的 `message` 字段与 `hookSpecificOutput.message` 等效，CLI 会做 fallback 合并。

#### 2. 拦截操作

`UserPromptSubmit`、`PreToolUse`、`Stop` 支持此能力。

```json
{
  "hookSpecificOutput": {
    "permissionDecision": "deny",
    "permissionDecisionReason": "请用 rg 代替 grep"
  }
}
```

`permissionDecision` 只认 `"deny"`，其他值无效。`permissionDecisionReason` 在 deny 时才被提取。各事件 deny 后的行为不同：

| 事件 | deny 后的行为 |
| --- | --- |
| `UserPromptSubmit` | 对话停止，`reason` 作为 assistant message 展示给用户 |
| `PreToolUse` | 工具执行被阻止，`reason` 展示给用户 |
| `Stop` | `reason` 注入为 user message，对话继续（每轮最多一次续写） |

::: tip 退出码 2 vs JSON deny
两种方式都能阻断，区别在于：退出码 2 只能传 stderr 文本作为原因；JSON deny 可以同时设置 `message`（注入上下文）和 `permissionDecisionReason`（阻断原因）。对于 `Stop` 事件，JSON 方式可以精确控制注入的续写文本。
:::

#### 3. 改写工具参数

仅 `RewriteToolInput` 支持此能力（需开启实验性标志 `hook-command-rewrite`）。v2 引擎不支持 `RewriteToolInput` 事件，参数改写由 `PreToolUse` 事件的 `updatedInput` 字段承担（同样需开启 `hook-command-rewrite`）。

```json
{
  "hookSpecificOutput": {
    "updatedInput": { "command": "rg pattern src/" }
  }
}
```

`updatedInput` 替换原始工具调用参数。必须是完整的参数对象，不是 patch。退出码 0 且无 `updatedInput` 则使用原始参数。详见[命令改写示例](#示例-命令改写)。

### 附录：完整字段参考

| 字段 | 类型 | 适用事件 | 说明 |
| --- | --- | --- | --- |
| `message` | `string?` | `UserPromptSubmit` | 注入到用户消息中的文本；顶层 `message` 与 `hookSpecificOutput.message` 等效 |
| `permissionDecision` | `"deny"?` | `UserPromptSubmit`、`PreToolUse`、`Stop` | 设为 `"deny"` 阻断当前操作；其他值无效 |
| `permissionDecisionReason` | `string?` | 同上 | 阻断原因；仅在 `permissionDecision` 为 `"deny"` 时提取 |
| `updatedInput` | `object?` | `RewriteToolInput` | 改写后的完整工具参数对象 |
| `additionalContext` | `string?` | — | 预留字段，已被 CLI 解析但**当前未接入对话注入**，返回不会报错但也无效果 |

::: info 哪些事件支持阻断？
只有 **blocking hooks**（`UserPromptSubmit`、`PreToolUse`、`Stop`、`RewriteToolInput`）的返回值会影响主流程。其中 `RewriteToolInput` 不能阻断，只能改写参数。其余事件属于**观察型事件**——触发后即发即忘，不管脚本返回什么，主流程都不会改变。
:::

## 事件一览

| 事件 | Matcher 匹配的是 | 会触发阻断？ | 说明 |
| --- | --- | --- | --- |
| `UserPromptSubmit` | 用户提交的文本内容 | ✓ | 用户发送消息时触发；返回文本会附加到上下文；若阻断，本轮不调用模型 |
| `PreToolUse` | 工具名 | ✓ | 工具调用前触发（权限检查前）；阻断后工具不会执行 |
| `Stop` | 空字符串 | ✓ | 模型准备结束本轮时触发；阻断后可追加一条消息让模型继续 |
| `PostToolUse` | 工具名 | — | 工具成功执行后触发（观察用） |
| `PostToolUseFailure` | 工具名 | — | 工具失败或被阻断后触发（观察用） |
| `PermissionRequest` | 工具名 | — | 即将等待用户审批前触发（观察用） |
| `PermissionResult` | 工具名 | — | 审批结束后触发（观察用） |
| `SessionStart` | `startup` 或 `resume` | — | 新会话启动或历史会话恢复后触发 |
| `SessionEnd` | `exit` | — | 会话关闭后触发 |
| `SubagentStart` | 子 Agent 名称 | — | 子 Agent 开始运行前触发 |
| `SubagentStop` | 子 Agent 名称 | — | 子 Agent 成功完成后触发（观察用） |
| `StopFailure` | 错误类型 | — | 本轮因错误失败后触发（观察用） |
| `Interrupt` | 空字符串 | — | 用户中断本轮时触发（例如按下 Esc）；超时或其他程序性中断不会触发。中断时 `Stop` 不会触发，由本事件替代。payload 含 `reason` 字段（观察用） |
| `PreCompact` | `manual` 或 `auto` | — | 上下文压缩开始前触发；返回值被完全忽略 |
| `PostCompact` | `manual` 或 `auto` | — | 上下文压缩完成后触发（观察用） |
| `Notification` | 通知类型（如 `task.completed`） | — | 后台任务状态变化时触发（观察用） |
| `RewriteToolInput` | 工具名 | — | 工具执行前触发（权限检查前）；可通过 JSON 响应中的 `updatedInput` 修改工具参数（需开启实验性标志 `hook-command-rewrite`）。仅 v1 引擎支持，v2 引擎不支持此事件 |
| `PreLlmRequest` | 空字符串 | — | LLM API 调用前触发，在消息和工具组装完成后（观察用） |
| `PostLlmRequest` | 空字符串 | — | LLM 响应返回后触发，在工具执行开始前（观察用） |

## 事件 payload 详情

基础字段（`hook_event_name`、`session_id`、`cwd`）包含在所有事件中。以下是每个事件通过 stdin 附带的额外字段。字段值仅为示例。

### `UserPromptSubmit`

```json
{
  "hook_event_name": "UserPromptSubmit",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "prompt": "帮我重构这个函数"
}
```

| 字段 | 说明 |
|------|------|
| `prompt` | 用户提交的文本内容 |

### `PreToolUse`

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git status" },
  "tool_call_id": "call_001"
}
```

| 字段 | 说明 |
|------|------|
| `tool_name` | 即将被调用的工具名称 |
| `tool_input` | 传给工具的参数 |
| `tool_call_id` | 本次工具调用的唯一 ID |

### `Stop`

```json
{
  "hook_event_name": "Stop",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "stop_hook_active": false,
  "last_assistant_message": "I've finished refactoring the auth module."
}
```

| 字段 | 说明 |
|------|------|
| `stop_hook_active` | 本轮中 Stop hook 的续写机制是否已被使用过（第二次及以后触发时为 `true`） |
| `last_assistant_message` | 模型最后一条回复的文本（截断为前 2000 字符）；让 hook 能根据模型刚才说了什么来决定是否阻断 |

### `PostToolUse`

```json
{
  "hook_event_name": "PostToolUse",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git status" },
  "tool_call_id": "call_001",
  "tool_output": "On branch main..."
}
```

| 字段 | 说明 |
|------|------|
| `tool_name` | 被调用的工具名称 |
| `tool_input` | 传给工具的参数 |
| `tool_call_id` | 本次工具调用的唯一 ID |
| `tool_output` | 工具输出文本（截断为前 2000 字符） |

### `PostToolUseFailure`

```json
{
  "hook_event_name": "PostToolUseFailure",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "git push" },
  "tool_call_id": "call_001",
  "error": { "message": "Permission denied" }
}
```

| 字段 | 说明 |
|------|------|
| `tool_name` | 被调用的工具名称 |
| `tool_input` | 传给工具的参数 |
| `tool_call_id` | 本次工具调用的唯一 ID |
| `error` | 描述工具失败原因的错误对象 |

### `PermissionRequest`

```json
{
  "hook_event_name": "PermissionRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "tool_input": { "command": "rm temp.txt" },
  "display": "rm temp.txt"
}
```

| 字段 | 说明 |
|------|------|
| `turn_id` | 本次工具调用所属的轮次编号 |
| `tool_call_id` | 本次工具调用的唯一 ID |
| `tool_name` | 请求权限的工具名称 |
| `action` | 请求的权限操作 |
| `tool_input` | 传给工具的参数 |
| `display` | 工具调用的可读摘要 |

### `PermissionResult`

审批通过时：

```json
{
  "hook_event_name": "PermissionResult",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "decision": "allow",
  "scope": "session"
}
```

审批过程中出错时，`decision` 为 `"error"`，用 `error` 字段替代 `scope`：

```json
{
  "hook_event_name": "PermissionResult",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "tool_call_id": "call_001",
  "tool_name": "Bash",
  "action": "allow",
  "decision": "error",
  "error": "Approval timed out"
}
```

| 字段 | 说明 |
|------|------|
| `turn_id` | 本次工具调用所属的轮次编号 |
| `tool_call_id` | 本次工具调用的唯一 ID |
| `tool_name` | 请求权限的工具名称 |
| `action` | 请求的权限操作 |
| `decision` | 审批结果：`allow`、`deny` 或 `error` |
| `scope` | 决策的作用范围（如 `session`、`project`）；`decision` 为 `error` 时省略 |
| `error` | 错误信息；仅在 `decision` 为 `error` 时存在 |

### `SessionStart`

```json
{
  "hook_event_name": "SessionStart",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "source": "startup"
}
```

| 字段 | 说明 |
|------|------|
| `source` | 会话启动方式：`startup`（新会话）或 `resume`（恢复历史会话） |

### `SessionEnd`

```json
{
  "hook_event_name": "SessionEnd",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "reason": "exit"
}
```

| 字段 | 说明 |
|------|------|
| `reason` | 会话结束原因；目前始终为 `exit` |

### `SubagentStart`

```json
{
  "hook_event_name": "SubagentStart",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "agent_name": "coder",
  "prompt": "Refactor the auth module",
  "model_alias": "sonnet"
}
```

| 字段 | 说明 |
|------|------|
| `agent_name` | 子 Agent 的 profile 名称 |
| `prompt` | 发送给子 Agent 的提示词（截断预览） |
| `model_alias` | 子 Agent 配置的模型别名 |

### `SubagentStop`

```json
{
  "hook_event_name": "SubagentStop",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "agent_name": "coder",
  "response": "Refactoring complete. Changed 3 files...",
  "duration_ms": 15200
}
```

| 字段 | 说明 |
|------|------|
| `agent_name` | 子 Agent 的 profile 名称 |
| `response` | 子 Agent 的最终响应（截断预览） |
| `duration_ms` | 子 Agent 执行耗时（毫秒） |

### `StopFailure`

```json
{
  "hook_event_name": "StopFailure",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "error_type": "APIError",
  "error_message": "Rate limit exceeded"
}
```

| 字段 | 说明 |
|------|------|
| `error_type` | 错误类型名称 |
| `error_message` | 错误信息 |

### `Interrupt`

```json
{
  "hook_event_name": "Interrupt",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": 5,
  "reason": "cancelled"
}
```

| 字段 | 说明 |
|------|------|
| `turn_id` | 被中断的轮次编号 |
| `reason` | 中断原因；目前始终为 `cancelled` |

### `PreCompact`

```json
{
  "hook_event_name": "PreCompact",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "trigger": "auto",
  "token_count": 45000,
  "context_window_size": 128000
}
```

| 字段 | 说明 |
|------|------|
| `trigger` | 触发压缩的方式：`manual`（手动）或 `auto`（自动） |
| `token_count` | 触发压缩时的 token 数量 |
| `context_window_size` | 模型的上下文窗口大小（token 数）；模型能力未知时省略 |

### `PostCompact`

```json
{
  "hook_event_name": "PostCompact",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "trigger": "auto",
  "estimated_token_count": 12000
}
```

| 字段 | 说明 |
|------|------|
| `trigger` | 触发压缩的方式：`manual`（手动）或 `auto`（自动） |
| `estimated_token_count` | 压缩完成后的估计 token 数量 |

### `Notification`

```json
{
  "hook_event_name": "Notification",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "sink": "context",
  "notification_type": "task.completed",
  "title": "Background task completed",
  "body": "Linting finished with 0 errors",
  "severity": "info",
  "source_kind": "task"
}
```

| 字段 | 说明 |
|------|------|
| `sink` | 通知的投递目标（如 `context`） |
| `notification_type` | 通知类型（如 `task.completed`） |
| `title` | 通知标题 |
| `body` | 通知正文 |
| `severity` | 通知严重级别（如 `info`、`warning`、`error`） |
| `source_kind` | 通知来源类型（如 `task`） |

### `RewriteToolInput`

完整 payload 和响应格式见[命令改写示例](#示例-命令改写)。

::: warning 注意
`RewriteToolInput` 仅 v1 引擎支持，v2 引擎不支持此事件。v2 引擎中参数改写由 `PreToolUse` 事件的 `updatedInput` 字段承担（见 [Hook 输出能力](#hook-输出能力)）。
:::

### `PreLlmRequest`

每个 loop step 触发一次，在消息和工具组装完成后、LLM API 调用前触发。适合观测实际调用的模型、请求内容随对话的变化趋势、动态工具加载是否按预期工作。

`messages` 中的媒体部分（图片、音频、视频）被替换为 `size_bytes` 标记，因为 base64 数据可能有数 MB。文本、思考块、工具调用和工具声明完整保留。

```json
{
  "hook_event_name": "PreLlmRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": "3",
  "step": 1,
  "model": "claude-sonnet-4-20250514",
  "message_count": 12,
  "tool_count": 8,
  "messages": [
    {
      "role": "system",
      "content": [
        { "type": "text", "text": "You are a helpful coding assistant..." }
      ],
      "tools": [
        {
          "name": "dynamic_tool",
          "description": "A tool loaded mid-conversation",
          "parameters": { "type": "object", "properties": { "x": { "type": "number" } } }
        }
      ]
    },
    {
      "role": "user",
      "content": [
        { "type": "text", "text": "看下这个截图" },
        { "type": "image_url", "size_bytes": 1843200 }
      ]
    },
    {
      "role": "assistant",
      "content": [
        { "type": "think", "think": "用户发了一张截图，我需要分析一下..." }
      ],
      "tool_calls": [
        { "id": "call_abc", "name": "Read", "arguments": "{\"path\":\"/src/main.ts\"}" }
      ]
    },
    {
      "role": "tool",
      "tool_call_id": "call_abc",
      "content": [
        { "type": "text", "text": "1\timport { createApp } from 'vue'\n2\t..." }
      ]
    }
  ]
}
```

| 字段 | 说明 |
|------|------|
| `turn_id` | 本次 LLM 调用所属的轮次编号 |
| `step` | 本轮中的步骤编号（从 1 开始） |
| `model` | 被调用的模型名称 |
| `message_count` | 请求中的消息数量 |
| `tool_count` | 请求中可用的工具数量 |
| `messages` | 序列化后的对话消息；媒体部分替换为 `size_bytes` |

### `PostLlmRequest`

每个 loop step 触发一次，在 LLM 响应返回、usage 记录后、工具执行开始前触发——早于 `step.end`。适合追踪 token 用量、估算成本、监控响应耗时，无需等待工具调用完成。

如果 LLM 调用失败（所有重试后），`PreLlmRequest` 会触发但 `PostLlmRequest` 不会——可以关联两者来检测失败。

```json
{
  "hook_event_name": "PostLlmRequest",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "turn_id": "3",
  "step": 1,
  "model": "claude-sonnet-4-20250514",
  "finish_reason": "tool_use",
  "usage": {
    "input_other": 344,
    "output": 567,
    "input_cache_read": 890,
    "input_cache_creation": 0
  },
  "duration_ms": 3420,
  "ttft_ms": 850,
  "tool_call_count": 2
}
```

| 字段 | 说明 |
|------|------|
| `turn_id` | 本次 LLM 调用所属的轮次编号 |
| `step` | 本轮中的步骤编号（从 1 开始） |
| `model` | 被调用的模型名称 |
| `finish_reason` | 响应结束原因：`end_turn`、`tool_use`、`max_tokens`、`filtered`、`paused` 或 `unknown` |
| `usage` | Token 用量明细（见下表） |
| `duration_ms` | LLM 调用端到端耗时（毫秒，含重试） |
| `ttft_ms` | 首 token 延迟（毫秒）；provider 不报告时省略 |
| `tool_call_count` | 响应中的工具调用数量 |

`usage` 对象字段：

| 字段 | 说明 |
|------|------|
| `input_other` | 非缓存读取、非缓存创建的输入 token |
| `output` | 模型生成的输出 token |
| `input_cache_read` | 从 provider prompt 缓存读取的输入 token |
| `input_cache_creation` | 写入 provider prompt 缓存的输入 token |

::: tip Token 追踪示例
```bash
#!/bin/bash
# ~/.mirri-code/hooks/token-tracker.sh
input=$(cat)
model=$(echo "$input" | jq -r '.model')
input_tokens=$(echo "$input" | jq -r '.usage.input_other + .usage.input_cache_read + .usage.input_cache_creation')
output_tokens=$(echo "$input" | jq -r '.usage.output')
echo "$(date -Iseconds),$model,$input_tokens,$output_tokens" >> ~/.mirri-code/token-usage.csv
```
:::

## 示例：阻断危险 Shell 命令

下面的 hook 在 Agent 调用 `Bash` 工具前检查命令内容，发现 `rm -rf` 就阻断：

```toml
[[hooks]]
event = "PreToolUse"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/block-dangerous-bash.mjs"
timeout = 5
```

```js
// block-dangerous-bash.mjs
// 从 stdin 读取 CLI 传来的事件数据
let input = '';
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  const payload = JSON.parse(input);         // 解析事件数据
  const command = payload.tool_input?.command ?? '';

  if (command.includes('rm -rf')) {
    // 通过 stderr 说明阻断原因，退出码 2 表示阻断
    console.error('检测到危险命令，已阻断');
    process.exit(2);
  }
  // 正常退出（退出码 0）表示放行
});
```

阻断后，Mirri Code CLI 会把阻断原因写回上下文，模型可以据此选择更安全的替代方案。

::: warning 注意
此示例仅演示阻断机制，不是生产级的安全解析器。真实场景更适合用白名单，或用专门的 Shell 解析器处理引号、变量展开和多段命令。
:::

## 示例：安全关键 hook 使用 failClosed

对于安全关键场景（如检查命令是否在白名单内），你可以用 `failClosed = true` 确保 hook 出错时阻断而非放行：

```toml
[[hooks]]
event = "PreToolUse"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/whitelist-check.mjs"
failClosed = true
timeout = 10
```

```js
// whitelist-check.mjs
// 检查命令是否在白名单内，不在则阻断
let input = '';
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  const payload = JSON.parse(input);
  const command = payload.tool_input?.command ?? '';

  // 白名单：只允许这些命令前缀
  const whitelist = ['git status', 'git diff', 'npm test', 'ls '];
  const allowed = whitelist.some(prefix => command.startsWith(prefix));

  if (!allowed) {
    console.error('命令不在白名单内: ' + command);
    process.exit(2);  // 退出码 2 = 阻断
  }
  // 退出码 0 = 放行
});
```

这样即使脚本崩溃、超时或输出无效 JSON，操作也会被阻断而非放行——宁可误杀，不可漏过。注意 `failClosed` 仅对可阻断事件（`PreToolUse`、`Stop`、`UserPromptSubmit`）有效。

## 示例：命令改写

`RewriteToolInput` 事件可以在工具执行前透明地改写工具参数。

::: warning 注意
本节示例基于 v1 引擎。v2 引擎不支持 `RewriteToolInput` 事件，请改用 `PreToolUse` 事件的 `updatedInput`（见 [Hook 输出能力](#hook-输出能力)）。
:::

首先，启用实验性功能：

```bash
# 通过环境变量（快速测试）
export MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE=true

# 或通过配置文件（持久化）
# [experimental]
# hook-command-rewrite = true
```

然后配置改写 hook。`matcher` 字段是正则表达式，匹配工具名称——可以指定 `Bash`，也可以用 `.*` 匹配所有工具：

```toml
[[hooks]]
event = "RewriteToolInput"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/rewriter.mjs"
timeout = 5
```

### 工作原理

`RewriteToolInput` hook 在工具调用生命周期中**准备阶段之后、权限检查之前**执行。具体流程：

1. Agent 决定调用工具（如 `Bash` 执行 `git status`）
2. 准备阶段 hook 运行（去重等）
3. **`RewriteToolInput` hook 触发**——你的脚本可以改写参数
4. 改写后的参数经过验证
5. 权限检查运行（包括 `PreToolUse` hook）
6. 工具使用最终参数执行

### 多个 hook

如果多个 `RewriteToolInput` hook 匹配同一个工具，**第一个**返回 `updatedInput` 的 hook 生效。如果 hook 没有返回 `updatedInput`，则被跳过。

### 编写自己的改写 hook

任何从 stdin 读取 JSON、向 stdout 写入 JSON 响应的脚本都可以作为改写 hook。

下面是一个生产级 Node.js 示例——改写命令并记录审计日志：

```js
#!/usr/bin/env node
// ~/.mirri-code/hooks/command-logger.mjs
//
// 改写工具参数并将每次改写记录到审计文件。
// 用法：在 config.toml 中配置为 RewriteToolInput hook。

import { appendFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';

const LOG_DIR = join(process.env.HOME ?? '/tmp', '.mirri-code', 'logs');
const LOG_FILE = join(LOG_DIR, 'rewrite-audit.log');

function log(message) {
  try {
    mkdirSync(LOG_DIR, { recursive: true });
    appendFileSync(LOG_FILE, `${new Date().toISOString()} ${message}\n`);
  } catch {
    // 日志失败不能影响 hook 执行
  }
}

let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  let payload;
  try {
    payload = JSON.parse(input);
  } catch (err) {
    log(`PARSE_ERROR: ${err instanceof Error ? err.message : err}`);
    process.exit(1);
  }

  const toolName = payload.tool_name;
  const toolInput = payload.tool_input;
  const command = toolInput?.command;

  // 只改写 Bash 工具调用
  if (toolName !== 'Bash' || typeof command !== 'string' || command.length === 0) {
    process.exit(0);
  }

  // 示例改写：加上自定义前缀
  const rewritten = `my-tool ${command}`;

  log(`REWRITE tool=${toolName} original=${JSON.stringify(command)} rewritten=${JSON.stringify(rewritten)}`);

  process.stdout.write(JSON.stringify({
    hookSpecificOutput: {
      permissionDecision: 'allow',
      updatedInput: { command: rewritten },
    },
  }));
});
```

配置：

```toml
[[hooks]]
event = "RewriteToolInput"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/command-logger.mjs"
timeout = 5
```

脚本通过 stdin 收到的输入：

```json
{
  "hook_event_name": "RewriteToolInput",
  "session_id": "session_abc",
  "cwd": "/path/to/project",
  "tool_name": "Bash",
  "tool_input": { "command": "ls -la" },
  "tool_call_id": "call_001"
}
```

脚本必须向 stdout 写入 JSON 响应：

```json
{
  "hookSpecificOutput": {
    "permissionDecision": "allow",
    "updatedInput": { "command": "my-tool ls -la" }
  }
}
```

**规则：**
- 退出码 `0` 且响应包含 `updatedInput` → 使用改写后的参数执行
- 退出码 `0` 但没有 `updatedInput` → 使用原始参数执行
- 退出码非零 → 使用原始参数执行（fail-open）
- 崩溃或超时 → 使用原始参数执行（fail-open）

::: tip
生产环境中务必先验证 `tool_name` 和 `tool_input` 再改写。不是所有工具都有 `command` 字段——只改写你理解的内容。
:::

::: warning 安全提示
允许 hook 改写工具参数会带来安全风险。恶意 hook 可能透明地重定向命令。仅在使用可信来源的 hook 时启用此功能。
:::

## 使用场景

- **命令包装**：在每个命令前加上自定义工具（如 `my-tool git status`）
- **节省 token**：集成 [rtk](https://github.com/rtk-ai/rtk) 压缩 CLI 输出
- **审计日志**：执行前记录每个命令
- **环境注入**：为特定命令添加环境变量或参数

## 下一步

- [配置文件](../configuration/config-files.md#hooks) — `[[hooks]]` 在 `config.toml` 中的完整字段声明
- [Agent 与子 Agent](./agents.md) — 利用 `SubagentStop` 事件在子 Agent 完成后触发通知
