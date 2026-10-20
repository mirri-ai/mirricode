# LLM 感知的子 Agent 模型自动选择

派发子 Agent 时用哪个模型？大多数工具的回答是"由你决定"——你在设置里选一个默认模型，所有子 Agent 都用它，快慢强弱一个价。任务有轻重缓急，模型却只有一个档位，这本身就是错配。

Mirri Code 把这个决策交还给**最了解任务的人**：正在拆解任务的 LLM 自己。

## 这是什么

当主 Agent 通过 `Agent` / `AgentSwarm` 工具派发子 Agent 时，`model` 参数不是配置文件里写死的，而是 LLM 根据任务性质**当场挑选**的：

- 主 Agent 拆出"在整个仓库里找一个函数的定义"——它知道这是轻量探索，派个便宜快模型。
- 拆出"重构这个模块的并发安全逻辑"——它知道这需要深度推理，派个旗舰模型。

关键在于 LLM 拿到的决策信息是充分的：每个子 Agent profile 的工具描述自带 `defaultModel` 和能力说明，`config.toml` 里声明了 `description` 的模型会列在"可用模型"清单上，连同各自的长处。它不是在盲猜，而是在一个带标注的货架前挑货。

模型按三层优先级解析：

1. **LLM 显式指定** — 工具调用中传入的 `model` 参数
2. **Profile 默认** — 子 Agent profile 声明的 `defaultModel`
3. **继承父级** — 主 Agent 当前使用的模型

如果 LLM 传了无效的 model alias，工具会返回错误并附上有效选项列表，它在下一轮自行修正——整个纠偏过程不需要你出手。

## 为什么值得单独一说

对比一下主流做法：

| 方案 | 谁决定模型 | 问题 |
|------|-----------|------|
| 全局固定默认模型 | 你（一次设置） | 重任务欠火力，轻任务烧钱 |
| 每个子 Agent 类型手工绑定 | 你（逐个配置） | 配置面大，且绑定死的档位不随任务变化 |
| **Mirri Code：LLM 按任务挑选** | LLM（每次派发时） | 决策点恰好在"任务已拆解、性质已明确"的位置 |

这个设计的本质是：**任务拆解完成的瞬间，恰好是模型选型信息最充足的瞬间**。主 Agent 比任何静态配置都清楚这个子任务是"找一下"还是"想清楚"，让它顺手做完选型，比让你预先穷举所有组合更准。

## 你要做的决定

把选型交给 LLM 之后，你仍然握有三个决定——而且只有这三个：

**决定一：货架上摆什么。** 在 `config.toml` 里给想进入候选清单的模型写上 `description`，这行文字就是 LLM 挑货时看到的标签：

```toml
[models."glm-5.3"]
provider = "zhipu"
model = "glm-5.3"
max_context_size = 200000
description = "strong reasoning, best for complex coding and refactoring"

[models."mimo-flash"]
provider = "mimo"
model = "MiMo-V2.6-Flash"
max_context_size = 128000
description = "cheap and fast, best for simple lookups and quick tasks"
capabilities = ["image_in", "tool_use"]
```

标签写得越贴合"什么任务该用它"，LLM 挑得越准。反过来说，不写 `description` 的模型不会出现在 LLM 可见的清单中——这是刻意设计：货架只摆你挑选过的货，选择面保持干净。

**决定二：哪条路给默认。** 对"这类活基本就用这个模型"的子 Agent 类型，在 profile 里写上 `defaultModel`，省去 LLM 每次现选：

```yaml
# ~/.mirri-code/agents/media-reader.yaml
defaultModel: mimo-flash
```

比如媒体分析基本总是走廉价多模态模型——这条路的选型通常没有悬念，给它一个默认值，LLM 大多数时候不用费心挑选。但注意它只是**默认**而非锁死：LLM 判断有更好的选择时，依然可以通过 `model` 参数覆盖它。

**决定三：什么都不做。** 这也是一个合法选项。两个决定都跳过时，子 Agent 会按"LLM 现选 → profile 默认 → 继承主 Agent"的顺序兜底，日常使用完全够用。货架可以以后再补。

## 与多模态调度的配合

这个机制与[通过子 Agent 实现多模态访问](./multimodal-via-subagent.md)是同一个调度体系的两层引导：主 Agent 先选**派谁**（subagent → 子 Agent profile），再选**用哪个脑子**（model → 模型别名）。多模态文档中的"给纯文本主模型配副驾"就是两层引导的典型案例——LLM 看到占位符后，从货架上挑出那个带 `image_in` 能力的便宜视觉模型，一次派发完成"看图"这件主模型做不到的事。

反过来，模型自动选择也让多模态配置更省心：你不需要为"看图"单独维护一条派发路径，只要视觉模型在货架上、`description` 写清楚，LLM 在需要看图的时刻自然会选它。

## 相关文档

- [通过子 Agent 实现多模态访问](./multimodal-via-subagent.md) — 两层引导的典型应用：给纯文本主模型配多模态副驾
- [Agent 与子 Agent](../customization/agents.md) — profile 字段、`defaultModel` 优先级和模型解析的完整参考
- [平台与模型](../configuration/providers.md) — 在 `config.toml` 中声明模型、`description` 和 `capabilities`
