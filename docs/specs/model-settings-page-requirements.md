# Model 设置页需求文档（User Stories）

> 状态：v10（v9 基础上增补：手动创建允许空模型集合（SDR-2026-10-13，US-4 修订）） · 2026-10-13 ·
> 本文是 provider/model 配置功能形态的唯一事实源（内部规格文档，不进文档站构建，中文单语）。
> 契约基准为 TS v2 server（kap-server）的现有行为；运行目标为 rust 后端。
> 技术架构（组件、非功能需求映射、序列、Trade-offs）见 [model-settings-architecture.md](model-settings-architecture.md)（下称「架构文档」）。
> 现状盘点（根因、差异矩阵、代码事实）见 [model-settings-context.md](model-settings-context.md)（下称「context 文档」），本文不重复。
> 验收标准统一采用 Given-When-Then 格式；优先级 P0 > P1 > P2。
> 本文只定义页面层与数据面的呈现、交互、契约语义；OAuth 登录流程、MCP、agent profile 域（用户另行处理）不在本文范围。

## 0. 功能闭环总览

设置页不是 config.toml 的 CRUD 界面，而是「**配置 → 运行 → 感知 → 回读**」完整闭环上的用户操作面：用户在这里改变可用模型集合，集合驱动会话运行与 LLM 的 sub agent 模型决策，运行结果回读 UI 验证生效。闭环拆成两张图，各讲一件事：

- **图 A · 用户操作闭环**：配置 → 选择 → 切换 → 运行 → 回读，用户直接可见的路径；
- **图 B · LLM 模型决策感知面**：闭环点 ⑤ 的细化——模型集合如何被 main agent LLM 感知、`model` 参数如何被校验、sub agent 运行结果如何汇回状态回读（D-M10/US-14）。

两图共享两个锚点：**config.toml（唯一持久真值，图 A 的 ④ 即图 B 的数据源）**与**会话状态回读**。闭环上任何一条边断开，都表现为"配置了但不生效"或"看得到但选不了"——本轮评审前的 rust 后端即是实例（会话模型切换静默 no-op、选择器整体 unavailable、感知面只有裸别名清单）。**两图的每条边都是验收闭环点：一条边不通过对应的需求条目，闭环即未合拢。**

### 图 A · 用户操作闭环

```mermaid
%%{
  init: {
    "theme": "base",
    "themeVariables": {
      "fontFamily": "system-ui, sans-serif",
      "primaryColor": "#ffffff",
      "primaryBorderColor": "#cbd5e1",
      "primaryTextColor": "#0f172a",
      "lineColor": "#94a3b8",
      "clusterBkg": "#f8fafc",
      "clusterBorder": "#e2e8f0"
    }
  }
}%%
flowchart TD
    subgraph UI["用户面 · web UI"]
        SETTINGS["设置页 Models 面板<br/>添加 · 导入 · 编辑<br/>删除 · 刷新 · 设默认<br/>配置 LLM 可见性"]
        PICKER["chat 模型选择器<br/>浏览 · 搜索 · 切换"]
        PILL["当前模型指示"]
    end

    subgraph STORE["持久层"]
        CONFIG[("config.toml<br/>别名 · provider · 默认模型<br/>唯一持久真值")]
        DIR[("models.dev 目录数据<br/>可丢弃缓存")]
    end

    subgraph RUNTIME["引擎与会话运行时"]
        ENGINE["会话运行时<br/>turn 装配<br/>别名 → provider 调用"]
        STATUS["会话状态回读<br/>session.status"]
    end

    SETTINGS ==>|"① 写<br/>（唯一持久化入口）"| CONFIG
    DIR -.->|"名称兜底 · 导入素材"| SETTINGS
    CONFIG ==>|"② 已配置模型集合"| PICKER
    PICKER ==>|"③ 切换生效<br/>（不写 config）"| ENGINE
    CONFIG ==>|"④ 装配 · 别名解析"| ENGINE
    ENGINE ==>|"⑥ 运行"| STATUS
    STATUS ==>|"⑦ 回读 · 名称链渲染"| PILL
    STATUS -->|"⑧ 回读反哺"| PICKER

    classDef core fill:#eff6ff,stroke:#3b82f6,stroke-width:2px,color:#1d4ed8;
    classDef neutral fill:#ffffff,stroke:#cbd5e1,stroke-width:1px,color:#475569;
    classDef surface fill:#f8fafc,stroke:#cbd5e1,stroke-width:1px,color:#0f172a;
    classDef success fill:#f0fdf4,stroke:#22c55e,stroke-width:1.5px,color:#166534;

    class CONFIG,ENGINE core;
    class SETTINGS,PICKER neutral;
    class DIR surface;
    class PILL,STATUS success;

    linkStyle 0,2,3,4,5,6 stroke:#3b82f6,stroke-width:2px;
```

### 图 B · LLM 模型决策感知面（闭环点 ⑤ 细化）

```mermaid
%%{
  init: {
    "theme": "base",
    "themeVariables": {
      "fontFamily": "system-ui, sans-serif",
      "primaryColor": "#ffffff",
      "primaryBorderColor": "#cbd5e1",
      "primaryTextColor": "#0f172a",
      "lineColor": "#94a3b8",
      "clusterBkg": "#f8fafc",
      "clusterBorder": "#e2e8f0"
    }
  }
}%%
flowchart LR
    subgraph DATA["数据面"]
        CONFIG[("config.toml<br/>别名集合")]
        CATALOG["已配置模型注册表<br/>（ModelCatalog）<br/>感知面数据源"]
    end

    subgraph AGENTRUNTIME["agent 运行时"]
        AGENTTOOL["Agent / AgentSwarm<br/>工具描述<br/>Available models 清单"]
        LLM["main agent LLM<br/>决策指定 model"]
        SUB["sub agent spawn<br/>别名校验"]
        STATUS["会话状态回读<br/>汇入图 A ⑥⑦"]
    end

    CONFIG -->|"别名投影"| CATALOG
    CATALOG -->|"清单注入"| AGENTTOOL
    AGENTTOOL ==>|"⑤ 感知面<br/>（D-M10）"| LLM
    LLM ==>|"model 参数<br/>（模型别名）"| SUB
    CONFIG -.->|"校验 ∈ 别名集合"| SUB
    SUB -->|"运行结果"| STATUS

    classDef core fill:#eff6ff,stroke:#3b82f6,stroke-width:2px,color:#1d4ed8;
    classDef neutral fill:#ffffff,stroke:#cbd5e1,stroke-width:1px,color:#475569;
    classDef decision fill:#fffbeb,stroke:#f59e0b,stroke-width:1.5px,color:#92400e;
    classDef success fill:#f0fdf4,stroke:#22c55e,stroke-width:1.5px,color:#166534;

    class CONFIG core;
    class CATALOG,AGENTTOOL,LLM neutral;
    class SUB decision;
    class STATUS success;

    linkStyle 2,3 stroke:#3b82f6,stroke-width:2px;
```

**闭环点对照表**（两图的每条边 → 需求条目，缺一即断线）：

| 图 | 闭环点 | 语义 | 需求条目 |
|---|---|---|---|
| A | ① | 设置页是别名的**唯一持久化入口**——手动添加与目录导入都经此落 config；刷新不写 | US-4/5/6/7/8/9/10（D-M2） |
| A | ② | config → 选择器：清单**恰好等于**已配置集合，名称按 D-M1 链解析 | US-1（D-M1/D-M9） |
| A | ③ | 选择器切换 → 会话**真实生效**（状态可回读验证，非乐观 UI；不写 config） | US-11 |
| A | ④ | config → 会话运行时装配（别名 → provider 调用；默认模型决定新会话起点）；也是图 B 感知面的数据源 | US-8（引擎既有路径，契约不变） |
| A | ⑧ | 状态回读反哺选择器（切换后重开时当前项与清单一致） | US-1/US-11 |
| B | ⑤ | config → 已配置模型注册表 → 工具描述 → LLM 感知 | US-14（D-M10） |
| B | 别名投影 | config → 注册表（同一 curated 投影，数据面完成） | US-14 |
| B | 清单注入 | 注册表 → 工具描述（curated 门控：仅写 `description` 的别名） | US-14 |
| B | model 参数 | LLM 决策值（模型别名）→ spawn 传递 | US-14 |
| B | 别名校验 | 校验 ∈ 已配置别名集合；错误内联 curated 清单 | US-14 |
| B | 结果汇回 | sub agent 运行汇入状态回读 | US-12（A⑥⑦） |
| A | ⑥⑦ | 运行 → 状态回读 → UI 名称渲染（pill / 选择器当前项）；图 B 的运行结果汇回此处 | US-12 |
| A | 目录边（虚线） | 目录数据只做**名称兜底与导入素材**，永不直接进入任何集合或感知面 | US-3/13（D-M1/D-M2/D-M4） |

## 1. 角色定义（Personas）

| 角色 | 场景 | 核心诉求 |
|---|---|---|
| **web UI 操作者** | 在浏览器里管理 provider/model 并选择聊天模型（loopback 本地场景） | 不改 config.toml 就能完成全部配置；模型名人类可读；改完即生效 |
| **远程管理员** | 在无外设服务器上跑轻量后端 | API 优先的语义不受页面能力影响（配置面是叠加不是替换） |

## 2. 术语与集合定义（全文唯一口径）

本文所有 User Story 只允许引用以下集合口径（三个集合 + 一个子视图），禁止使用"所有可用模型"这类无对照的表述：

| 术语 | 定义 | 出现在哪里 |
|---|---|---|
| **已配置模型**（别名集合） | config.toml `[models."provider/model"]` 中登记的别名，每条都可通过其 provider 的凭据实际调用。进入该集合的唯一途径是用户的显式动作：手动添加（US-4/US-9）或从目录导入（US-5）。 | chat 模型选择器（Composer 下拉、ModelPicker 全屏）、设置页 Models 面板的每个 provider 详情 |
| **curated（精选）** | 已配置模型的子集：写了非空 `description` 的别名。`description` 是 LLM 感知面的唯一可见性门控（D-M10.1）——未写的模型对人类界面可见、对 LLM 整体隐藏。操作入口 = 设置页「LLM 可见」开关（US-9）。 | Agent / AgentSwarm 工具描述的 "Available models" 清单、错误反馈内联清单 |
| **已配置 provider** | config.toml `[providers.<id>]` 中登记的供应商（mirri 官方 OAuth provider 亦在此）。 | 设置页 Models 面板的 provider 列表 |
| **目录数据** | 从 models.dev 拉取并缓存的公开模型目录（全部 provider 及其模型，含人类可读名、上下文长度、能力标注）。**目录数据不是可用模型**——一个目录模型只有被添加为别名（导入/手动添加）后才可用。 | 三个用途：(a) 已配置别名缺 `displayName` 时的名称兜底（D-M1）；(b) "Add provider" 对话框的浏览与导入来源（US-5）；(c) 刷新后保持以上两者最新（US-3） |

模型名称展示遵循 **D-M1 解析链**：别名显式 `displayName`（用户写的）→ 目录名（别名未写时，从目录数据查）→ 原始模型名。

### 2.1 标识符规范（四个名字，禁止自造同义词）

上文三个集合描述"哪里能看到什么"；本表定义"用什么名字指称它"。spec 中出现任何含 "id" 的模型相关表述，必须能唯一归位到下表四者之一，否则视为缺陷。四个术语的**跨域唯一定义**收录在 [glossary.md](glossary.md)（模型概念横跨 config / 引擎 / RPC / web / agent 工具多个域）；本表是它在本文的域内视图，两者冲突时以 glossary 为准：

| 规范名 | 形态与示例 | 它是什么 / 用在哪 | config.toml 对应位置 | 禁止的叫法（曾引发歧义） |
|---|---|---|---|---|
| **模型别名**（短称"别名"） | `[models.<键>]` 的键，**规范形态** `provider-id/model-id`（如 `github-copilot/claude-opus-4.8`）；亦容忍**短名形态**（如 `sonnet`，此时斜杠规则不适用） | 全系统指称"哪个模型"的唯一方式：Agent 工具 `model` 参数的唯一合法值、`default_model` 与会话当前模型的取值。**别名本身就是标识符，没有另外一个"id"**；本域所有写路径（手动添加、目录导入）生成的键一律用规范形态 | `[models."github-copilot/claude-opus-4.8"]` 的键（表名后面的部分） | ~~别名 id~~、~~modelId~~、~~模型 id~~ |
| **原始模型名** | 规范形态别名的斜杠右侧部分，如 `claude-opus-4.8`；短名形态别名没有斜杠，原始模型名只体现在表内 `model` 字段 | 调用 provider 时实际发送的模型名；`model.list` 行的 `model` 字段、表单中名为 "model id" 的输入框，取值都是它 | 同一张表内的 `model = "claude-opus-4.8"` 字段的值 | ~~原始 id~~、~~raw model id~~、~~原始 model id~~ |
| **provider id** | 规范形态别名斜杠左侧部分，如 `github-copilot` | `[providers.<id>]` 的键、provider 列表的条目标识；短名形态别名的 provider 由表内 `provider` 字段指明 | `[providers.github-copilot]` 的键 | —— |
| **显示名**（display name） | 任意人类可读文本，如 "Claude Opus 4.8" | D-M1 链的产物，仅供阅读。**不是标识符，不能作为 `model` 参数的值** | 同一张表内的 `display_name = "Claude Opus 4.8"` 字段的值 | —— |

四个标识符在 config.toml 中的位置（一段带注释的完整示例；`#` 注释指明对应规范名）：

```toml
# 模型别名（键）= "github-copilot/claude-opus-4.8"  ← 规范形态
# provider id = "github-copilot"                     ← 别名斜杠左侧 = 下方 [providers.<id>] 的键
[models."github-copilot/claude-opus-4.8"]
provider = "github-copilot"     # provider id（表内指明，短名形态别名的唯一 provider 来源）
model = "claude-opus-4.8"       # 原始模型名（发给 provider 的实际值）
display_name = "Claude Opus 4.8" # 显示名（D-M1 链最高优先；缺省时走目录名 → 原始模型名）

[providers.github-copilot]      # provider id = 表键 "github-copilot"
type = "openai"
api_key = "YOUR_API_KEY"

default_model = "github-copilot/claude-opus-4.8"  # 取值 = 模型别名（config 键）
```

**同名异义警示（三处）**：

1. wire 与代码中 "model" 一词有两种含义——`model.list` 行的 `model` 字段是**原始模型名**；Agent 工具与 `session.updateMeta` 的 `model` 参数是**模型别名**。两个 "model" 相邻出现（如"别名的 model 字段"）时，须按本表先归位再理解。
2. wire 行携带 config 键（`id` 字段，架构 ADR-003 推翻原 T3 裁决——组装 id 曾顺着写链路回传，短名别名下引擎解析不到，静默落到错误 provider），web 以该**模型别名**为应用层 id，一切回写（设默认模型、切换会话模型）以别名定位；无该字段的旧引擎行退回 `${provider}/${model}` 组装（仅展示，不回写）。
3. 英文 "catalog" 一词两义——引擎内 `ModelCatalog` = **已配置模型注册表**（config 别名装配，运行时解析与感知面的数据源，图 B 的 CATALOG 节点即它）；`providerDiscovery.listCatalog` / `importCatalog` 及 REST `/catalog/*` 中的 catalog = **models.dev 目录数据**。类名层与方法名层指向不同实体，唯一定义见 [glossary.md](glossary.md) 的 catalog 警示。

## 3. 范围与优先级

**In scope**：设置页 Models 面板全部能力、chat 模型选择器、Agent 工具的模型感知面契约（D-M10，数据面归本域、实现落点在 agent 工具域）、支撑上述能力的 RPC 契约语义。
**Out of scope**：OAuth 登录流（另立故事）、TS server 行为变更、provider 重命名迁移（D-M7）、agent profile 域（用户另行处理）、目录 UI 增强（搜索/分页）。

优先级：US-1/3/11/12 为 P0（用户当前被阻断的路径）；US-2/4/5/6/8/9/14 为 P1；US-7/10/13 为 P1~P2。

## 4. 契约级裁决（本次评审确定）

### D-M1 Display name 解析链（全 UI 统一）

任何展示模型名的界面（选择器行、搜索匹配、当前模型指示、设置页）按同一条链解析：**用户显式覆盖（config 别名 `displayName`）→ models.dev 目录名 → 原始模型名**。解析在数据面完成（`model.list` 的每一行都携带解析后的 `display_name`），展示层不做第二套逻辑。

评审注记：用户 config.toml 的别名大多已写 `display_name`，后端也已正确投递——当前失真是 web 适配器的 wire 字段命名 bug（见 context 文档 §1），属实现修复，不改变本链条定义。目录名兜底仅作用于未写 `displayName` 的别名。

### D-M2 刷新语义 = 目录拉取 + 读时合并，不改变已配置模型集合

刷新动作的语义：拉取 models.dev 目录 → 更新内存目录 + 磁盘缓存。**刷新不增删任何已配置模型（别名集合不变）**；目录数据只在读取时用于名称兜底（D-M1）与目录浏览（US-5）。目录不可达是降级事件（沿用缓存 + 诊断警告），不是调用错误。

### D-M3 展示不因刷新失败置灰

选择器打开时使用既有列表立即渲染，刷新在后台进行；刷新失败只产生提示，不把选择器置为不可用状态。

### D-M4 目录缓存懒加载 + 本地持久化开关

服务器启动**不因目录读取阻塞**；允许启动后台磁盘预热（零网络、零阻塞）。首次目录数据使用时（`model.list` 的名称兜底、目录浏览），若内存目录为空且磁盘缓存存在，则从本地缓存文件惰性恢复（本地磁盘读，快）；**任何网络拉取都只在显式刷新或后台触发时发生，永不阻塞请求路径**。

磁盘缓存的本地持久化可通过 config.toml 开关禁用（默认启用；禁用后刷新不写缓存、启动不读缓存，目录数据退化为"本次进程内刷新过才有"）。

### D-M5 目录浏览/导入本批交付

"从 models.dev 目录添加供应商"（浏览 + 一键导入）纳入本批范围（两个新后端方法，见 §6 契约面）。

### D-M6 供应商状态保持两态

`model.listProviders` 的 `status` 仅由 apiKey 有无推导（connected / unconfigured），不反映 OAuth 托管 provider 的登录态（对齐 TS 的 connected-含-OAuth 语义列为 followup）。

### D-M7 不做 provider 重命名

改 provider id 为低频操作，删除后重建可替代；`new_id` 迁移语义（别名键重写、默认模型指针改写）不做。

### D-M8 删除被活跃会话使用的别名 = 静默保持

删除别名不主动迁移正在使用它的会话；会话按已加载配置继续，后续请求遇到模型不存在时报可理解错误。

### D-M9 选择器与设置页的操作对象 = 已配置集合

chat 模型选择器与设置页 Models 面板展示、搜索、操作的对象**只限已配置模型与已配置 provider**。目录数据不出现在选择器中（一个未配置 provider 的目录模型选了也调不通：无凭据、无别名）；想使用新模型必须先通过 US-4/US-5/US-9 显式添加。曾考虑替代方案"选择器额外显示已配置 provider 的目录新模型、选中时隐式创建别名"（TS 刷新写回后的等效体验），因与 D-M2（不写回用户配置）冲突且引入隐式配置写入而否决。

### D-M10 Agent 模型感知面（LLM 决策契约）

main agent 通过 Agent / AgentSwarm 工具为 sub agent 指定模型时，LLM 依据的感知面契约：

1. **集合口径**：感知面清单 = 已配置模型中写了 `description` 的子集（curated，集合定义见 §2）；`description` 是唯一可见性门控，未写的模型不出现在任何 LLM 可见面。目录数据不出现在感知面。
2. **信息密度与标识口径**：每个条目必须携带——可指定的**模型别名**（`model` 参数的唯一合法值，即 config 键，规范形态见 §2.1）、描述（即门控字段，清单内条目必有）、能力标注（已配置时）。LLM 不需要额外调用任何接口就能据此决策。**感知面不携带显示名**：显示名是 UI 概念，D-M1 链只服务人类界面；LLM 面以模型别名为唯一标识——别名即 config 键，对 LLM 与人类同样可读，可读性诉求通过别名命名解决，不通过向感知面引入显示名解决（引入第二标识会破坏 `model` 参数取值的唯一性）。
3. **单一事实源**：感知面与设置页、chat 选择器不出现两套清单或两套标识（同一别名集合、同一模型别名）；显示名链（D-M1）只存在于人类 UI 面。
4. **错误反馈闭环**：指定未配置的别名时，错误信息必须**内联 curated 清单（含描述）**，LLM 无需额外调用即可自纠。
5. **安全边界**：凭据类信息（apiKey 等）永不出现在感知面。

实现落点在 agent 工具域（工具描述构建与参数校验）；本域负责数据面（别名集合与字段完整性）与契约定义。

## 5. User Stories

### US-1 查看已配置模型（P0）

作为用户，我打开 chat 模型选择器（或设置 → Models 页某个 provider 的详情）时，看到的模型清单**恰好等于已配置模型中属于该范围的那部分**：选择器显示全部已配置模型（按 provider 分组）；provider 详情显示该 provider 的已配置模型。目录里我未配置的模型不出现在这两个界面的任何位置。

- 名称展示遵循 D-M1 链。
- 验收：
  - Given 已配置 provider P，其别名共 3 条，When 打开选择器，Then P 分组下恰好出现这 3 个模型，不多不少；
  - Given 目录中存在未配置 provider Q 的 50 个模型，When 打开选择器，Then Q 的模型一条也不出现；
  - Given 某别名无显式 `displayName` 且目录数据含该模型，Then 显示目录名而非原始模型名；
  - Given 某别名有显式 `displayName`，Then 显式值优先于目录名；
  - Given 目录不可达且无缓存，Then 无 `displayName` 的别名显示原始模型名，不报错。

### US-2 查看已配置 provider 列表与状态（P1）

作为用户，我打开设置 → Models 页，看到的 provider 列表**恰好等于已配置 provider**。每个条目显示：id、类型、连接状态、是否有 API key、其名下已配置模型的数量、默认模型标记。目录里的其他 provider 不出现在列表中（想用它们走 US-5 添加）。

- 状态遵循 D-M6（两态）。
- 验收：
  - Given 配置了带 apiKey 的 provider P，When 打开面板，Then P 显示 connected、模型数 = P 的别名数；
  - Given 无 apiKey 的 provider，Then 状态为 unconfigured；
  - Given 目录中存在未配置的 provider Q，When 打开面板，Then 列表中没有 Q。

### US-3 刷新目录数据（P0）

作为用户，我可以 (a) 在设置页刷新目录数据（全部），(b) 针对单个已配置 provider 刷新，(c) 打开 chat 模型选择器时自动触发一次后台全量刷新。刷新的作用对象是**目录数据**，不是已配置模型集合（D-M2）：刷新后，(a) 缺 `displayName` 的别名名称兜底生效或更新，(b) "Add provider" 对话框中的目录列表更新，(c) 针对已配置 provider 的刷新会汇报该 provider 在目录中的模型增删（提示"有 N 个新模型可添加"）。刷新结果汇报 changed / unchanged / failed。

- 交互遵循 D-M3。
- 验收：
  - Given 已配置 provider P 在目录中有 3 个新模型，When 刷新，Then 返回结果中 P 为 changed（added=3）并可提示可添加；**P 的已配置模型数量与内容不变**；
  - Given 某别名无显式 `displayName` 且目录含该模型，When 刷新成功后重新读取模型清单，Then 该别名显示目录名；
  - Given 指定单个 provider 刷新，Then 只影响该 provider 相关的目录数据汇报；
  - Given 目录不可达，Then 调用不报错，UI 收到降级说明，名称兜底沿用既有缓存；
  - Given 服务器重启后磁盘缓存存在，When 首次目录数据使用发生，Then 目录已从缓存恢复（D-M4），期间无网络等待、无请求阻塞。

### US-4 手动添加供应商（P1）

作为用户，我可以在设置页手动添加 provider：填 id、类型、API key、可选 baseUrl，**初始模型可留空**（原始模型名、maxContextSize、可选 displayName 在添加模型时填写）。保存后该 provider 立即进入已配置集合——出现在 provider 列表中；若提交了初始模型，它们同时出现在 chat 选择器中。创建为空的 provider 可在同一页面继续添加模型（US-9），新模型随后出现在选择器中。

- 验收：
  - Given 提交合法表单（含初始模型），When 保存，Then provider 列表含新条目（connected），选择器出现其初始模型；
  - Given 提交合法表单（不含模型），When 保存，Then provider 以空模型集合创建并出现在 provider 列表，选择器不出现其模型，保存后自动引导进入添加模型流程；
  - Given id 与现有 provider 重复，Then 报已存在错误，表单不关闭；
  - Given API key 缺省，Then provider 以 unconfigured 状态创建（先建后配 key）。

### US-5 从目录导入供应商（P1）

作为用户，我可以在"Add provider"对话框浏览**目录数据**（全部目录 provider 及各自模型、env key 提示），选择一个并填 API key（+ 需要时的 baseUrl）后一键导入。导入 = 把该 provider 写入已配置 provider 集合，并把其目录模型写入已配置模型集合——模型自动携带目录标注的能力（如视觉输入、工具调用、推理）与输出上限、推理档位等目录事实，**无需我逐模型手动补能力**（显示名按 D-M1 链解析，不强制持久化 displayName）。对已配置的 provider 重复导入不报错：既有模型配置保持不变（我做过的人工修改不被覆盖），目录新增的模型被追加，我曾删除的模型不会被复活；全新环境导入后第一个模型自动成为默认。

- 交付遵循 D-M5；浏览的合法性见 §2（目录数据只在对话框中出现）；能力推导与 merge 语义见 SDR-2026-09-18。
- 验收：
  - Given 目录可达，When 打开对话框，Then 列出目录 provider（含名称 / 是否需要 baseUrl / 环境变量名提示）；
  - Given 选择目录 provider 并填 key，When 导入，Then 已配置集合出现该 provider 及其全部目录模型（带目录名展示），选择器可见，结果提示导入数量；
  - Given 导入的目录模型声明了视觉输入与工具调用能力，When 导入后打开该模型的编辑表单，Then 能力标注已自动勾选，无需手动补填；
  - Given 对已配置的 provider 再次导入，When 目录新增了 2 个模型，Then 导入成功、新增 2 条模型、既有模型配置逐项不变、此前删除过的模型不复活；
  - Given 目录不可达，Then 对话框退化为手动添加路径（US-4）并提示原因，不阻塞手动添加。

### US-6 编辑供应商凭据 / 端点（P1）

作为用户，我可以修改**已配置 provider** 的 API key 与 baseUrl，保存后新值即时生效（无需重启）；编辑表单能回显已存 key。

- 验收：
  - Given 提交新 key，When 保存，Then 后续请求使用新 key；
  - Given key 提交空串，Then 清除凭据（provider 变 unconfigured）；
  - Given OAuth 托管 provider，Then 拒绝手动改 key（明确的域错误码）。

### US-7 删除供应商（P1）

作为用户，我可以删除一个已配置 provider：其名下别名一并从已配置模型集合移除；默认模型指针若指向被删范围则同步清理。删除后该 provider 及其模型从设置页与选择器消失。

- 与 TS 基线的差异注记：TS 的删除端点级联删别名但**故意不动全局默认模型指针**（视为用户设置不代为回收）；本域目标态选择"指针同步清理"，由 rust `config.removeProvider` 的现有语义承载（见 context 文档 §3、架构 C6），无需实现差异。
- 验收：
  - Given provider P 有 3 条别名，When 删除 P，Then 3 条别名全部消失，provider 列表与选择器不再含 P 及其模型；
  - Given 默认模型属于 P，When 删除 P，Then 默认模型指针不悬空；
  - Given OAuth 托管 provider，When 删除，Then 明确域错误拒绝（与 US-6 的 OAuth 拒改 key 同口径，对齐 TS 契约）。

### US-8 设置默认模型（P1）

作为用户，我可以把某个**已配置模型**设为默认，新会话默认使用它。

- 验收：
  - Given 设置别名 X 为默认，When 新建会话，Then 会话模型为 X；
  - Given X 不在已配置模型集合中，Then 报 model_not_found，默认不变。

### US-9 添加 / 编辑模型别名（P1）

作为用户，我可以为已配置 provider 手动添加模型别名（原始模型名、displayName、maxContextSize、capabilities、supportEfforts/defaultEffort、description（用途说明，高级选项区）），并编辑已有别名的这些字段（表单自动保存）。任何添加/修改立即反映到已配置模型集合（选择器可见）。`description` 同时是 LLM 感知面的可见性开关（D-M10.1）：表单提供「LLM 可见」开关控制它。

- 验收：
  - Given 编辑某别名的 `displayName` 为 "Foo Pro"，When 保存后打开选择器，Then 该模型显示 "Foo Pro"（D-M1 链中显式值最高优先）；
  - Given 为 provider P 添加别名 M，When 保存，Then 选择器 P 分组出现 M；
  - Given maxContextSize 非法（非正整数），Then 表单校验拒绝；
  - Given 打开某别名的「LLM 可见」开关并填写用途说明，When 保存，Then 该模型出现在 Agent 工具的 "Available models" 清单（进入 curated 集合）；
  - Given 关闭某别名的「LLM 可见」开关，When 保存，Then 其 description 清空、该模型从 LLM 清单消失（人类界面——选择器 / 设置页——不受影响）。

### US-10 删除模型别名（P1）

作为用户，我可以删除一个已配置模型别名；若它是默认模型，指针同步清理；被活跃会话使用时遵循 D-M8。删除后该模型从选择器消失。

- 验收：
  - Given 删除默认模型别名 X，Then 默认模型指针清空、`model.list` 不再含 X、选择器不再显示 X。

### US-11 Chat 模型选择器（P0）

作为用户，我在 chat 界面打开模型选择器（Composer 下拉或全屏 ModelPicker），看到的是**全部已配置模型**（US-1 口径，按 provider 分组），支持搜索（按 D-M1 解析后的名称匹配，大小写不敏感）。选择一个模型后，**当前会话的模型真实切换**（后端状态可查证，不只是乐观 UI），后续消息使用新模型；选择器打开时后台刷新目录数据（D-M3：不阻塞、失败不置灰）。

- 验收：
  - Given 已配置模型若干，When 打开选择器，Then 列表立即可见（来自已配置模型集合），后台刷新进行中；
  - Given 在会话 S 中选择模型 Y，When 切换完成，Then 从会话状态接口回读的当前模型为 Y（不是切换前的旧值），S 的后续消息实际使用 Y；
  - Given 切换失败（后端错误），Then UI 回滚到切换前的模型并提示；
  - Given 搜索 "pro"，Then 匹配名称解析结果含 "pro" 的已配置模型，不匹配未配置的目录模型。

### US-12 当前模型指示（P0）

作为用户，chat 输入框的当前模型指示显示 D-M1 链解析后的名称，悬停可见完整模型别名（`provider-id/model-id`，其斜杠右侧即原始模型名）。

- 验收：
  - Given 当前模型无显式 `displayName`，When 指示渲染，Then 显示目录名（有目录数据时）或原始模型名，不显示内部拼接串。

### US-13 目录数据生命周期（P2）

服务器重启后目录数据（用于名称兜底与目录浏览）无需手动刷新即可用；缓存的本地持久化可被 config 开关禁用。

- 遵循 D-M4；更新路径 = 手动刷新 + 选择器打开触发的后台刷新（D-M3），无启动网络刷新、无定时器（允许启动后台磁盘预热：零网络、零阻塞）。
- 验收：
  - Given 上次刷新曾成功（缓存文件存在），When 重启后首次目录数据使用，Then 目录已从缓存恢复（经后台预热或惰性读取），期间无网络等待、无请求阻塞；
  - Given config 禁用本地持久化，When 刷新成功，Then 不写缓存文件，重启后目录数据为空（名称兜底退化为原始模型名）；
  - Given config 禁用本地持久化，Then 磁盘上已有的缓存文件不被读取。

### US-14 LLM 模型决策感知面（P1）

作为 main agent（LLM），我在决定为 sub agent 指定哪个模型时，能在 Agent 工具描述中看到每个对 LLM 可见模型的别名、描述与能力标注（感知面以别名为唯一标识，不出现显示名），据此做出有依据的选择；指定错误时能从错误反馈中直接自纠。

- 契约遵循 D-M10；实现落点在 agent 工具域（工具描述构建与参数校验），本域交付数据面与契约。
- 验收：
  - Given 已配置 3 个别名（其中 2 个写了 `description`），When 构建 Agent / AgentSwarm 工具描述，Then "Available models" 清单恰好含 2 条（curated 门控，D-M10.1）：每条以模型别名（规范形态，见 §2.1）为唯一标识并携带其描述，任何条目不携带显示名；
  - Given 未写 `description` 的别名，Then 它不出现在感知面清单（对人类界面无影响）；
  - Given 别名 A 的 `description` 为"适合快速检索"，When LLM 读取清单，Then A 的条目携带该描述（无需任何额外调用即可据此决策）；
  - Given LLM 指定了未配置的别名 Z，When 工具返回错误，Then 错误信息内联 curated 别名清单（含描述），LLM 可直接自纠；
  - Given 目录中存在未配置 provider 的模型，Then 感知面清单不含它们（与 chat 选择器同口径）；
  - Given 无任何已配置模型或全部未写 `description`，Then 省略整个清单节，`model` 参数说明保持可读。

## 6. 契约面要求（数据面必须提供的方法语义）

| 能力 | RPC 方法（命名对齐现有注册表） | 语义 |
|---|---|---|
| 模型目录 | `model.list` | **恰好返回已配置模型集合**（别名全集），每行带 D-M1 解析后的 `display_name` 与 `id`（= `[models.<id>]` 配置键，模型别名，ADR-003）；行内 `provider`/`model` 字段对应 config 模型条目的同名字段；wire 形状遵循 klient snake_case 契约（`id` 为加法字段，klient strip 兼容）；**不**返回未配置 provider 的目录模型 |
| 供应商列表 | `model.listProviders` | 恰好返回已配置 provider 集合（`ProviderCatalogItem` snake_case），状态遵循 D-M6 |
| 刷新 | `providerDiscovery.refreshProviderModels` | D-M2 语义（只更新目录数据），支持 `{scope:'all'}` 与 `{providerId}`；磁盘缓存持久化受 D-M4 开关控制 |
| 供应商 CRUD | `provider.set` / `provider.delete` / `provider.get` / `provider.list` / `config.removeProvider` | US-4/6/7；**删除走 `config.removeProvider`**（级联别名与默认指针——`provider.delete` 刻意不级联，见架构 C6）；OAuth 托管 provider 拒删（US-7） |
| 模型 CRUD | `model.set` / `model.delete` / `model.setDefaultModel` | US-8/9/10；删除遵循 D-M8 |
| 目录浏览 | **新增**（如 `providerDiscovery.listCatalog`；方法名中的 catalog 指 models.dev 目录数据，见 §2.1 警示 3） | US-5：返回目录数据（provider 条目：name / needs_base_url / env_key / models），与已配置集合无关 |
| 目录导入 | **新增**（如 `providerDiscovery.importCatalog`；catalog 同上） | US-5 组合写 provider + 全量别名（遵循 D-M1/D-M2：别名字段最小化，名称靠解析链），返回导入数量 |
| 会话模型切换 | `session.updateMeta`（或等效方法） | US-11：`model`（及 `thinking`）字段必须真实生效（校验模型在已配置集合内），不再是静默 no-op |
| 配置开关 | config.toml `[model_catalog]` 域 | D-M4 的本地持久化开关（默认启用） |

## 7. 评审记录

### 2026-09-08 首轮（Q1~Q7）

| 问题 | 裁决 |
|---|---|
| Q1 display name 生效方式 | 修 web 适配器 wire 字段名 bug（P0）+ 目录名读时合并兜底（D-M1/D-M2）；用户 config 已写 display_name，修复即恢复 |
| Q2 目录浏览/导入 | 本批交付（D-M5，US-5 P1） |
| Q3 agent profile | 移出本域，用户另行处理 |
| Q4 供应商状态 | 保持两态（D-M6），OAuth 态 followup |
| Q5 provider 重命名 | 不做（D-M7） |
| Q6 删除被活跃会话使用的别名 | 静默保持（D-M8） |
| Q7 目录自动刷新 | 懒加载 + 本地缓存持久化 + config 开关（D-M4/US-13）；无启动网络刷新、无定时器 |

### 2026-09-08 表述澄清轮

| 澄清点 | 结论 |
|---|---|
| "所有可用模型"歧义 | 定义三个集合（§2）：已配置模型 / 已配置 provider / 目录数据；全部 User Story 只引用集合口径（D-M9） |
| 选择器是否含未配置 provider 的目录模型 | 否（D-M9）：目录数据不是可用模型，只用于名称兜底与添加/导入来源 |
| 刷新的可见效果 | 只作用于目录数据（D-M2），不增删已配置模型；新模型的进入途径 = 显式添加/导入 |
| 会话模型切换 | 补充根因（`session.updateMeta` 忽略 `model` 字段，切换为静默 no-op），US-11 验收要求后端状态可查证的真实切换 |
| LLM 模型感知面 | 新增 D-M10/US-14：感知面清单 = 已配置集合 + 模型别名/显示名/描述/能力，错误反馈内联合法清单；rust 现状为裸别名清单（见 context 文档 §1.2），属实现缺口非目标差异 |
| 功能闭环总览 | 新增 §0：闭环示意图 + 闭环点对照表（图上每条边对应需求条目），明确设置页与 agent 运行时的因果闭环，防"配置不生效"类断线 |

### 2026-09-08 标识符规范轮

| 澄清点 | 结论 |
|---|---|
| "别名 id"表述歧义 | 存在两类风险：(a) 读者把"别名 id"误解为别名之外还有一个 id（实际别名本身就是标识符）；(b) 与"原始 model id"（原始模型名）混淆，二者形态不同（`provider-id/model-id` vs `model-id`）、取值域不同（`model` 参数 vs 发给 provider 的值）。裁决：新增 §2.1 标识符规范表，全文统一为四个规范名（模型别名 / 原始模型名 / provider id / 显示名），禁用"别名 id""原始 id""modelId"等自造同义词；wire 上 `model` 字段的双重含义（`model.list` 行字段 = 原始模型名，Agent 工具 `model` 参数 = 模型别名）列入同名异义警示 |
| 防复发机制 | `docs/specs/AGENTS.md` 修改设计文档检查清单新增第 5 问（标识符唯一可归位、同形异义显式标注）；需求文档中任何含 "id" 的模型表述必须能归位到 §2.1 四者之一，否则视为缺陷 |
| 标识符与 config.toml 的对应 | §2.1 增加 "config.toml 对应位置" 列与带注释的完整 toml 示例；核实依据：`[models.<键>]` 键 = 模型别名（`default_model` 取值即键），表内 `model` 字段 = 原始模型名，`display_name` 字段 = 显示名，`[providers.<id>]` 键 = provider id |
| 别名的两种键形态 | 键的**规范形态**为 `provider-id/model-id`（本域所有写路径按此生成）；**短名形态**（如 `[models.sonnet]`，provider 由表内字段指明）为合法的历史容忍形态。新增警示：wire 行不含键，web 组装 id 仅在规范形态下等于键，回写 config 的操作必须以键定位（架构文档 C6 约束） |
| 跨域术语文档 | 新建 [glossary.md](glossary.md)：横跨多个域的术语（模型四术语为其首批条目）唯一定义于该文件，域内 spec 引用而不重复定义（治理见 `docs/specs/AGENTS.md` 术语文档类型） |

### 2026-09-08 感知面修订轮

| 裁决点 | 结论 |
|---|---|
| 感知面条目是否携带显示名 | 否（D-M10.2/3、US-14 修订，架构 §5 同步）：感知面以模型别名为唯一标识——别名即 config 键，对 LLM 与人类同样可读；可读性诉求通过别名命名解决，不通过向感知面引入显示名解决（引入第二标识会破坏 `model` 参数取值唯一性） |
| 感知面清单口径 | curated 门控（D-M10.1、§2 补 curated 视图、US-14 验收修订）：清单 = 已配置模型中写了 `description` 的子集；`description` 是唯一可见性门控，未写的模型对 LLM 整体隐藏（避免全量清单压迫上下文窗口）；错误反馈内联 curated 清单 |
| description 的编辑入口 | US-9 补 description 字段与「LLM 可见」开关验收（现状 web 已有：ModelEditForm / ProviderDetail，见 context §1.2）——开关即 curated 门控的用户操作面 |
| 「catalog」一词两义 | §2.1 新增警示 3 + glossary 补 catalog 警示 + 全文统一（图 B 节点、架构 §5/C4、context §1.2）：`ModelCatalog` = 已配置模型注册表；方法名中的 catalog = models.dev 目录数据 |
| provider 删除的级联语义 | 已核事实落文档（US-7/架构 C6/context §3）：`provider.delete` 刻意不级联，级联在 `config.removeProvider`（含默认模型指针清理）；web 接线后者；补 OAuth 拒删验收；与 TS 基线的指针差异为目标态选择（US-7 注记） |
| addProvider 组合方式 | 定型（架构 C6）：两次 `config.set` 分节写、别名键规范形态、全新环境种子默认（对齐 TS `createProvider`），替换原"取实现简单者" |
| 目录预载措辞 | D-M4/US-13 改为「启动不因目录读取阻塞；允许启动后台磁盘预热」，消除与架构 C1 的字面冲突 |
| 闭环点对照表 | 补 6 行（图 A ⑧ 边 + 图 B 五条边），图 A 补 ⑧ 边标签，兑现"每条边都对应需求条目"的声明 |
| 供应商状态 | 复核后确认无需改裁决：TS 实现同样只产生两态（`'error'` 枚举值从不赋值），说明补入架构 C6 |

### 2026-09-12 模型传递链路修订（SDR）

| 裁决点 | 结论 |
|---|---|
| 选择链路的标识 | **模型别名（config 键）是模型信息传递链路的唯一标识**（用户裁决）：web 的一切回写（切换会话模型、设默认模型、删除）以别名定位；引擎解析失败必须明确报"未配置"，不得静默落到内部默认通道 |
| `model.list` 行携带 `id` | SDR 落定（架构 ADR-003 推翻 T3）：行新增 `id` = `[models.<id>]` 键。原 T3 的"web 组装零成本"判断被实测推翻——组装 id 顺着写链路回传，短名别名（如 `[models.fast]` 命名 rakuten DeepSeek）下引擎解析不到，静默落到错误 provider 的环境变量死胡同，用户明确选择的模型被擅自替换；同一根因还造成默认模型徽标显示裸别名（display_name 丢失）与 Composer 面板空白（当前模型解析失败） |
| 收藏迁移 | web 在 `loadModels` 时把按旧组装 key 收藏的条目一次性迁移到别名 key（同一 `provider/model` 组合匹配），未匹配项保留原样 |
| 引擎防御 | 引擎保留 `provider/model` 组合 id 的防御性解析（`resolve_model_alias_in`）与未配置明确报错，兼容存量客户端与存量会话回读 |

### 2026-09-18 目录导入能力字段与 merge 语义（SDR）

| 裁决点 | 结论 |
|---|---|
| 导入模型的能力标注从何而来 | **随导入自动写入**（目录事实推导：能力标注、输出上限、推理档位、推理字段名）：用户从目录导入 provider 后，模型无需手动编辑即具备正确能力。背景（现状证据见 context 文档 §6）：rust 实测导入的模型能力全空，运行时只剩名字猜测，用户被迫逐模型手勾能力标签；TS 基线（契约基准）的导入即如此写入。此为用户显式动作写 config，与 D-M2（刷新永不写别名）不冲突 |
| 能力呈现的诚实性 | 浏览目录时，目录模型行如实携带其能力标注与推理标注（只读目录事实，非已配置集合承诺）——目录说支持什么就展示什么，导入后即按此写入 |
| 重复导入已配置 provider | **merge**：既有模型配置逐项保持不变（用户的人工修改不被目录覆盖），目录新模型追加，被用户删除过的模型不复活。"已存在即报错"保留给手动添加场景（US-4）——原架构引用的 TS 错误面属于手动建 provider，导入面实际为 merge（见架构 ADR-004 勘误） |
| 导入后默认模型 | 全新环境（尚未设置默认模型）导入时，以第一个未被用户删除的模型作为默认；已有默认不移动 |
| 暂不引入的字段 | 目录的输入上限、推理关闭档位、不可关闭的推理标注暂不随导入写入（rust 配置模型暂无对应落点，rust 引擎亦不消费——记为开放项，出现真实消费方再引入） |

### 2026-10-13 手动创建允许空模型集合（SDR）

| 裁决点 | 结论 |
|---|---|
| 手动创建是否要求初始模型 | **不再要求**：手动创建 provider 时模型列表可为空，provider 以空模型集合创建。原先"至少一个初始模型"的约束取消——创建表单不再内联模型编辑器，模型在创建后通过添加模型流程（US-9）补录。背景：零模型 provider 是合法目标态（用户先登记凭据、再逐个补模型），强制至少一个模型反而阻断该路径 |
| 空 provider 的引导 | 创建为空的 provider 后，保存动作自动引导进入添加模型流程（打开添加模型弹窗），用户在同一页面完成首个模型补录，无需自行寻找入口 |
| 零模型创建的种子语义 | 全新环境默认模型种子仅在 provider 带至少一个模型时发生（以该 provider 默认模型或首个模型为种子）；空模型创建不产生任何种子——没有模型可指，全局默认指针保持未设置，等后续添加模型后再由用户或既有种子规则落定 |
| 空模型列表的契约 | 服务端接受空 `models` 列表（创建与整体替换皆然）；替换语义不变——提交空列表即清空该 provider 名下全部别名，空创建与空替换都是合法终态 |
