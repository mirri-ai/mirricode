# Model / Provider 配置域架构设计（RFC）

> 状态：v2（v1 基础上增补：ADR-005 批量删除与导入时模型选择；C4/C6 对应修订） · 2026-10-28 ·
> 本文是 provider/model 配置域的技术架构（内部规格文档，中文单语）。
> 需求与契约裁决见 [model-settings-page-requirements.md](model-settings-page-requirements.md)（下称「需求文档」，D-M1~D-M9）；现状与代码事实见 [model-settings-context.md](model-settings-context.md)（下称「context 文档」）。
> 功能需求决定"做什么"；本文回答"怎么建"，并把非功能需求（性能、可用性、数据所有权）显式映射到结构。

## 1. Scope

provider/model 配置域的端到端技术架构，六个落点：

1. **rust 引擎目录服务**（models.dev 数据的内存/磁盘生命周期）
2. **RPC 契约面**（`model.*` / `provider.*` / `providerDiscovery.*` / `session.updateMeta` 的语义与形状）
3. **web rust 适配层**（`ws-rpc-api.ts` 的类型、映射与接线）
4. **会话模型切换路径**（选择器 → 后端真实生效）
5. **思考强度表达路径**（用户选择的档位/关闭 → 端点可兑现的请求字段，C7）
6. **供应商接入配置路径**（自建/代理场景下的可修正配置面，C8）

## 2. Goals & Non-Goals

**Goals**

- G1 `model.list` / `model.listProviders` 恰好返回已配置集合（D-M9），名称解析在数据面完成（D-M1）。
- G2 目录数据生命周期：懒加载 + 本地缓存 + config 开关（D-M4）；**网络 IO 永不出现在请求路径上**。
- G3 刷新接线与两个新方法（目录浏览 / 目录导入，D-M2/D-M5）。
- G4 会话模型切换真实生效（不再是静默 no-op）。
- G5 契约兼容：klient snake_case wire、bare-array 语义不变，演进只做加法。
- G6 思考强度的关闭与档位按端点可兑现的写法送达，且界面不给无法兑现的控制项（D-M11，C7）。
- G7 供应商接入配置可反复编辑（类型/端点/请求头/思考关闭方式），改错不必删库重建（D-M12，C8）。

**Non-Goals**：OAuth 登录流、provider 重命名迁移（D-M7）、agent profile 域、TS server 行为变更、目录 UI 增强、统一档位刻度映射（C7.8 末段）。

## 3. 架构驱动（非功能需求 → 结构约束）

| # | 非功能需求 | 对结构的约束 |
|---|---|---|
| N1 性能 | 目录数据首次使用快（用户裁决原文："内容多、网络下载等待长，同步读 UX 不好"） | 目录加载 = 一次性**本地磁盘读**；请求路径零网络 IO；名称解析为内存查表 |
| N2 可用性 | 目录源不可达不伤主流程 | 目录是**降级域**：拉取失败 → 沿用缓存 + 诊断警告，任何 API 不因此报错 |
| N3 数据所有权 | 用户对 config.toml 的显式控制权 | config.toml 是**唯一持久真值**（别名 / provider / 默认模型）；目录缓存是可随时丢弃的派生数据（`rm` 缓存文件不影响配置） |
| N4 安全 | 凭据不外泄 | `apiKey` 永不出现在 `model.list` / `model.listProviders` / 目录浏览响应中（`has_api_key` 布尔除外，对齐 TS 契约） |
| N5 兼容 | TS SDK（klient）契约稳定 | wire 行形状（snake_case 字段集）只做加法；bare-array 断言不变；集合语义变更需过契约测试 |

## 4. 组件设计

### C1 目录服务（engine 内，`EmbeddedServer` 现有字段的正式化）

models.dev 数据的唯一持有者，三个入口一个存储：

```
存储：内存 catalog（Mutex<Option<Arc<Value>>>，整个 api.json）＋ 磁盘缓存 <mirri-home>/models-dev.json
入口：① 懒加载（读）② refresh（写：网络拉取→换内存→按开关写盘）③ listCatalog（读）
```

- **懒加载（N1）**：启动时 spawn 一个**后台预热任务**（只读磁盘缓存，零网络），使多数首次请求命中内存；预热未完成时首次使用**兜底同步读**（一次性本地读，量级 ~100–300ms）。两种路径写同一个内存 catalog，先到先得。
- **读路径零网络（N1/N2）**：`listCatalog` 只从内存/磁盘缓存应答；缓存为空时返回空集 + "需要刷新"提示，**不触发网络**。对话框需要新数据时由 web 层先调 refresh（后台）再拉 listCatalog。
- **持久化开关（D-M4）**：config.toml `[model_catalog]` 域新增 `persist_cache`（默认 `true`）。`false` = 刷新不写盘、启动/懒加载不读盘；已存在的旧缓存文件不读也不删（非破坏性）。
- **降级（N2）**：沿用现有语义——拉取失败保留内存目录或读缓存，失败以 `config.diagnostics` 警告呈现。

### C2 `model.list` 投影管线（G1，D-M9 的执行面）

```
config 别名（to_mirri_config_wire）
  → 逐行投影 { provider, model, max_context_size,
               display_name = 别名.displayName ?? 目录[provider].models[model].name ?? 缺省,
               description?, capabilities?, support_efforts?, default_effort? }
  → bare array（snake_case）
```

**`description?` 为超出 TS 基线 wire 的加法字段（N5）**：供展示类消费者（选择器悬停提示、TUI 等）使用；web 编辑路径走 config 接口（设置页表单数据源是 config，非本接口）。

- **集合 = 别名全集，不再追加目录 overlay 行**（现状的 overlay 追加被移除，见 T2）。
- **`display_name` 解析在投影时完成**（数据面一次性，展示层零逻辑，D-M1）。目录查表为两层 map 查找（provider → model → name），无索引结构。
- **wire 行携带 `id` 字段**（`[models.<id>]` 配置键 = 模型别名，ADR-003；klient zod object 对未知键 strip 不拒绝，加法安全），web 以其作为应用层模型 id；无该字段的旧引擎行由 web 侧退回组装 `provider/model`。

### C3 刷新（G3，D-M2）

`providerDiscovery.refreshProviderModels`（现有方法，web 接线）：拉目录 → 换内存 → 按开关写盘 → 返回 `{changed, unchanged, failed}`。`changed` 的语义 = 该 provider 在**目录中**的模型条目数变化（提示"可添加"），**不改变已配置别名集合**。

### C4 目录浏览 / 导入（G3，两个新 RPC）

- 两个方法名中的 "catalog" 指 **models.dev 目录数据**（见需求文档 §2.1 catalog 警示——非引擎内 `ModelCatalog` 注册表）。
- `providerDiscovery.listCatalog()` → 目录条目数组（`{id, name, wire_type?, needs_base_url, env_key?, models[]}`，snake_case）。只读缓存（C1），零网络。目录模型行携带能力事实（`capabilities?`（能力字符串，词表同别名）、`reasoning?`（布尔，目录 thinking）、`context_length?`）——klient 契约加法（zod strip 兼容），与 TS REST `catalogModelItemSchema` 对齐（ADR-004）。
- `providerDiscovery.importCatalog({catalog_id, id?, api_key?, base_url?, exclude_model_ids?})` → 在**一个 config 写事务**内：写 `[providers.<id>]` + 为该 provider 的每个**未被排除**的目录模型写别名（`exclude_model_ids` 为导入时未勾选的模型清单，落 `removed_model_ids` 墓碑，ADR-005）——别名按目录事实携带**推导字段**（ADR-004）：`provider` / `model` / `maxContextSize`（`limit.context`）+ `maxOutputSize`（`limit.output`）/ `capabilities`（`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/`dynamically_loaded_tools`，按目录 `modalities.input`/`tool_call`/`reasoning`/`dynamically_loaded_tools` 推导）/ `reasoningKey`（`interleaved.field`）/ `supportEfforts`（`reasoning_options` 的 effort 档，排除关档）；不可用目录模型（无正整数 `limit.context`、`status` 为 deprecated/alpha、输出模态不含 `text`、名字带 embedding 标记）整条跳过，浏览与导入集合同源（对齐 TS `modelsDevProviderModels` 过滤）；**不写 displayName**（名称靠 C2 解析链）与 **description**（curated 门控是用户显式动作，US-9）→ 返回 `{provider, models_imported}`。
- 导入对**已存在 provider** 为 merge 语义（ADR-004）：已配置别名逐字节保留（用户自定义字段不被目录覆盖）、被用户删除的模型（`removed_model_ids` 墓碑）不复活、只追加目录新模型。全新环境（无默认模型）导入后以第一个非墓碑模型种子默认（对齐 TS 导入语义；`already_exists` 域错误保留给手动建 provider，US-4）。

### C5 会话模型切换（G4）

`session.updateMeta` 扩展 `model` / `thinking` 可选字段（web 现有调用形状不变，零 web 改动）：

```
updateMeta { session_id, model?, thinking?, title?, tags? }
  → model 存在时：校验 ∈ 已配置别名集合（否则 model.not_found 域错误）
  → session.agent_scope.set_model(model_id) / set_thinking(effort)   ← 引擎已有 API
  → title/tags 路径不变
```

切换的真实性由 `session.status` 回读验证（需求 US-11 验收）。

### C6 web rust 适配层（`ws-rpc-api.ts`）

| 改动 | 内容 |
|---|---|
| 类型修正 | `RpcModel` / `RpcProvider` 字段改为 snake_case（`display_name` / `max_context_size` / `has_api_key` / `base_url` / `default_model`…），对齐 klient wire。`status` 按两态返回（D-M6）；TS wire schema 的 `'error'` 枚举值从不被赋值，rust 同样不产生——枚举兼容保留，非行为收窄 |
| id 组装 | `toAppModel` 的 id = wire `id`（`[models.<id>]` 配置键，ADR-003）——切默认模型、切换会话模型、删除等**一切回写以该别名 key 定位**；无 wire `id` 的旧引擎行退回组装 `${provider}/${model}`（仅展示 key，不回写）。收藏 id 在 `loadModels` 时按 `provider/model` 组合键一次性迁移到别名 key |
| 接线 | `refreshAllProviders` / `refreshProvider` → C3；`listCatalogProviders` / `importCatalogProvider`（携带导入排除清单）→ C4；`deleteProvider` → `config.removeProvider`（级联删除别名 + 清理默认模型指针；`provider.delete` 刻意不级联，仅删 provider 记录）；`deleteModel`（单个与批量同接口）→ `model.delete`（批量走别名数组，单次写事务 + 永久排除标记，ADR-005） |
| 组合 | `addProvider` = 两次 `config.set` 分节写（providers 节 + models 节，对齐 TS `createProvider`），别名键规范形态；全新环境（无任何默认模型）时种子默认模型，已有默认不动 |
| 展示 | Composer 模型 pill 按 AppModel 名称渲染（不再渲染原始 `status.model` 串） |

### C7 思考开关表达（D-M11 的执行面）

思考强度的"关闭"在 OpenAI 兼容端点上没有统一写法，且"省略字段"在多数厂商处意为"使用厂商默认"——对推理模型即"继续思考"。C7 把"用哪个字段表达关闭"从适配器类型中解耦，改为可解析的**思考方言**（[glossary](glossary.md) TS1）。

#### C7.1 三种表达法

| 方言取值 | 关闭时的请求体 | 使用该方言的端点 |
|---|---|---|
| `thinking_type` | `"thinking": {"type": "disabled"}` | MiMo、DeepSeek、Zhipu GLM、Kimi K2.x、MiniMax |
| `reasoning_effort` | `"reasoning_effort": "none"` | OpenAI（逐模型支持）、DeepSeek（等效写法） |
| `enable_thinking` | `"enable_thinking": false` | Qwen / DashScope |

`kimi` 适配器（`KimiLlm`）已硬编码 `thinking_type` 的关闭写法，但把档位放在思考对象**内部**（`{"type":"enabled","effort":...}`），与本表 C7.3 "对象内不带 effort"的结论不一致。C7 的处理：该适配器自行拥有其方言，`thinking_off` 配置对它不生效（配置面在 provider 类型为 `kimi` 时隐藏该字段），避免出现"配了但不生效"的控制项；其档位编码维持现状，作为已知偏差记录，不在本批修正。

#### C7.2 解析链

```
模型级 thinking_off  →  provider 级 thinking_off  →  内置端点表  →  无结论
```

- **模型级优先**：同一 provider 内语法分裂时（Kimi K2 认对象、K3 只认顶层档位）用别名级覆盖。
- **非法取值降级而非中断**：无法识别的显式取值继续走下一层，由配置体检报告该值——一个拼写错误不能静默关掉一个本来已知的模型的思考控制。
- **内置端点表**按主机名匹配（重建 provider 或改名不失效），再按模型名前缀分叉（同厂商跨代际）。表内不含 OpenAI：其 `reasoning_effort: "none"` 仅部分模型支持，端点级猜测会把"关不掉"变成 400；OpenAI 系模型的可关闭性由目录事实逐模型写入。
- **无结论 = 不可关闭**：三层都无结论时，`off_allowed = false`，界面不提供关闭项（D-M11.4）。

#### C7.3 请求编码规则

对每个请求，先取"是否显式关闭"与"目标档位"两个输入，再按方言落字段：

| 方言 | 显式关闭 | 档位（模型声明了档位） | 档位（未声明档位） |
|---|---|---|---|
| `thinking_type` | `thinking: {type: disabled}` | 见下"档位字段" | **不发任何字段** |
| `reasoning_effort` | `reasoning_effort: "none"` | `reasoning_effort: <level>` | `reasoning_effort: <level>` |
| `enable_thinking` | `enable_thinking: false` | `enable_thinking: true` | `enable_thinking: true` |
| 无方言（legacy） | 不发任何字段 | `reasoning_effort: <level>` | `reasoning_effort: <level>` |

**档位字段不是随手加一个顶层 `reasoning_effort` 就成立。** 核实到的厂商文档里，顶层 `reasoning_effort` 只被 DeepSeek、Kimi K3、OpenAI 系声明；MiMo、Kimi K2、MiniMax、Zhipu GLM 的文档只描述思考对象（GLM 另有其自己的 `reasoning_effort`，但那是它自己的方言组合）。因此 `thinking_type` 行的"档位"列按端点分两种：

- **该端点同时声明了顶层档位字段**（如 DeepSeek、GLM）→ 发 `reasoning_effort: <level>`。
- **该端点只声明思考对象**（如 MiMo、Kimi K2、MiniMax）→ **不发档位字段**，档位仅用于限制界面可选范围。发一个文档未声明的字段是各家拒收的典型形态，宁可不发。

设计要点：

- **`thinking` 对象不含档位**：所有被核实厂商都不接受对象内的 effort 字段，档位只走顶层兄弟字段。
- **`enable_thinking` 需显式开启**：该方言的端点默认可能不思考（部分模型默认关），且目录数据里这类模型通常只声明开关、不声明档位数组。因此只要不是显式关闭，就发 `enable_thinking: true`——否则用户在界面选"开"或选某个档位都不会生效。
- **`always_thinking` 短路**：模型声明永远思考时，即使调用方传来"显式关闭"，也按"不发字段"处理——UI 没有该选项、wire 也不会出现该字段，两侧一致。
- **未配置 intent 时保持既有行为**：直接构造的适配器（测试、其他调用点）不设 intent，走 legacy 行，历史 wire 不变。

#### C7.4 能力面：`always_thinking` 作为有效能力

`model.list` 行携带的 `capabilities` 是**配置声明**。C7 在其上补一层**有效能力**：

- **只对走 OpenAI 方言的供应商计算**。`anthropic` / `openai_responses` / `google-genai` / `kimi` 各自按自己的协议表达关闭（省略即关、或专用字段），它们没有"方言缺失"这回事；若对它们也算一遍，所有 Anthropic 模型都会失去关闭项。
- **只对声明了思考能力的模型追加**。模型根本没声明思考时，追加"永远思考"会把它从"不支持思考"误标成"始终思考"，并让思考控件出现一个本不该有的"开"档。
- 两个条件同时成立且解析不出方言时，有效能力含 `always_thinking`（即便配置未写）。

web 端的思考控件已按该标记生成分段（含标记时不产出关闭项，见 context 文档 §8.2），但**说明性文案尚未实现**：只在控件旁隐藏关闭项，没有任何文字解释"为什么没有关闭"。C7 要求补一句说明（"此模型始终思考"），否则用户只会觉得控件缺了一块（R4 信号真实性）。

区分"声明"与"有效"的意义：声明是模型事实（用户可编辑，导入时写入），有效是"该引擎当前能否兑现"。两者分离后，语法未知的网关自动退化为不可关闭，而不是依赖用户手工补一条永远思考标记。

#### C7.5 配置形状

```toml
[providers.xiaomi-token-plan-cn]
type = "openai"
base_url = "https://token-plan-cn.xiaomimimo.com/v1"
thinking_off = "thinking_type"          # provider 级；可省略（内置表已知 MiMo）

[models."kimi-code-plan-cn/k3"]
provider = "kimi-code-plan-cn"
model = "kimi-k3"
thinking_off = "reasoning_effort"       # 模型级覆盖（K3 拒收 thinking 对象）

[models."zhipuai/glm-5.3"]
provider = "zhipuai"
model = "glm-5.3"
capabilities = ["thinking", "always_thinking", "tool_use"]   # 强制思考，关不掉
```

- 两个键都在 round-trip 白名单内（`config/wire.rs` 的 `KNOWN_PROVIDER_ENTRY_KEYS` / `KNOWN_MODEL_ENTRY_KEYS` 与 camelCase 别名表），否则配置体检会把它们报成未知键、写回时会丢掉。
- **默认零配置**：内置表覆盖已知厂商，用户不需要写任何一行；`thinking_off` 只是逃生门。

#### C7.6 目录导入派生（撤销 G-18 的 `always_thinking` 挂账）

`models_dev_alias_record` 在既有的 `support_efforts` / `reasoning_key` 派生上补两项（目录数据已编码足够信息，见 context §8.5）：

| 目录 `reasoning_options` | 派生 |
|---|---|
| 含 `toggle` | 可关闭；`thinking_off` 保持未写（由端点表解析） |
| `effort` 值含 `"none"` | 可关闭；`thinking_off = "reasoning_effort"` 写入别名（=`none` 是该档位的关闭表达） |
| 其余（仅 effort 或空） | `capabilities` 追加 `always_thinking` |

`offEffort` 字段本身仍不引入（其语义是"关闭对应的 effort 取值"，在 `reasoning_effort` 方言下已由固定 `none` 覆盖；在 `thinking_type` 方言下没有对应物）。

#### C7.7 会话创建时的默认档位

模型声明的默认档位解析（`default_effort`，缺省取 `support_efforts` 中位）目前只在切换模型路径执行。C7 要求创建会话时走同一条解析：请求未显式给思考强度时，按模型声明初始化；显式给出时以请求为准。否则"导入即带档位"对新会话无效，用户必须先切一次模型。

#### C7.8 全局思考配置节：移除，由模型级声明取代

`[thinking]` 节的三个字段都不该以"全局"形式存在，理由是各自都有更准确的归属：

| 字段 | 现状问题 | 取代者 |
|---|---|---|
| `effort` | 全局常量作用于**跨模型**，而档位刻度逐模型不同——存在合法性问题（对某模型合法的 `high` 对另一模型非法） | 模型级 `default_effort`（缺省取 `support_efforts` 中位），已有解析规则且天然落在该模型的能力范围内 |
| `enabled` | 与"模型能否关闭"重复；且"全局关闭思考"是跨模型的强断言 | 模型级可关闭性（C7.2/C7.4）+ 会话级选择 |
| `keep` | 无任何请求路径消费者，文档描述的发送行为来自已退役的引擎 | 无——随本节一并移除 |

**移除语义**：存量 `config.toml` 中的该节按未知节透传保留（不改写用户文件），引擎不消费；配置体检输出"该节已废弃，`effort`/`enabled` 请改用模型级 `default_effort` 与思考控件"的提示。**写入端同步收掉**：界面上的档位选择不再写入全局节——它只落当前会话（会话级才是该选择的真实归属），避免保留一条"写进废弃配置"的无意义持久化副作用。

**能力等价性**：移除后"新会话默认思考强度"由 C7.7 的创建时解析承担——模型声明 → 缺省中位 → 未声明则不设置。这与全局配置相比是能力等价、少一个跨模型常量，不构成功能回退。

**不做统一刻度映射**：`support_efforts` 保持目录导入的原值透传（`low`/`high`/`max` 等厂商刻度），产品层不引入"统一档位语义 → 厂商值"的映射层。理由：映射表带来持续的维护成本（每家厂商增删档位都要跟），而导入已自动填好这些值、用户无需理解刻度的来源；目标用户以技术人员为主，原值可读。此裁决否定了"用产品抽象刻度收敛厂商差异"的方案。

### C8 供应商接入配置的编辑面（D-M12 的执行面）

自建 API 服务、走代理、多模型聚合网关这三类用法有一个共同点：接入配置需要**试错修正**。当前的面只有两个单点编辑（凭据 key、端点地址），改类型只能删库重建——而重建会连已配置的模型清单一起丢掉。C8 把供应商的接入配置收敛成一个可反复编辑的表单。

#### C8.1 字段范围与写入落点

| 字段 | 写入落点 | 现成通道 |
|---|---|---|
| 协议类型 | 供应商记录的 `type` | `provider.set` 接受任意记录，已能改 |
| 端点地址 | 供应商记录的 `baseUrl` | 已有单点编辑对话框 |
| 自定义请求头 | 供应商记录的 `customHeaders`（名称 → 值） | 无界面，配置可读 |
| 思考关闭方式 | 供应商记录的 `thinking_off`（C7.5） | 无界面，配置可读 |
| 凭据 | **不在此面**——归凭据域，经凭据库写入 | 已有单点编辑对话框 |

**"自定义"是创建模式，不是一种协议类型。** 界面上"自定义"指的是手动填写 id + 类型 + 端点 + 凭据的创建路径；引擎侧没有名为 `custom` 的协议（该取值会被显式拒绝）。文档与界面文案都必须区分：**模式**是可自由填写，**类型**是协议枚举。

#### C8.2 类型切换的语义与前置校验

切换 `type` 改变的是"用哪种协议与端点对话"。三件事因此改变，一件不变：

| 变化 | 说明 |
|---|---|
| 传输适配器 | 按新类型分派 |
| 端点地址的默认值 | 新类型若有一方官方地址，清空端点即回落到它；没有则必须显式填写 |
| 凭据的约定变量名 | 类型决定约定的环境变量名——但凭据库按**供应商身份**存放，所以已存凭据不丢 |
| **不变的：模型别名** | 别名只引用供应商 id 与原始模型名，与协议无关 |

**前置校验的两条硬规则**：

1. **只提供本引擎能建立连接的协议类型**。引擎不支持的协议不得出现在可选项里——"选了之后报错"违反 R5，且用户无法从错误中判断是配置错还是产品不支持。
2. **端点地址必填性按新类型判定**。保存时若新类型无官方默认地址且端点为空，拒绝保存并指出缺失项；不落一个"看着配好了、请求时才炸"的记录。

#### C8.3 保存的原子性

一处校验失败则整次保存不落盘，既有配置原样保留。这一条与既有的配置写协议同源（load → mutate → validate → persist → reload），C8 不新增写通道，只是把多个字段放进同一次提交。

### C9 模型专属提示词（D-M13 的执行面）

三件东西：一张内置规则表、一个文件目录、一条读取接缝。

#### C9.1 内置规则表

一张有序的 `(原始模型名匹配模式, 提示词正文)` 常量表，编译进二进制（**不随用户配置**，用户既不改也不看它）。模式匹配只作用于**原始模型名**（M2）——不是模型别名（M1，用户自定义），也不是 `provider/model` 复合串。匹配在规范化后的小写形态上做，故用户不必关心大小写。

首批条目：

| 模式 | 目标模型 | 提示词正文 |
|---|---|---|
| `mimo-v2.6-*` | `mimo-v2.6-pro`、`mimo-v2.6-flash` | 见下 |

正文（英文，与基座模板语言一致）：

```text
Keep your internal reasoning strictly within what the task requires. Do not
produce long, performative <thinking> monologues. Go straight to acting: emit
the task's next concrete step or the tool call it needs.

Before a tool call, keep the reasoning short and focused on the immediate next
action. Do not speculate beyond the available evidence and do not re-litigate
your own conclusions.

Plan at most three tool actions per round. When you already have enough
evidence to answer, summarise and answer — do not keep expanding the
investigation.
```

规则表是纯数据 + 纯函数（输入原始模型名，输出可选的提示词正文），不读文件、不读配置、不访问网络——可被单测穷举。

#### C9.2 文件目录与身份

**位置**：`~/.mirri-code/model_prompts/`——即应用数据目录（`home_root()`，实际解析链为 `MIRRICODE_HOME` → `$HOME/.mirri-code` → 相对 `.mirri-code`）下的 `model_prompts/`。与既有 `SYSTEM.md` 同一父目录、同一条"组合根读文件、引擎不自己读"的通道。

**文件名**：由原始模型名经统一安全化规则生成（详见 C9.3），后缀 `.md`。

**文件形态**：

```markdown
---
model: "mimo-v2.6-pro"
---
<提示词正文>
```

**身份 = frontmatter 的 `model` 字段，文件名为其附属**。读取时按目录枚举 + 解析 frontmatter 建 `原始模型名 → 正文` 映射，**从不从文件名反推模型名**。理由：安全化是有损的（`a/b` 与 `a-b` 会得到同一个 slug），只有内容自陈才能保证身份唯一。两个文件声明同一 `model` 时按文件名的字典序取第一个并输出一条诊断——确定性优先于随机。

`model` 值一律双引号包裹输出（兼容含 `:`、`#` 的名称）；解析时同时接受裸值与引号值。

#### C9.3 统一安全化规则（路径安全）

**唯一规则，模型域内所有"模型名 → 文件路径"的场合共用**（当前只有 C9.2 一处消费者；新增消费者必须复用本函数，不得另写一套）。

按序执行：

| 步骤 | 规则 | 理由 |
|---|---|---|
| 1 | 转小写 | macOS/Windows 默认大小写不敏感；小写化使三端映射一致。**这一步计入有损判定**——`Mimo-Pro` 与 `mimo-pro` 必须得到不同文件 |
| 2 | 保留 `[a-z0-9._-]`，其余字符**每个**折成 `-` | 覆盖 `/`、`\`、空格、`:`、全角字符等全部路径敏感字符 |
| 3 | 连续 `-` 折叠为一个 | `Qwen/Qwen2.5` 不产生 `--` |
| 4 | 去首尾的 `-` `.` `_` | 首字符 `.` 在 Unix 变隐藏文件；`..` 在任一路径拼接位置都是穿越风险；Windows 不允许尾随点 |
| 5 | 结果为空 → `model-<8位十六进制指纹>`，**到此终止**（不再走步骤 8） | 纯符号或纯非 ASCII 名称不留空路径；终止避免 `model-<fp>-<fp>` 的双指纹 |
| 6 | 命中 Windows 保留设备名（`con` `prn` `aux` `nul` `com1`–`com9` `lpt1`–`lpt9`）→ 前缀 `model-` | 这些名字在 Windows 上无法创建为文件。前缀本身**不计入有损**（保留名是有限的封闭集合，加前缀即为唯一映射，无碰撞可能） |
| 7 | 截断至 `100 - 9 = 91` 字符（按 Unicode 标量，不切多字节） | 与步骤 8 的 9 字符指纹合计不超过 100，留出目录路径与 `.md` 后仍在各平台长度限制内 |
| 8 | **当且仅当**步骤 1–4 或步骤 7 任一有损（转了小写、折叠了字符、去了首尾、或发生了截断）→ 追加 `-<8位十六进制指纹>` | 无损失的名字保持干净可读（`mimo-v2.6-pro.md`）；有损的名字必须能与其"干净的兄弟"区分——否则 `a/b` 与 `a-b`、`Mimo` 与 `mimo` 会静默共享一份文件 |

指纹取原始（未处理的）模型名的 32 位 FNV-1a 十六进制。步骤 8 是这条规则的关键：没有它，安全化就是**有损且不可逆**的，而文件系统不报错，故障表现为"另一个模型的提示词莫名其妙生效了"。

规则的自检不变式（单测须覆盖）：对任意两个不同的原始模型名，若两者安全化结果相同，则两者都带指纹且指纹不同。

#### C9.4 生成（物化）接缝

**接缝位置：配置提交管线，而不是用户动作 RPC。** 模型进入 `[models]` 的写面至少四条（`provider.set`、`model.set`、`config.set`、`config.replaceSections`，后者可直写任意顶层节）。逐个 RPC 挂钩会漏掉 `config.set` 直写、手工编辑 config.toml 后的 reload、以及恢复路径。因此接缝定义在**配置持久化管线**（load → mutate → validate → persist → reload）的提交处：凡提交后使某个原始模型名**首次出现**于 `[models]`，即对其调用 `ensure_model_prompt_file`。

**语义**：

1. 规则未命中 → 什么都不做（不生成空文件）。
2. 规则命中且目标文件不存在 → 写入 frontmatter + 正文。
3. 规则命中且目标文件已存在 → **什么都不做**（单向覆盖；不比较内容、不更新、不告警）。
4. 任何写失败 → 记诊断、不阻断配置提交（提示词是增强能力，不得让配置动作失败）。

**不改写已有文件**是本机构的硬约束：规则表升级只影响此后新加入的模型。用户读到"产品替我写的那份"之后做的任何修改，产品永远不覆盖。

**存量模型（读取时兜底）**：接缝在配置提交管线上，只能覆盖"提交之后新出现"的模型；升级前已在配置里的模型不会再经历一次提交。因此**读取接缝在规则命中而目录中无对应文件时，惰性物化一次**（写文件、纳入本次结果），语义与提交时完全一致（仍遵守第 2、3 条：文件不存在才写）。不做则升级前的存量模型（正是首批 `mimo-v2.6-*` 的主要受众——它们只能手动新增）永远拿不到提示词，且无声无息。

**触发面口径**：任何使原始模型名首次进入配置的动作——目录导入、手动新增供应商、手动新增模型、编辑模型改名、`config.set` 直写节、手工编辑 config.toml 后的 reload、会话恢复——经配置提交管线**一处收口**，不逐个枚举。

#### C9.5 读取（注入）接缝

**读取时机**：会话创建、以及会话恢复（恢复路径同样走 agent 创建，`restore.rs` 的 `create_agent` 调用点）时一次。读取结果物化为 `原始模型名 → 正文` 的一张表，随会话配置下发；agent 创建时按自身当前模型查表。查表未命中但规则命中且目录无文件时，先走 C9.4 的惰性物化，再纳入本次结果。

**注入位置与三条路径的差异**（此处必须分述——三条路径的刷新语义不同，一刀切的"末尾追加"会让其中一个静默丢失）：

| 路径 | 装配形态 | 注入落点 |
|---|---|---|
| 内置基座模板（无自定义提示词、无 SYSTEM.md） | 两趟槽位替换 | **末尾追加**（`apply_workspace_part` 之后） |
| SYSTEM.md 覆盖 | 同上，覆盖文本替换固定部分后再跑工作区趟 | **末尾追加**，同上 |
| 自定义完整提示词 | 捕获 base → 追加 AGENTS.md 后缀 → 追加目录树后缀 | **并入被捕获的 base**（即 AGENTS.md 后缀之前） |

第三条的约束来自既有刷新机制：自定义分支在**追加任何后缀之前**捕获 `base_system_prompt`，压缩后的 `refresh_agent_system_prompt` 只重建 AGENTS.md 与目录树两段后缀。若把模型提示词追加在目录树之后，一次压缩刷新即**静默丢失**。因此该路径必须把模型提示词并入被捕获的 base——代价是它不位于最终文本的最末尾（后面还有工作区段），这是"不丢失"优先于"位置美观"的取舍。前两条路径的 base 为 `None`（永不刷新），故可以确实地追加在末尾。

`rust-core-design/10` §4 的验收"同一会话内创建与刷新产出的 prompt 字节一致"对自定义路径给出了一条可执行的判据：模型提示词必须在这条不变式覆盖的范围内，因此它属于 base 而非后缀。

**装配顺序的事实源**是 `docs/specs/rust-core-design/10-agent-context-assembly.md` §2（三段固定顺序 + "同一会话内创建与刷新产出的 prompt 字节一致"这条验收）。C9 新增的是第四段，必须同批更新该文档的 §2 与验收，否则两处规格互相矛盾。

**未命中/空文件**：不追加任何内容，最终系统提示词与未启用本能力时逐字节一致。

**会话中途切换模型**：不重算（与需求 D-M13.5 一致）。此后新建的子 agent 按各自模型取各自条目。

**体积上限**：单份文件正文超过 16 KiB → 整篇不注入，输出一条含**绝对路径与实测大小**的诊断，并作为会话可见的装配诊断上报（不只落日志——用户需要知道"这个模型这次没得到提示词"，否则他只会看到模型变回老毛病而没有归因线索）。**不截断**——截断会得到半句指令，比没有更危险（模型会照着半句执行）。

#### C9.6 整体关闭

config.toml 顶层布尔 `disable_model_prompts`（默认 `false`，即默认启用）。为 `true` 时**读取接缝**整体短路：不枚举目录、不注入任何条目、也不做惰性物化。已存在的文件原样留在磁盘上（关闭的是注入，不是删除）。

双向语义：

- **只关注入，不关物化与编辑**：关闭期间用户新增/导入模型仍会生成文件（生成发生在配置提交管线，与注入无关），编辑入口仍可用。理由：物化与编辑是"文件的产生与维护"，注入是"是否对模型说话"——用户关掉后者通常是不想被产品的判断影响，而不是想放弃自己写提示词的能力。
- **重新开启**：此后新建（或恢复）的会话恢复注入既有文件，内容即磁盘上的当前内容，无需重新生成。
- **编辑在关闭态仍有效**：写下的内容在重新开启后的新会话生效。

该开关**不参与热重载**：读时生效，改动影响此后创建/恢复的会话——与"会话创建时确定"同一语义，避免运行中的会话在中途改变系统提示词。

#### C9.7 编辑面

一对读写 RPC：`model.getPrompt` / `model.setPrompt`（方法名定名，不再留"如"）。**两者都只接受原始模型名，不接受路径**——文件名由服务端按 C9.3 派生，客户端无法指定落点。这样 `model_prompts/` 之外的写入在协议层就不可能，C9.7 的两道路径校验因此降级为服务端的不变式断言（防未来改动引入路径参数），而不是当前的攻击面。

语义：读按原始模型名返回该文件的原文、是否存在、以及是否命中内置规则；写提交原文并落盘（父目录不存在时创建）。原文为空串 = 写入一个 frontmatter-only 文件（语义为"该模型明确不要额外提示词"，见 D-M13.2）。

写入成功后不刷新任何已存在会话——需求 D-M13.5 已定注入随会话创建时固定，编辑生效于新建或恢复的会话。界面必须明示这一点（否则用户会以为"保存了却没生效"是缺陷）。

## 5. 边界与接缝

**Agent 工具的模型感知面（契约见需求文档 D-M10/US-14，实现落点在 agent 工具域）**：main agent 通过 Agent / AgentSwarm 工具的 `model` 参数为 sub agent 指定模型。目标契约：清单 = 已配置模型的 curated 子集（`description` 门控，见需求文档 D-M10.1），每条携带模型别名（config 键，规范名见需求文档 §2.1 / glossary）+ 描述 + 能力标注，**不携带显示名**（显示名是 web UI 概念，D-M1 链只服务人类界面；LLM 感知面以模型别名为唯一标识，见需求文档 D-M10.2），错误反馈内联 curated 清单（含描述）；数据源是**已配置模型注册表**（代码名 `ModelCatalog`，config 别名投影；见需求文档 §2.1 catalog 警示——非目录数据），与本域 C2 投影管线共享同一已配置模型集合。rust 现状为裸别名清单（见 context 文档 §1.2），属实现缺口。本域职责仅限：维护好该数据面（别名集合管理 + 字段完整性）。

## 6. 关键序列

- **S1 选择器打开**：`listModels`（立即可用：别名 + 既有名称解析）→ 并行后台 `refreshAllProviders`（C3）→ 完成后重拉 `listModels`（名称兜底更新）。失败只提示（D-M3）。
- **S2 冷启动首次 `model.list`**：后台预热通常已就绪；否则兜底读磁盘缓存（一次性）→ 投影。
- **S3 目录导入**：`listCatalog`（缓存）→ 用户提交 → `importCatalog`（单事务写 config）→ 重拉 `listModels` / `listProviders`。
- **S4 切换会话模型**：`updateMeta {model}` → 别名校验 → `set_model` → `session.status` 回读一致。

## 7. Trade-offs（被拒绝的选项）

| # | 被拒选项 | 拒绝理由 |
|---|---|---|
| T1 | 刷新时把目录模型/名称写回别名（TS 刷新行为） | 违反 D-M2/N3：批量修改用户 config.toml |
| T2 | `model.list` 保留全目录 overlay 行，web 按"provider 已配置"过滤 | 集合逻辑进入每个消费者；已配置 provider 的目录新模型会漏过滤（只有别名级判断才准确）；AppModel 携带不可用行。改为服务端集合收窄（C2），klient 契约测试仅断言 bare-array，收窄安全 |
| T3 | ~~rust 在 wire 行加 `id` 字段~~（**已被 ADR-003 推翻**） | 原拒绝理由是"id 是 web 视图概念，web 组装零成本"；实测组装 id 顺着写链路回传（切换会话模型 / 设默认模型），短名别名下引擎解析不到，静默落到错误 provider 的环境变量死胡同（`ANTHROPIC_API_KEY not set`）。模型选择的用户裁决：链路必须以模型别名（config 键）传递，解析失败须明确报错、不得擅自换模型 |
| T4 | 启动时同步预载缓存到内存 | 阻塞启动（用户裁决：懒加载不阻塞）；改为后台预热 + 兜底读（C1） |
| T5 | 目录数据入结构化存储 / 建名称索引 | 过度设计：~20MB JSON 内存查表（两层 map）已满足 N1；索引是缓存之缓存 |
| T6 | 选中目录模型时隐式创建别名 | 已随 D-M9 否决（隐式写用户配置） |
| T7 | 思考关闭用统一复合对象承载（开关 + 档位一个字段发出去） | 见 ADR-006：档位一律是顶层兄弟字段，任何统一对象都会在至少一家产出非法字段；且部分模型拒收思考对象 |

## 8. Risks & Mitigations

| # | 风险 | 缓解 |
|---|---|---|
| R1 | 缓存文件大（models.dev api.json ~10–20MB），兜底同步读阻塞首请求 ~100–300ms | 后台预热使常态命中内存；一次性成本，仅目录启用时存在；`persist_cache=false` 可完全关闭 |
| R2 | models.dev schema 漂移（字段缺失/改名） | 解析按宽松 JSON（缺字段降级为无名/0），refresh 已是此模式；R2 只损失名称兜底质量 |
| R3 | overlay 行移除影响未知 klient 消费者 | 契约测试仅断言 bare-array（集合无断言）；实施前 grep `model.list` 全仓消费者核对 |
| R4 | `importCatalog` 半途失败留下部分配置 | config 写走既有 load→mutate→persist→reload 原子管线，单事务语义；失败即整体不落盘 |
| R5 | rust config 新增 `[model_catalog]` 节与现有解析冲突 | 实施时按 mirri-core config 节约定加字段 + 测试；旧 config 无此节走默认值 |
| R6 | persist_cache=false 时旧缓存文件残留引起困惑 | 文档化语义：不读不删；用户可手删（N3：缓存是可丢弃派生数据） |
| R7 | 内置端点表按主机名匹配，网关/代理主机名不在表内 → 关闭项消失 | 用户可显式 `thinking_off` 覆盖（C7.5）；界面在隐藏关闭项的同时提示该端点未声明关闭方式与可配置位置，用户有明确下一步 |
| R8 | 目录事实与端点真实行为不符（目录说可关、端点实际拒收） | 导入派生的 `thinking_off` 只是一种方言选择，用户可改；请求被拒时错误照常浮现，可据此改配置。风险落在"多一次往返"，不落静默失真 |
| R9 | `thinking_off` 非法取值被静默忽略 | 解析链降级（不中断）但配置体检报错；测试覆盖"降级 + 告警"两条路径 |
| R10 | 切换协议类型后，旧类型下的可选模型集与新类型不兼容（例如某模型只在原协议下可用） | 类型切换不清空模型清单（D-M12.2：别名与协议无关），但**首次请求可能失败**。缓解：切换保存时提示"模型清单保留，若新端点不支持其中某些模型需自行删除"，把判断权交给用户，不擅自删数据 |
| R11 | 自定义请求头可承载任意值 → 用户把自己的凭据写进请求头明文落盘 | 该字段是显式配置项（与凭据库的写路径不同），风险落点是"用户主动明文写入"。缓解：字段旁明示"值会写入配置文件明文保存"，并把密钥类用法指向凭据库 |

## 9. Success Criteria

1. 集合正确性：`model.list` 行数 == config 别名数（US-1 验收的数量断言）；`model.listProviders` 同理。
2. 性能：冷启动首次 `model.list`（含兜底缓存读）P95 < 500ms；预热后稳态 P95 < 10ms；刷新期间并发请求零阻塞。
3. 切换真实性：`updateMeta {model}` 后 `session.status` 回读一致；未配置模型报 `model.not_found`。
4. 降级：断网状态下全部 API 可用（名称退化为原始模型名 / 目录浏览提示需要刷新）。
5. 门禁：rust workspace 测试 + clippy + zero-io 全绿；web typecheck + 现有测试；klient 契约 e2e 不回归。
6. 思考关闭真实性：对每个内置方言，显式关闭的请求体携带该方言的关闭字段（`thinking.type: disabled` / `reasoning_effort: none` / `enable_thinking: false`），而非省略；声明 `always_thinking` 的模型在收到显式关闭时不产生任何思考相关字段。
7. 无谎报：`model.list` 的有效能力对"无可用方言"的模型含 `always_thinking`，web 的 `segmentsFor` 据此不产出 `off` 段（现有单测已覆盖该分支，验收断言其输入来自引擎）。
8. 供应商可修正性：改类型/端点/请求头/思考关闭方式后，下一次请求即按新配置发出（无需重启）；类型切换后模型清单与凭据数量不变（数量断言）。

## 10. Rollout（每批独立可交付可验证）

| 批次 | 内容 | 生效 |
|---|---|---|
| B1 | web casing 修正 + id 组装 + pill 名称渲染（C6 前两行） | 用户已配置的 display name 立即恢复；纯 web 改动 |
| B2 | rust：`session.updateMeta` 的 model/thinking 生效（C5）+ `model.list` overlay 收窄与名称解析链（C2） | 切换真实生效；选择器集合正确 |
| B3 | rust：目录懒加载 + 后台预热 + `[model_catalog].persist_cache`（C1）；web：refresh 接线（C6） | 重启后名称兜底可用；刷新可用 |
| B4 | rust：`listCatalog` / `importCatalog`（C4）；web：添加/删除 provider、目录导入接线（C6 余项） | 设置页全功能 |
| B5 | rust：Agent 工具 LLM 感知面 curated 门控（§5，D-M10/US-14）——`ModelCatalog::curated_models` description 门控（D-M10.1）；`SubagentHost::curated_models`（`SessionSubagentHost` 经 `model_catalog` 实现）；`AgentTool`/`AgentSwarmTool` 描述模型节改列 curated 清单（`- 别名 — 描述, Nk context (能力)`，别名 = config 键，无 displayName，D-M10.2，清单空整节省略）；spawn 别名校验错误内联 curated 清单（20 条截断，D-M10.4） | LLM 感知面只呈现带描述的 curated 别名；无描述模型对人类界面可见、对 LLM 隐藏；LLM 可据错误自纠 |
| B6 | rust：`importCatalog` 目录推导字段（capabilities / maxOutputSize / reasoningKey / supportEfforts + 不可用模型过滤，ADR-004）+ merge 语义（既有别名保留、墓碑不复活、追加新模型）+ 全新环境种子默认模型；klient：browse 目录模型行加法补 `capabilities?`/`reasoning?`；web：rust mapper 修正（停发硬填 `reasoning:false`，透传 capabilities） | 目录导入即能力：导入的模型无需手动编辑即具备正确能力标注；重复导入不再报错（对齐 TS merge）；新装首个 provider 导入后开箱即用 |
| B7 | web：custom `addProvider` 原子化（两次 `config.set` 合并为单次分节写）+ 模型 id 命中目录时预填 capabilities/maxContextSize/maxOutputSize（目录事实做表单默认值，用户可改） | 手动添加获得目录级体验 |
| B8 | rust：`model.delete` 批量化 + 墓碑（ADR-005）；`importCatalog` `exclude_model_ids` 入参（ADR-005）；klient 契约加法。web：provider 详情多选 + 批量删除（单次提交）、`load()` 选中 provider 无中间态（过滤词保留）、Add provider 对话框可勾选模型清单（默认全选 + 清单内搜索 + 实时计数）+ 导入排除清单接线 | 批量删除单次写事务；单删后过滤条件保留；导入前可排除模型且排除/删除永久生效 |
| B9 | rust：思考方言三级解析 + 内置端点表 + 三种方言的请求编码（C7.1–C7.3）；`thinking_off` 两个配置键入 round-trip 白名单；`model.list` 有效能力追加 `always_thinking`（C7.4） | 已知厂商的"关闭思考"真实生效；无可用方言的模型不再显示关闭项 |
| B10 | rust：`importCatalog` 派生 `always_thinking` / `thinking_off`（C7.6，撤销 G-18 的 `always_thinking` 挂账）；会话创建时解析模型默认档位（C7.7）；`[thinking]` 节移除 + 废弃体检提示（C7.8）；非法 `thinking_off` 体检告警 | 导入的模型开箱即带正确的可关闭事实；新会话默认档位生效；废弃配置不再静默 |
| B11 | web：模型编辑表单增加"思考关闭方式"（自动 / 三种取值 / 不发送），默认"自动"、位于思考折叠区并与推理字段名覆盖并列；关闭项隐藏时给出可配置位置提示（R7）；移除 `persistGlobalThinking` 的全局写入（C7.8 的 web 侧收尾——档位选择只落会话，不再写已废弃的全局节） | 未知网关的用户可自助修复；推理模型用户不再遇到无效开关；切档位不再产生无效的持久化副作用 |
| B12 | web：供应商编辑面扩展为完整接入配置表单（类型 / 端点 / 自定义请求头 / 思考关闭方式），类型下拉只列引擎可建立的协议；保存原子、失败不落盘；切换类型的模型保留提示（C8，D-M12/US-16） | 自建/代理用户改错配置不必删库重建；不再出现"选了不支持的协议直到请求才失败" |

每批完成跑对应门禁与 changeset；B1 不依赖 rust 改动可先行合入。

## 11. 决策记录（ADR）

### ADR-003 · 2026-09-12 · `model.list` 行携带 `id`（别名 key），推翻 T3

**背景**：用户在 web 选择 rakuten 的 DeepSeek V4 Flash（别名 `fast`）发送消息失败。根因链：T3 裁定 wire 行不带 `id` → web 组装 `${provider}/${model}` 当 UI id → 该组装 id 顺着"切换会话模型 / 设默认模型"写链路原样回传引擎 → 引擎按别名精确匹配失败 → 落到内部默认 Anthropic 通道 → `ANTHROPIC_API_KEY not set` 死胡同。同一根因还导致：默认模型徽标显示裸别名 `ultra` 而非 display_name（`models.find(id === 'ultra')` 匹配不到组合 id）；Composer 模型面板空白（当前模型解析失败 → 当前 provider 的模型列表为空）。

**决策**（用户裁决）：模型信息传递链路以**模型别名（`[models.<id>]` 配置键，模型唯一 key）**为唯一标识。`model.list` 行新增 `id` 字段（加法，klient zod strip 兼容）；web `toAppModel` 以 wire `id` 为应用层模型 id，一切回写以别名 key 定位；无该字段的旧引擎行退回组装 key（仅展示）。引擎侧（`resolve_model_alias_in`）保留 `provider/model` 组合 id 的防御性解析与"未配置明确报错"，兼容存量客户端。

**后果**：收藏 id 由 web 在 `loadModels` 时按组合键一次性迁移；T3 标记为被本 ADR 推翻。

### ADR-004 · 2026-09-18 · 目录导入别名携带目录推导字段；导入 merge 语义

**背景**：用户在 rust + web 实测"添加 provider 后模型没有正确 capabilities"。根因（证据见 context 文档 §6）：C4 原定的最小别名 `{provider, model, max_context_size}` 对 capabilities 从未裁决——无意识遗漏而非决策；而契约基准 TS 的导入从目录推导 capabilities/maxOutputSize/reasoningKey/supportEfforts 等并写入别名（`modelsDevModelToRecord`）。目录数据在 rust 的唯一运行时消费是 display_name 兜底，capabilities 没有等价兜底链，运行时能力解析只剩 config 声明 ∪ 名字 marker 启发式——导入模型的能力普遍缺真。另有一处失真：原 C4 "provider id 冲突 → 已存在域错误（对齐 TS 40921）" 与 TS 导入实际行为不符——40921 是**手动建 provider** 的错误面；TS 导入对已存在 provider 是 merge（保留既有别名、墓碑生效、仅追加新模型），web 对话框（alreadyAdded 徽标）亦按 merge 假定设计。

**决策**：

1. 导入别名从"最小三字段"扩为"目录事实推导字段"：`capabilities` / `maxOutputSize` / `reasoningKey` / `supportEfforts` 随导入写入（对齐 TS `modelsDevModelToRecord`；`defaultEffort` 不在 TS 导入写集内，不写）；不可用目录模型（无正整数 `limit.context`、deprecated/alpha、非 text 输出、embedding 标记）**整条跳过**——`maxContextSize` 恒为正，原"缺省落 0"路径消除（G-22 随此裁决收口）。`displayName` 仍不写（D-M1 解析链）、`description` 仍不写（curated 门控是用户显式动作）。不违反 D-M2/N3：导入是用户显式动作写 config，refresh 仍永不写别名（T1 维持）。
2. 能力词表取两侧共享集：`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/`dynamically_loaded_tools`。`maxInputSize`/`offEffort`/`always_thinking` 暂不引入（rust `ModelAlias` 无字段落点、引擎无 `always_thinking` 消费方，记 followup G-18）。**（2026-11-07 修订：`always_thinking` 部分被 ADR-006 第 6 条取代——消费方已就绪，导入时写入；`maxInputSize`/`offEffort` 维持不引入。）**
3. 导入对已存在 provider 改为 **merge 语义**（对齐 TS 基线）：既有别名逐字节保留、`removed_model_ids` 墓碑不复活、仅追加新目录模型；`already_exists` 域错误保留给手动建 provider（US-4）。
4. 导入在无默认模型时以第一个非墓碑模型种子默认（对齐 TS 导入；与 C6 的 addProvider 种子语义同口径）。
5. browse 目录模型行补发能力事实（`capabilities?`/`reasoning?`）——klient 契约加法（zod strip 兼容，旧引擎不发即缺省），web 停止硬填 `reasoning:false`。

**被拒绝的备选**：

- **读时 capabilities 目录兜底链**（仿 D-M1 display_name 链）：能力真值分裂在 config 与目录两处，且与 TS 基线（目录止于导入写入，运行时 = 声明 ∪ 检测）不一致。拒绝。
- **refresh 时补写 capabilities 进别名**：违反 D-M2/N3（T1 裁决维持）。拒绝。
- **仅 web 端推导**（客户端从 browse 行拼 capabilities 提交）：browse 行原本无能力字段，且推导散落客户端后 CLI/TUI 等其他消费者不可复用；推导属引擎数据面。拒绝。

**影响范围**：C4（本 ADR 修订）、需求文档 US-5（SDR-2026-09-18）、klient `providerDiscovery` 契约、web `ws-rpc-api.ts` 映射、ROADMAP B6/B7。

### ADR-005 · 2026-10-28 · 删除批量化与永久排除语义；导入时模型选择

**背景**：三个用户报告的交互缺口。(1) 模型只能逐个删除，删除 N 个模型 = N 次 config 写事务，且无批量操作面；(2) 单删后 provider 详情的过滤词丢失——`load()` 整体替换列表导致详情组件在同一次 flush 中出现 `undefined` 中间态被 `v-if` 卸载重挂，本地过滤 state 归零（现状证据见 context 文档 §7）；(3) `importCatalog` 只能全量导入目录模型，大 provider 上百个模型一次性涌入，用户被迫导入后再逐个删除——而删除（`model.delete` 路径）不写 `removed_model_ids` 墓碑，重导入时全部复活，清理成果不可保持。

**决策**：

1. **删除即永久排除**：`model.delete` 在删除别名的同时落 `removed_model_ids` 墓碑（复用既有 merge 语义的墓碑机制，ADR-004 第 3 条），使"删除过的模型不复活"对现行删除路径同样成立。手动删除的别名想恢复时重新手动添加即可（显式动作优先，墓碑不阻止 `model.set`）。
2. **删除批量化**：`model.delete` 的入参从单个别名放宽为**别名数组**（单元素等价单删，同一接口、同一语义），一个 config 写事务内完成：逐个删除别名 + 落墓碑 + 默认模型在被删范围时清指针。前端批量操作走该接口一次提交；被拒绝的备选是前端循环调单删（N 次写事务，违背"配置写入只发生一次"的用户要求）。
3. **导入时模型选择**：`importCatalog` 增加 `exclude_model_ids` 可选入参（导入时未勾选的模型清单）；同一写事务内对被排除模型落墓碑，仅写入勾选模型。缺省（不传）= 全量导入，既有调用方行为不变。
4. **web 交互**：provider 详情模型列表加多选（checkbox + 全选含半选态 + 批量操作栏），批量操作作用于当前过滤可见范围；`load()` 重取数据时保持选中 provider 无 `undefined` 中间态（过滤词随组件存活而保留）；Add provider 对话框的模型预览改为可勾选清单（默认全选、清单内搜索只影响显示不影响勾选、实时"将导入 N / 共 M 个"）。
5. **klient 契约**：`model.delete` 参数与 `importCatalog` 参数均为加法演进（可选/放宽，zod strip 兼容，N5）。

**被拒绝的备选**：

- **纯前端批量**（循环单删）：N 次写事务 + 无墓碑，两条用户要求都违背。拒绝。
- **导入后逐个 `model.delete` 排除**：墓碑语义本 ADR 之前不成立；即便成立也是 N+1 次写事务。拒绝。
- **通用 config 批量写协议**（`config.set` 支持删除语义）：改的是全局配置协议，影响面远超模型域。拒绝。

**影响范围**：C4（`importCatalog` 入参）、C6（`deleteModel` 接线与批量交互）、需求文档 US-5/US-10（SDR-2026-10-28）、klient `model.delete`/`importCatalog` 契约（后随 SDR-002 引擎退役作废，wire 由 rust 引擎层与 web `ws-rpc-api.ts` 直接约定）、web `ProviderDetail.vue`/`AddProviderDialog.vue`/`ModelsPanel.vue`、ROADMAP B8。

**实施补录（2026-10-28 终审裁决，两条边界语义）**：

1. **重加别名的墓碑清除粒度收窄**：`config.set` 重加某别名时，只清除**该模型自身**的墓碑，不再清空整个 `removed_model_ids` 清单（原"整表清空对齐 TS"的平价依据随 SDR-002 退役失效）。语义：批量删 a/b/c 后手动重加 a，b/c 仍永久排除。
2. **导入排除只作用于将导入的模型**：重复导入已配置 provider 时取消勾选一个**已配置**的模型，效果是"该模型被墓碑（未来不再追加）+ 既有别名保留"——排除不回删既有别名（merge 从不删）。用户想移除已配置模型应走删除（US-10）。

### ADR-006 · 2026-11-07 · 思考关闭方式与传输适配器解耦（三种方言 + 三级解析）

**背景**：用户报告"把思考切到关闭后模型仍在思考"。抽象问题：端点用不同字段表达"停止推理"，而引擎把该表达硬编码在**传输适配器类型**上，导致同一传输下的所有端点被迫共用一种（多数情况下是"省略字段"）表达法——而省略字段在推理模型上是"使用厂商默认"，即继续思考。用户动作被静默吞掉：界面显示控制项、系统接受操作、真实行为不变，且没有任何信号可供察觉。

同类问题还有第二面：同一厂商跨代际会更换语法（早代用思考对象、新代只认顶层档位），把语法绑在"厂商/适配器"维度上无法容纳这种分裂。第三面是能力面：模型"能否关闭"是模型事实，与"用哪个字段关闭"是两个维度，现状只有前者的一半表达（`support_efforts`），另一半（永远思考）在 rust 侧无落点，而 web 侧的消费方早已就绪并有测试。

**决策**：

1. **引入"思考方言"作为独立维度**，与传输适配器类型正交。三种取值（`thinking_type` / `reasoning_effort` / `enable_thinking`），各自对应一类端点的关闭写法（C7.1）。
2. **三级解析**：模型级 `thinking_off` → provider 级 `thinking_off` → 内置端点表（按主机名，再按模型名前缀分叉）。无法识别的显式取值降级到下一层，由配置体检报告，不中断。
3. **无结论即不可关闭**：解析不出方言时，不伪造关闭动作——`off_allowed = false`，界面隐藏关闭项（D-M11.4、R5 推论）。
4. **能力面分"声明"与"有效"两层次**：`capabilities` 是配置声明；`model.list` 投递的有效能力在"无可用方言"时追加 `always_thinking`，使未知网关自动退化为不可关闭，而不依赖用户手工标注。
5. **请求编码按方言分表**（C7.3）：`thinking` 对象不带档位（所有被核实厂商都拒绝对象内 effort）；`enable_thinking` 方言在任何非关闭状态下都需发开启标记（目录里这类模型通常只声明开关、不声明档位数组，漏发会让"开"和选档位双双失效）；顶层 `reasoning_effort` 只在端点确实声明该字段时才发，其余端点不发档位字段。`always_thinking` 只对**关闭载荷**短路（不发关闭字段），不抹掉方言本身——否则该模型的档位编码会退回到无条件发 `reasoning_effort` 的历史行为。
5b. **有效能力只在 OpenAI 方言且模型声明思考时计算**（C7.4）：其它适配器各自按协议表达关闭；未声明思考的模型保持"不支持思考"的呈现。
6. **导入派生 `always_thinking` 与 `thinking_off`**：目录的 `reasoning_options` 已编码"能否关闭"（context §8.5），按 C7.6 的三行规则写入别名。此项撤销 ADR-004 第 2 条中"`always_thinking` 暂不引入"的挂账（G-18），`offEffort` 仍不引入。
7. **会话创建时解析模型默认档位**（C7.7）：与切换模型路径同口径。
8. **`[thinking]` 全局节移除**（C7.8）：`effort` 由模型级 `default_effort` 取代、`enabled` 由模型级可关闭性 + 会话级选择取代、`keep` 是未实现残留一并移除。存量配置按未知节透传保留（不改写用户文件），体检提示已废弃。不移除会造成"全局跨模型常量"与"逐模型异质刻度"的持续冲突。

**被拒绝的备选**：

- **统一复合对象承载开关与档位**：九家厂商文档核实显示档位一律是顶层兄弟字段、从不嵌在思考对象内，且部分模型拒收思考对象。统一对象在任何一家都会产出非法字段。拒绝。
- **改用既有的思考对象适配器**：该适配器会额外发送对象内档位字段，而文档化该对象的厂商只声明了开关字段——修好关闭、弄坏档位；且同厂商跨代际语法分裂会立刻坏掉一个既有 provider。拒绝。
- **每厂商新增一个 provider 类型**：`type` 已承担"用哪个传输适配器"，再叠加"用哪种思考语法"会让两者互相绑架。拒绝。
- **任意 JSON 载荷直塞请求体**：引擎无法校验，且提供请求体注入面。拒绝。
- **保留关闭项并加警告文案**：假开关的问题不是缺提示，而是用户不知道该怀疑它。拒绝。
- **运行时回查目录推导可关闭性**：能力真值分裂在配置与目录两处、用户无法覆盖；延续既有"导入即写入、运行时不回查目录"口径。拒绝。
- **保留 `[thinking]` 全局节并接上（作为全局默认档位）**：全局 `effort` 是跨模型常量，而档位刻度逐模型而异——对某模型合法的值对另一模型非法，接上必须引入"不合法怎么降级"的规则，等于把厂商差异塞进产品语义。且与模型级 `default_effort` 重复。拒绝。
- **保留全局节但只加体检告警**：留下一个"有编辑器、有写入方、永不生效"的配置面，用户会持续被误导。拒绝。
- **引入产品统一档位刻度 + 到厂商值的映射表**：能收敛厂商差异，但映射表需随每家厂商增删档位持续维护；且导入已自动填好 `support_efforts`、大幅降低认知门槛，目标用户以技术人员为主，原值可读。收益不抵长期维护成本。拒绝。

**影响范围**：C7（新增）、C2 投影（有效能力）、C4 导入派生（C7.6）、ADR-004 第 2 条（`always_thinking` 挂账撤销）、需求文档 D-M11/US-15/US-9/US-6/SDR-2026-11-07、context 文档 §8、glossary TS1/TS2/TS3、ROADMAP G-18 与实施批。

### ADR-007 · 2026-11-07 · 供应商接入配置作为可反复编辑的面

**背景**：自建 API 服务、走代理、多模型聚合网关三类用法的共同点是接入配置需要试错修正。当前的产品面只有两个单点编辑（凭据密钥、端点地址），而协议类型、自定义请求头没有任何界面入口——改类型只能删库重建，而重建会连已配置的模型清单一起丢掉。抽象问题：**接入配置在创建时刻一次性确定、之后不可修正**，把一次填错的成本放大成"重建整个供应商"。这类错误在自建/代理场景下不是例外而是常态。

另有一处一致性缺口：创建对话框提供的协议类型选项中包含本引擎无法建立连接的取值，选择后到请求时才失败——用户无法从错误判断是配置问题还是产品不支持。

**决策**：

1. **接入配置成为可反复编辑的面**：协议类型、端点地址、自定义请求头、思考关闭方式（C7.5）合并为一个编辑表单；凭据不在其中（写入口归凭据域）。
2. **类型切换不触碰模型别名与凭据**：别名只引用供应商身份与原始模型名，凭据按供应商身份存放，两者都与协议无关。切换后立即可用，无需重启。
3. **只提供引擎能建立连接的协议类型**：不可建立的协议不出现在可选项。这是 R5（不提供点了会报错的入口）在配置面的具体化。
4. **端点地址必填性随"是否一等供应商"判定**：一等供应商（openai / anthropic / google-genai / kimi）可留空并回落到官方默认地址；其余（自建、代理、目录导入的第三方）必须填写，保存时拒绝并指出缺失项。判据是**供应商 id** 是否属于一等集合，不是协议类型——一个 id 为 `gw`、类型为 `openai` 的供应商即使类型有一等默认地址，引擎也不会替它套用。
5. **保存原子**：任一字段校验失败整次不落盘，既有配置原样保留。
6. **区分"创建模式"与"协议类型"**：界面上"自定义"指手动填写的创建路径，引擎侧没有同名协议类型；文案与校验都必须按这两个概念分开表述。

**被拒绝的备选**：

- **保持现状，靠删库重建改配置**：重建会丢模型清单，而清单往往是用户精心筛选过的（含永久排除标记）；在自建/代理场景下这次成本会反复发生。拒绝。
- **只加"改类型"一个入口**：解决了最痛的一项，但自定义请求头同样没有入口，而它正是代理场景最常见的配置项。半开的面会让用户继续走配置文件。拒绝。
- **在编辑面提供引擎不支持的全部协议类型**（含文档中标注仅另一引擎支持的取值）：与 R5 冲突，且用户看到选项就无法判断支持与否。拒绝——不支持的类型应当不存在于可选项，而不是存在且报错。
- **类型切换时清空模型别名以示"配置变了"**：别名的正确性不依赖协议，清空只会造成数据丢失。拒绝。

**影响范围**：C8（新增）、需求文档 D-M12/US-16/SDR-2026-11-07、US-6（仅接口面引用，写路径不变）、C6 web 适配层（供应商编辑接线）、ROADMAP G-33/G-34 与实施批 B12。

### ADR-008 · 2026-11-08 · 模型专属提示词：内置规则物化为用户可改的文件

**背景**：模型级的行为倾向差异（想得太久、铺陈过多、迟迟不进入动作）在每一个回合都可见，因此校正它的价值很高。两类做法各有硬伤：让用户自己写，等于要求用户先从行为上诊断出问题、再知道该说什么——绝大多数用户走不到这一步；由产品静默注入，用户既不知道系统对自己的模型说了什么，也没有任何修正手段，一旦内容不合意就成了无法申诉的黑盒。

第三个维度是**跨供应商的同一模型**：模型特性属于模型本身，不属于接入方式。按别名组织提示词会让同一个模型在不同供应商下需要各写一份，且别名是用户自定义的显示层概念，不适合承载模型事实。

**决策**：

1. **规则表内置，独立于用户配置**：编译进二进制，用户不配置也不修改。规则命中是产品判断，不是用户负担。
2. **命中即物化**：导入供应商 / 新增模型时命中规则，即在应用数据目录的 `model_prompts/` 下生成该模型的一份 Markdown 文件。文件是普通文本，用户可打开、可版本管理、可分享。
3. **单向覆盖**：文件存在即永不改写；规则表升级只作用于此后新加入的模型；文件被清空视为"该模型明确不要"，不重新填充。
4. **身份由内容自陈**：文件 frontmatter 记录原始模型名，读取时以它为准，文件名只是人类可读的附属物。
5. **统一的路径安全化规则**：模型名到文件名只有一套规则（C9.3），且**有损时必附指纹**——安全化不可逆，若无指纹，`a/b` 与 `a-b` 会静默共享同一份文件，故障表现为另一个模型的提示词莫名生效。
6. **读取接缝在会话创建时**：与既有"提示词在会话创建时装配"的语义一致；物化为 `原始模型名 → 正文` 表随会话配置下发，agent 创建时查表。会话中途换模型不重算，此后新建的子 agent 按各自模型取值。
7. **注入 = 末尾追加**：三条既有提示词路径（内置基座模板 / SYSTEM.md 覆盖 / 自定义完整提示词）一律在最终结果末尾追加，不替换任何既有段；未命中时逐字节不变。
8. **超限不截断**：单份超过 16 KiB 整篇不注入并给出可定位诊断——半句指令比没有指令更危险。
9. **整体可关闭**：顶层布尔一次性退出，关闭的是注入而非删除。

**被拒绝的备选**：

- **只提供配置项，全部交给用户**：用户裁决明确否决（"把使用认知负担推给用户很不友好"）；且用户不读源码就无从知道产品对模型有什么预期。拒绝。
- **静默注入、不落文件**：用户无法审查、无法调整，内容不合意时零手段。拒绝。
- **按模型别名组织提示词**：同一模型换别名即需重写；别名是显示层自定义概念，不承载模型事实。拒绝。
- **每次导入或每次启动按规则刷新文件**：产品每次改进话术都会静默覆盖用户调校；用户对此的唯一对策是"别升级"。拒绝。
- **文件名即身份（从文件名反推模型名）**：安全化有损不可逆，大小写差异与标点折叠会导致跨模型串味，且故障静默。拒绝。
- **复用既有的 `safe_file_stem` 或 MCP 工具名的 `sanitize`**（见 context §9.4）：两者都为各自域服务——前者目标是"工具结果落盘文件名"，后者是"LLM 工具标识符"，都不处理 Windows 保留设备名，且只有后者带指纹而指纹规则未覆盖大小写折叠。复用会把两处域语义耦合进模型域，且仍然需要扩展才能满足 C9.3 的自检不变式。拒绝复用；代价是仓库里存在第三套安全化函数，收益是模型域拿到一套自洽、可穷举验证的规则（三者服务对象不同，不构成重复实现）。
- **提示词写进 config.toml 的模型别名**：长文本污染配置文件的可审阅性，失去"普通文件"的编辑/版本管理形态。拒绝。
- **按 provider 分目录存放**：同一模型的特性不因供应商而变，分目录会让同一模型产出多份互相漂移的文本。拒绝。
- **在适配器层按请求实时读取文件**：每回合读盘、且提示词会在会话中途变化（打破 provider 前缀缓存）；与既有"会话创建时装配"语义冲突。拒绝。
- **用基座模板的槽位承载模型提示词**（如新增 `{{model_prompt_section}}`）：形态最自洽，但槽位只存在于"无自定义提示词"的两条路径——自定义完整提示词路径根本不渲染模板，等于让该路径的用户静默失去本能力；且槽位位置写死在模板里，用户无法调整"模型提示词放在哪里"。改为在各路径的装配点追加（C9.5），保持三条路径一致可用。拒绝槽位方案。
- **读取时不兜底、只靠提交时物化**：升级前已在配置中的模型不再经历提交，永远拿不到提示词且无提示；而首批 `mimo-v2.6-*` 恰恰只能手动新增，正是受影响人群。拒绝（见 C9.4 的惰性物化）。

**影响范围**：C9（新增）、需求文档 D-M13/US-17/SDR-2026-11-08、§6 契约面（提示词读写方法 + 顶层关闭开关）、context 文档 §9、glossary 新增 MP1、ROADMAP 新增实施批与开放项、**`docs/specs/rust-core-design/10-agent-context-assembly.md` §2 与验收标准**（新增第四段，组装顺序的事实源在该文档，不在画像域——画像域只定义覆盖入口）。**本域边界**：只定义"哪个模型有什么额外提示词"及其实物管理；装配顺序的事实源是 rust-core-design/10。**上下文注入域边界**：模型专属提示词是静态系统提示的组成部分，不是回合注入物——不进上下文注入域。
