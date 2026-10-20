# 配置文件

Mirri Code 把所有长期偏好写进 `~/.mirri-code/` 下的 TOML（一种结构清晰的纯文本配置格式）文件——比如使用哪个模型、填哪个 API 密钥、Agent 每轮最多跑几步。改一次，每次启动都生效。Agent 与运行时设置放在 `config.toml`，终端界面与客户端偏好（主题、编辑器、通知、自动更新）放在配套的 `tui.toml`。

默认位置：`~/.mirri-code/config.toml`，首次运行时自动创建。

## 配置文件位置

CLI 从 `~/.mirri-code/config.toml` 读取配置。如需把数据目录迁移到别处，可用 `MIRRICODE_HOME` 环境变量覆盖：

```sh
export MIRRICODE_HOME=/path/to/mirri-home
```

此时配置文件路径变为 `$MIRRICODE_HOME/config.toml`。无论目录在哪里，文件名固定是 `config.toml`。

::: tip
TOML 字段名一律用下划线（snake_case），如 `default_model`、`max_context_size`。字段名里若含 `.`，需用引号包住，例如 `[models."gpt-4.1"]`——否则 TOML 会把 `.` 解释为嵌套表分隔符。
:::

## 完整示例

以下示例覆盖最常用的配置项，可直接复制后按需修改：

```toml
default_model = "deepseek-flash"
default_permission_mode = "manual"
default_plan_mode = false
merge_all_available_skills = true
telemetry = true

[providers.deepseek]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = ""

[models."deepseek-flash"]
provider = "deepseek"
model = "deepseek-flash"
max_context_size = 131072

[thinking]
enabled = true
effort = "high"
keep = "all"

[loop_control]
max_retries_per_step = 10
reserved_context_size = 50000
# 单轮 stall watchdog 窗口（毫秒）。默认 600000（600s）；
# 0 = 禁用 watchdog。示例：长 thinking 无流式输出的模型调大到 900s
# turnStallTimeoutMs = 900000

[background]
# 这些字段也可以写在 [task] 下，后者按字段覆盖 background
max_running_tasks = 4
keep_alive_on_exit = false

[[permission.rules]]
decision = "allow"
pattern = "Read"

[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"

[[hooks]]
event = "PreToolUse"
matcher = "Bash"
command = "node ~/.mirri-code/hooks/check-bash.mjs"
timeout = 5
```

## 顶层字段

配置文件里的字段分两类：**顶层标量**直接控制默认行为，**嵌套表**（`providers`、`models`、`thinking` 等）各有独立结构，在下文各节单独说明。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `default_model` | `string` | — | 默认模型别名，必须在 `models` 中定义 |
| `default_provider` | `string` | — | `providers` 表中的默认供应商 key |
| `default_permission_mode` | `string` | `manual` | 新会话的默认权限模式，可选 `manual`（逐次询问）、`auto`（自动批准读操作）、`yolo`（全部自动批准） |
| `default_plan_mode` | `boolean` | `false` | 新会话是否默认以 Plan 模式（先出计划再执行）启动 |
| `merge_all_available_skills` | `boolean` | `true` | 是否合并所有目录中的 Agent Skills |
| `extra_skill_dirs` | `array<string>` | — | 额外 Skill 搜索目录，叠加到默认目录之上 |
| `extra_agent_dirs` | `array<string>` | — | 额外 Agent profile 搜索目录，用于自定义 Agent 发现 |
| `disabled_agents` | `array<string>` | — | 已弃用 — 不再控制 Agent 启停；仅为兼容旧配置文件而保留解析（非空值会在启动时记录一条弃用警告）。禁用 Agent 请使用 Agent profile 文件中的 `enabled: false` → [启用和禁用 Agent](../customization/agents.md#启用和禁用-agent) |
| `telemetry` | `boolean` | `true` | 是否启用匿名遥测；显式设为 `false` 时关闭 |
| `providers` | `table` | `{}` | API 供应商表 → [`providers`](#providers) |
| `models` | `table` | — | 模型别名表 → [`models`](#models) |
| `thinking` | `table` | — | Thinking 模式默认参数 → [`thinking`](#thinking) |
| `loop_control` | `table` | — | Agent 循环控制参数 → [`loop_control`](#loop_control) |
| `background` | `table` | — | 后台任务运行参数 → [`background`](#background) |
| `task` | `table` | — | 后台任务生命周期参数（当前写法）→ [`task`](#task) |
| `services` | `table` | — | 内置外部服务配置 → [`services`](#services) |
| `permission` | `table` | — | 初始权限规则 → [`permission`](#permission) |
| `hooks` | `array<table>` | — | 生命周期 hook，详见 [Hooks](../customization/hooks.md) |

以下各节对 `providers`、`models`、`thinking`、`loop_control`、`background`、`task`、`subagent`、`experimental`、`services`、`permission` 等嵌套表逐一展开。

## `providers`

`providers` 表的每一项定义一个 API 供应商，以唯一名称为 key。引擎构建连接时，API 密钥按以下顺序解析：

1. 供应商配置项里的 `api_key` 字段（非空值立即生效）；
2. 否则回落到 `[providers.<name>.env]` 子表里该类型惯用的键名——`type = "anthropic"` 读 `ANTHROPIC_API_KEY`，`openai` / `openai_responses` 读 `OPENAI_API_KEY`，`google` / `google-genai` 读 `GOOGLE_API_KEY`，`kimi` 读 `MIRRI_API_KEY`；
3. 否则再从 shell 环境读取同名的惯用变量；
4. 都没有时，启动报错，错误信息会写明期望的变量名。部分类型（如 `vertexai`）没有惯用变量，解析链会提前结束。

`base_url` 遵循同样的链路：先读配置字段，再读 `env` 子表里同名的惯用键，再读同名的 shell 环境变量（`ANTHROPIC_BASE_URL`、`OPENAI_BASE_URL`、`GOOGLE_GEMINI_BASE_URL`、`MIRRI_BASE_URL`），最后用类型内置默认端点。

完整的优先级规则详见[配置覆盖](./overrides.md#供应商凭证)。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `type` | `string` | 建议填写 | 供应商类型：`kimi`、`anthropic`、`openai`、`openai_responses`、`google-genai`。省略时默认为 `openai`。`vertexai` 仅 TypeScript 引擎支持——rust 引擎在建立连接时会拒绝它 |
| `api_key` | `string` | 否 | API 密钥，明文写在配置文件里 |
| `base_url` | `string` | 否 | API 基础 URL |
| `oauth` | `table` | 否 | OAuth 凭据引用（`storage`、`key` 两个字段），通常由 CLI 管理，无需手写 |
| `env` | `table<string, string>` | 否 | 传给供应商适配器的额外键值对；值支持 `${VAR}` 展开。供应商惯用的键名（如 `MIRRI_API_KEY`）同时作为 `api_key` / `base_url` 的凭证回退来源——见下文 |
| `custom_headers` | `table<string, string>` | 否 | 每次请求附加的自定义 HTTP 头 |
| `removed_model_ids` | `array<string>` | 否 | 上游模型 id 的黑名单；模型目录刷新不会重新加回在此列出的模型 |
| `media_file_refs` | `boolean` | 否 | 仅 `anthropic` 供应商。允许通过供应商的 Files API 上传图片并以 file id 引用，替代 base64 内联；默认为 `true` |

**`env` 子表**：可以把供应商惯用的键名（如 `MIRRI_API_KEY`）写在 `[providers.<name>.env]` 里，作为 `api_key` / `base_url` 解析链的第二步。子表本身只从配置文件读取，不会修改 shell 环境：

```toml
[providers.kimi.env]
MIRRI_API_KEY = "sk-xxx"
MIRRI_BASE_URL = "https://api.moonshot.ai/v1"
```

优先级：`api_key` 字段 > `env` 子表键 > 该供应商类型惯用的 shell 环境变量 > 全部缺失时启动报错。`base_url` 的解析链相同，最后一步是内置默认 URL 而非报错。

## `models`

`models` 表的每一项定义一个模型别名（即 `default_model` 或 `-m` 参数里使用的名称），以唯一名称为 key。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `provider` | `string` | 是 | 使用的供应商名称，必须在 `providers` 中定义 |
| `model` | `string` | 是 | 调用 API 时实际传给服务端的模型 ID |
| `max_context_size` | `integer` | 是 | 最大上下文长度（token 数），必须 ≥ 1 |
| `max_output_size` | `integer` | 否 | 单次请求的输出 token 上限（对应 `max_tokens`）。目前仅 `anthropic` 供应商读取。为 Claude 模型设置后，这个显式值会覆盖内置的服务端最大值 |
| `capabilities` | `array<string>` | 否 | 显式追加的能力标签：`thinking`、`always_thinking`、`image_in`、`video_in`、`audio_in`、`tool_use`。与供应商自动识别的能力取并集，只能追加不能移除 |
| `support_efforts` | `array<string>` | 否 | 模型支持的 Thinking 档位（如 `["low", "high", "max"]`） |
| `default_effort` | `string` | 否 | 模型的默认 Thinking 档位 |
| `display_name` | `string` | 否 | UI 中显示的名称，未设时回退到 `model` |
| `description` | `string` | 否 | 暴露给 LLM 的简短能力描述，出现在 `Agent`/`AgentSwarm` 工具描述中。设置后，该模型会出现在"可用模型"列表中，让 LLM 在派发子代理时能做出明智的模型选择。未设置 `description` 的模型仍可用于 `default_model`，但不会作为 LLM 可选择的选项 |
| `reasoning_key` | `string` | 否 | 仅 `openai` 供应商。当网关用非标准字段名返回推理内容时才需要设置；默认自动识别 `reasoning_content` / `reasoning_details` / `reasoning` |
| `adaptive_thinking` | `boolean` | 否 | 仅 `anthropic` 供应商。强制开启或关闭 adaptive thinking，覆盖按模型名推断的逻辑。省略时自动推断（Claude ≥ 4.6 使用 adaptive） |
| `protocol` | `string` | 否 | 强制指定传输协议（`anthropic`、`openai`、`openai_responses`、`google-genai`），覆盖供应商类型的推断结果 |
| `beta_api` | `boolean` | 否 | 请求是否走供应商的 beta API 端点 |

别名中含 `.` 时需要加引号：

```toml
[models."gpt-4.1"]
provider = "openai"
model = "gpt-4.1"
max_context_size = 1047576
```

### Catalog 刷新行为

provider-model 刷新（启动时、每 6 小时、或手动"刷新全部"）只**追加**在 catalog 中发现的新模型。已有模型 alias 绝不会被修改或删除——配置归你所有。你删除的模型会记录在对应 provider 的 denylist（`removed_model_ids`）中，刷新时不会把它们悄悄加回来。

无需修改配置文件也可以临时切换模型——通过 `MIRRICODE_MODEL_*` 环境变量在内存里合成一个临时供应商，详见[用环境变量定义模型](./env-vars.md#用环境变量定义模型-mirricode-model)。

## `thinking`

`thinking` 设置 Thinking 模式的全局默认行为。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `boolean` | `true` | 新会话是否默认开启 Thinking，设为 `false` 可强制关闭 |
| `effort` | `string` | — | Thinking 强度（例如 `low`、`medium`、`high`、`xhigh`、`max`），实际可用等级取决于模型声明的 `support_efforts`，未识别的值会被供应商忽略 |
| `keep` | `string` | `"all"` | 保留思考透传。在 `kimi` 上以 `thinking.keep` 发送；在 `anthropic`（Claude 以及 Mirri 的 Anthropic 兼容模式）上以 `context_management` 的 `clear_thinking_20251015` 编辑发送（开启 keep 会让 Anthropic 请求走 beta Messages API；关值可禁用 keep 并回到标准端点）。`"all"` 会保留历史轮次的思考内容（`reasoning_content` / Anthropic thinking blocks）；传入关值（`false`/`0`/`no`/`off`/`none`/`null`）可禁用。可被 `MIRRICODE_MODEL_THINKING_KEEP` 覆盖；仅在 Thinking 开启时注入 |

### 已废弃字段

| 字段 | 废弃版本 | 描述 |
| --- | --- | --- |
| `default_thinking` | 0.21.0 | 顶层布尔值，由 `[thinking] enabled` 取代。将 `default_thinking = true` 迁移为 `enabled = true`，`default_thinking = false` 迁移为 `enabled = false`。 |
| `thinking.mode` | 0.21.0 | 可选值 `auto` / `on` / `off`，由 `[thinking] enabled` 取代。`mode = "off"` 改为 `enabled = false`；`mode = "on"` 和 `mode = "auto"` 等价于 `enabled = true`（默认值），可删除该行。 |

## `loop_control`

`loop_control` 控制 Agent 执行循环的步数上限、单步重试次数，以及触发上下文自动压缩的阈值。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `max_steps_per_turn` | `integer` | — | 单轮最大步数；不设或设为 `0` 则无上限 |
| `max_retries_per_step` | `integer` | `10` | 单步失败后的最大重试次数 |
| `reserved_context_size` | `integer` | — | 预留给模型输出的 token 数；上下文窗口剩余量低于此值时触发自动压缩 |
| `compaction_trigger_ratio` | `number` | — | 上下文占用比例达到该值时触发自动压缩 |
| `max_overflow_compaction_attempts` | `integer` | — | 上下文溢出后最多重试压缩几次才把错误抛出；`0` 表示首次溢出即报错 |
| `max_history_messages` | `integer` | `10000` | 规范历史（canonical history）中的消息数上限；`0` 表示完全拒绝后续追加 |
| `max_deferred_messages` | `integer` | `1000` | 工具调用进行中允许缓冲的消息数上限；`0` 表示完全拒绝后续缓冲 |
| `retention_tail_ratio` / `retention_tail_tokens` | `number` / `integer` | — | 压缩时的保留尾部预算，支持按上下文窗口比例或绝对 token 数两种写法。二者互斥——同时设置会导致配置加载失败；ratio 必须满足 `0 < ratio < 1` |
| `turnStallTimeoutMs` | `integer` | 引擎默认 600000（600s） | 单轮 stall watchdog 窗口（毫秒）——一轮超过该时长没有任何 live 事件（LLM 增量、工具事件、重试等）即被引擎协作式取消。不设则用引擎默认 600s；设为 `0` 彻底禁用 watchdog。该值应**大于** provider 的 HTTP read timeout（240s）：静默挂死的模型流会先触发 provider 侧可重试的读超时错误，watchdog 退居兜底；若显式设置且小于 240000，启动时会打一条告警日志（不拒绝启动）。只在引擎构造时读取一次，改配置后需重启生效 |

## `background`

`background` 控制后台任务（通过 `Bash` 工具或 `Agent` 工具的 `run_in_background=true` 参数启动）的并发数。`background` 是 legacy 写法——当前配置应直接使用 [`task`](#task)，后者按字段覆盖它。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `max_running_tasks` | `integer` | — | 同时运行的最大后台任务数 |
| `keep_alive_on_exit` | `boolean` | `false` | 会话关闭时是否保留仍在运行的后台任务。默认情况下，Mirri Code 会在进程退出前请求停止所有后台任务；只有希望任务在会话结束后继续运行时才设为 `true`。在 print 模式（`mirri -p`）下，本字段仅作为 `print_background_mode` 未设置时的兼容回退：`true` 等价于 `print_background_mode = "drain"` |
| `bash_task_timeout_s` | `integer` | `600` | 后台 `Bash` 任务在调用未传 `timeout` 时的默认超时（秒）；前台命令超时转后台后也按此值重新计时。`0` 表示无超时——任务一直运行到自行结束或被模型手动停止。显式传入的 `timeout` 不受影响。在 print 模式（`mirri -p`）下未显式设置时默认为 `0` |
| `print_background_mode` | `"exit" \| "drain" \| "steer"` | `"steer"` | 仅 print 模式（`mirri -p`）生效，决定主 agent 的 turn 结束后如何处理未返回的后台任务：`"exit"` 立即退出；`"drain"` 退出前等待所有后台任务进入终态（结果不回馈给主 agent）；`"steer"` 不退出，让后台任务完成时像后台子代理一样以合成 user 消息 steer 主 agent 进入新 turn，直到某 turn 结束时无未决后台任务或触及上限。设置后优先级高于 `keep_alive_on_exit` 的 print 回退 |
| `print_wait_ceiling_s` | `integer` | `315360000` | print 模式（`mirri -p`）下，`print_background_mode` 为 `"drain"` 或 `"steer"` 时，等待/steer 循环的墙钟上限（秒；默认 10 年，近似不设限）。在非 print 模式或 `"exit"` 时无效 |
| `print_max_turns` | `integer` | `100000` | print 模式（`mirri -p`）且 `print_background_mode = "steer"` 时，允许由后台任务完成触发的新 turn 的最大数量，防止 steer 循环失控（默认值近似不设限） |

`keep_alive_on_exit` 可被环境变量 `MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT` 覆盖，优先级高于配置文件。

在 print 模式（`mirri -p "<prompt>"`）下，只要还有未决的后台任务，Mirri Code 在主 agent 的 turn 结束后不会退出：每个任务完成都会以合成 user 消息回馈给主 agent，steer 出新的 turn（默认 `print_background_mode = "steer"`），直到某 turn 结束时没有任何未决任务才退出。该循环受 `print_wait_ceiling_s` 与 `print_max_turns` 约束，默认值都近似不设限。print 模式下后台工作也不会被墙钟超时杀掉：后台 `Bash` 任务默认无超时（`bash_task_timeout_s = 0`），子代理默认无超时（`[subagent] timeout_ms = 0`），只有模型自己能停止任务。将 `print_background_mode` 设为 `"drain"` 可等待任务结束但不回馈结果，设为 `"exit"` 则在主 agent 结束后立即退出。

## `task`

`task` 是后台任务生命周期设置的当前归属节。它取代 legacy 的 `background`：两者同时存在时，`task` 按字段覆盖 `background`——`task` 未设置的字段回落到 `background` 的值。新配置应直接写 `task`。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `max_running_tasks` | `integer` | `0`（不限） | 同时运行的最大后台任务数；低于 `1` 的值会在加载时被拒绝 |
| `keep_alive_on_exit` | `boolean` | `false` | 会话关闭后仍在运行的后台任务是否继续存活 |
| `bash_task_timeout_s` | `integer` | `600` | 后台 `Bash` 任务的默认超时（秒）；`0` 表示无超时 |
| `bash_auto_background_on_timeout` | `boolean` | — | 前台 `Bash` 命令超时时转为后台继续运行，而不是被杀掉 |
| `kill_grace_period_ms` | `integer` | `5000` | 升级停止操作将任务置为 `stopped` 前的宽限窗口（毫秒） |
| `print_background_mode` | `"exit" \| "drain" \| "steer"` | `"steer"` | print 模式（`mirri -p`）的后台任务策略；与 `background` 同名字段语义一致 |
| `print_wait_ceiling_s` | `integer` | `315360000` | drain/steer 等待循环的墙钟上限（秒） |
| `print_max_turns` | `integer` | `100000` | steer 模式下由后台任务完成触发的新 turn 最大数量 |
| `heartbeat_interval_ms` | `integer` | `300000` | 运行中的任务多久（毫秒）没有活动会被标记为 stale；`0` 表示关闭假死检测 |

## `subagent`

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `timeout_ms` | `integer` | `7200000`（2 小时） | 单个子代理（`Agent` / `AgentSwarm`）允许运行的最长时间（毫秒）。超时后子代理以 `timed_out` 收尾。`0` 表示无超时——子代理一直运行到自行结束或被模型手动停止。该值是后台任务管理器对每个子代理任务的 per-task timeout，因此对前台与后台子代理同时生效。在 print 模式（`mirri -p`）下未显式设置时默认为 `0`。注意：超过 `2147483647`（约 24.8 天）的值会被运行时钳到约 24.8 天 |
| `max_nesting_depth` | `integer` | `5` | 子 Agent 嵌套深度上限（主 Agent 是深度 1）。`0` 表示无上限 |
| `max_swarm_items` | `integer` | `3` | 单次 `AgentSwarm` 最多启动的成员数。`0` 表示无上限。可被环境变量 `MIRRI_AGENT_SWARM_MAX_ITEMS` 覆盖（优先级：环境变量 > 配置文件 > 默认值） |

`timeout_ms` 可被环境变量 `MIRRICODE_SUBAGENT_TIMEOUT_MS` 覆盖，优先级高于配置文件。

`max_nesting_depth` 与 `max_swarm_items` 都在派发门（spawn gate）强制执行——超过上限的派发会被拒绝，拒绝消息会写明具体上限。

## `experimental`

`experimental` 存放实验功能 flag 的持久化覆盖。flag 默认为 `false`；设为 `true` 即可启用。

::: warning 注意
不同引擎对实验 flag 的支持存在差异：rust 引擎当前实现了 `hook-command-rewrite` 与 `session-consistency-follow`；`tool-select` 仅在 TypeScript 引擎中生效。
:::

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `tool-select` | `boolean` | `false` | 将 MCP 工具 schema 从顶层 tools 数组中移除，模型按需加载 |
| `hook-command-rewrite` | `boolean` | `false` | 允许 `RewriteToolInput` hook 在执行前修改工具参数（如 rtk 集成） |
| `session-consistency-follow` | `boolean` | `false` | 会话恢复时启动一个 driver，增量吸收其他进程写入会话 journal 的追加内容 |

## `services`

`services` 配置网页搜索（`moonshot_search`）和网页抓取（`moonshot_fetch`）两项内置服务。只识别这两个固定 key，其他 key 会被忽略。两项字段相同：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `base_url` | `string` | 否 | 服务 API URL |
| `api_key` | `string` | 否 | API 密钥 |
| `oauth` | `table` | 否 | OAuth 凭据引用，结构同 `providers.*.oauth` |
| `custom_headers` | `table<string, string>` | 否 | 请求时附加的自定义 HTTP 头 |

```toml
[services.moonshot_search]
base_url = "https://api.moonshot.cn/v1/search"
api_key = "sk-xxx"

[services.moonshot_fetch]
base_url = "https://api.moonshot.cn/v1/fetch"
api_key = "sk-xxx"
```

## `permission`

`permission` 设置会话启动时自动加载的权限规则，控制 Agent 调用工具时是否需要用户确认。规则用 `[[permission.rules]]` 数组表写出，按顺序匹配，第一条命中即生效。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `decision` | `string` | 是 | 匹配后的处置：`allow`（直接放行）、`deny`（直接拒绝）、`ask`（每次询问） |
| `scope` | `string` | 否 | 规则有效范围：`turn-override`、`session-runtime`、`project`、`user`；默认 `user` |
| `pattern` | `string` | 是 | 匹配模式，格式为 `工具名` 或 `工具名(参数模式)`，如 `Read`、`Bash(rm -rf*)` |
| `reason` | `string` | 否 | 规则说明，仅用于调试和审计 |

内置工具名见[内置工具](../reference/tools.md)。大多数支持规则参数的内置工具会定义自己的匹配对象，例如 `Bash(command-pattern)` 或 `Read(path-pattern)`。`AgentSwarm`、MCP 工具和自定义工具只能按工具名匹配，不支持参数模式。

```toml
[[permission.rules]]
decision = "allow"
pattern = "Read"

[[permission.rules]]
decision = "allow"
pattern = "Grep"

[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"

[[permission.rules]]
decision = "ask"
pattern = "Bash"
```

::: tip
MCP server 的声明配置写在 `~/.mirri-code/mcp.json` 或项目内 `.mirri-code/mcp.json` 中，不在 `config.toml` 里。交互式配置入口是 `/mcp-config`，详见 [Model Context Protocol](../customization/mcp.md)。
:::

## `tui.toml`

除了 `config.toml`，CLI 还在同一目录下用一份配套的 `tui.toml` 保存终端界面与客户端偏好（`~/.mirri-code/tui.toml`，或覆盖后的 `$MIRRICODE_HOME/tui.toml`）。它在首次运行时以默认值创建，交互式命令 `/config`、`/theme`、`/editor` 会自动写入，通常无需手动编辑。文件格式有误时，CLI 会回退到默认值并给出提示，而不是启动失败。

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `theme` | `string` | `auto` | 配色主题：`auto`（跟随终端）、`dark`、`light`，或[自定义主题](../customization/themes)的名字 |
| `disable_paste_burst` | `boolean` | `false` | 禁用非 bracketed paste 的粘贴突发兜底；默认开启，避免快速多行粘贴被逐行提交 |
| `[editor].command` | `string` | `""` | 编写长输入用的外部编辑器命令；留空则回退到 `$VISUAL` / `$EDITOR` |
| `[notifications].enabled` | `boolean` | `true` | 是否发送桌面通知 |
| `[notifications].notification_condition` | `string` | `unfocused` | 何时通知：`unfocused`（仅终端失去焦点时）或 `always`（总是） |
| `[upgrade].auto_install` | `boolean` | `true` | 是否自动安装新版本 |

```toml
# ~/.mirri-code/tui.toml
theme = "auto" # "auto" | "dark" | "light" | 自定义主题名
disable_paste_burst = false # true 表示禁用非 bracketed paste 的粘贴突发兜底

[editor]
command = "" # 留空则使用 $VISUAL / $EDITOR

[notifications]
enabled = true
notification_condition = "unfocused" # "unfocused" | "always"

[upgrade]
auto_install = true
```

修改在下次启动时生效，或用 `/reload-tui` 立即生效（只重载 `tui.toml`）；`/reload` 会同时重载 `config.toml` 和 `tui.toml`。

## 项目级本地配置

除了 `~/.mirri-code` 下的用户级文件，Mirri Code 还会读取位于 `<项目根目录>/.mirri-code/local.toml` 的项目级本地配置文件。它保存的是与某一个项目检出相关、通常不应与队友共享的设置。

该文件会在你通过 [`/add-dir`](../reference/slash-commands.md) 添加额外工作目录并选择记入项目时自动创建，通常无需手动编辑。

### `[workspace]`

`[workspace]` 表用于存放项目级的工作区设置：

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `additional_dir` | `array<string>` | 否 | 额外工作目录列表，以绝对路径存储。在 `/add-dir` 中确认"记住此目录"时自动写入；启动时读回，使这些目录在该项目的每个会话中都可用 |

```toml
[workspace]
additional_dir = ["/absolute/path/to/shared"]
```

目录以绝对路径存储，与具体机器相关。因此建议把 `.mirri-code/local.toml` 加入项目的 `.gitignore`，避免被提交。

## 下一步

- [平台与模型](./providers.md) — 各供应商类型（Mirri、Claude、OpenAI、Gemini）的接入示例
- [配置覆盖](./overrides.md) — CLI 选项、配置文件、环境变量的优先级规则
- [环境变量](./env-vars.md) — `MIRRICODE_HOME` 等运行时变量的完整列表
