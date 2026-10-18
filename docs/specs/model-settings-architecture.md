# Model / Provider 配置域架构设计（RFC）

> 状态：v1（待评审） · 2026-09-08 ·
> 本文是 provider/model 配置域的技术架构（内部规格文档，中文单语）。
> 需求与契约裁决见 [model-settings-page-requirements.md](model-settings-page-requirements.md)（下称「需求文档」，D-M1~D-M9）；现状与代码事实见 [model-settings-context.md](model-settings-context.md)（下称「context 文档」）。
> 功能需求决定"做什么"；本文回答"怎么建"，并把非功能需求（性能、可用性、数据所有权）显式映射到结构。

## 1. Scope

provider/model 配置域的端到端技术架构，四个落点：

1. **rust 引擎目录服务**（models.dev 数据的内存/磁盘生命周期）
2. **RPC 契约面**（`model.*` / `provider.*` / `providerDiscovery.*` / `session.updateMeta` 的语义与形状）
3. **web rust 适配层**（`ws-rpc-api.ts` 的类型、映射与接线）
4. **会话模型切换路径**（选择器 → 后端真实生效）

## 2. Goals & Non-Goals

**Goals**

- G1 `model.list` / `model.listProviders` 恰好返回已配置集合（D-M9），名称解析在数据面完成（D-M1）。
- G2 目录数据生命周期：懒加载 + 本地缓存 + config 开关（D-M4）；**网络 IO 永不出现在请求路径上**。
- G3 刷新接线与两个新方法（目录浏览 / 目录导入，D-M2/D-M5）。
- G4 会话模型切换真实生效（不再是静默 no-op）。
- G5 契约兼容：klient snake_case wire、bare-array 语义不变，演进只做加法。

**Non-Goals**：OAuth 登录流、provider 重命名迁移（D-M7）、agent profile 域、TS server 行为变更、目录 UI 增强。

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
- `providerDiscovery.importCatalog({catalog_id, id?, api_key?, base_url?})` → 在**一个 config 写事务**内：写 `[providers.<id>]` + 为该 provider 的每个目录模型写别名——别名按目录事实携带**推导字段**（ADR-004）：`provider` / `model` / `maxContextSize`（`limit.context`）+ `maxOutputSize`（`limit.output`）/ `capabilities`（`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/`dynamically_loaded_tools`，按目录 `modalities.input`/`tool_call`/`reasoning`/`dynamically_loaded_tools` 推导）/ `reasoningKey`（`interleaved.field`）/ `supportEfforts`（`reasoning_options` 的 effort 档，排除关档）；不可用目录模型（无正整数 `limit.context`、`status` 为 deprecated/alpha、输出模态不含 `text`、名字带 embedding 标记）整条跳过，浏览与导入集合同源（对齐 TS `modelsDevProviderModels` 过滤）；**不写 displayName**（名称靠 C2 解析链）与 **description**（curated 门控是用户显式动作，US-9）→ 返回 `{provider, models_imported}`。
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
| 接线 | `refreshAllProviders` / `refreshProvider` → C3；`listCatalogProviders` / `importCatalogProvider` → C4；`deleteProvider` → `config.removeProvider`（级联删除别名 + 清理默认模型指针；`provider.delete` 刻意不级联，仅删 provider 记录）；`deleteModel` → `model.delete` |
| 组合 | `addProvider` = 两次 `config.set` 分节写（providers 节 + models 节，对齐 TS `createProvider`），别名键规范形态；全新环境（无任何默认模型）时种子默认模型，已有默认不动 |
| 展示 | Composer 模型 pill 按 AppModel 名称渲染（不再渲染原始 `status.model` 串） |

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

## 8. Risks & Mitigations

| # | 风险 | 缓解 |
|---|---|---|
| R1 | 缓存文件大（models.dev api.json ~10–20MB），兜底同步读阻塞首请求 ~100–300ms | 后台预热使常态命中内存；一次性成本，仅目录启用时存在；`persist_cache=false` 可完全关闭 |
| R2 | models.dev schema 漂移（字段缺失/改名） | 解析按宽松 JSON（缺字段降级为无名/0），refresh 已是此模式；R2 只损失名称兜底质量 |
| R3 | overlay 行移除影响未知 klient 消费者 | 契约测试仅断言 bare-array（集合无断言）；实施前 grep `model.list` 全仓消费者核对 |
| R4 | `importCatalog` 半途失败留下部分配置 | config 写走既有 load→mutate→persist→reload 原子管线，单事务语义；失败即整体不落盘 |
| R5 | rust config 新增 `[model_catalog]` 节与现有解析冲突 | 实施时按 mirri-core config 节约定加字段 + 测试；旧 config 无此节走默认值 |
| R6 | persist_cache=false 时旧缓存文件残留引起困惑 | 文档化语义：不读不删；用户可手删（N3：缓存是可丢弃派生数据） |

## 9. Success Criteria

1. 集合正确性：`model.list` 行数 == config 别名数（US-1 验收的数量断言）；`model.listProviders` 同理。
2. 性能：冷启动首次 `model.list`（含兜底缓存读）P95 < 500ms；预热后稳态 P95 < 10ms；刷新期间并发请求零阻塞。
3. 切换真实性：`updateMeta {model}` 后 `session.status` 回读一致；未配置模型报 `model.not_found`。
4. 降级：断网状态下全部 API 可用（名称退化为原始模型名 / 目录浏览提示需要刷新）。
5. 门禁：rust workspace 测试 + clippy + zero-io 全绿；web typecheck + 现有测试；klient 契约 e2e 不回归。

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
2. 能力词表取两侧共享集：`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/`dynamically_loaded_tools`。`maxInputSize`/`offEffort`/`always_thinking` 暂不引入（rust `ModelAlias` 无字段落点、引擎无 `always_thinking` 消费方，记 followup G-18）。
3. 导入对已存在 provider 改为 **merge 语义**（对齐 TS 基线）：既有别名逐字节保留、`removed_model_ids` 墓碑不复活、仅追加新目录模型；`already_exists` 域错误保留给手动建 provider（US-4）。
4. 导入在无默认模型时以第一个非墓碑模型种子默认（对齐 TS 导入；与 C6 的 addProvider 种子语义同口径）。
5. browse 目录模型行补发能力事实（`capabilities?`/`reasoning?`）——klient 契约加法（zod strip 兼容，旧引擎不发即缺省），web 停止硬填 `reasoning:false`。

**被拒绝的备选**：

- **读时 capabilities 目录兜底链**（仿 D-M1 display_name 链）：能力真值分裂在 config 与目录两处，且与 TS 基线（目录止于导入写入，运行时 = 声明 ∪ 检测）不一致。拒绝。
- **refresh 时补写 capabilities 进别名**：违反 D-M2/N3（T1 裁决维持）。拒绝。
- **仅 web 端推导**（客户端从 browse 行拼 capabilities 提交）：browse 行原本无能力字段，且推导散落客户端后 CLI/TUI 等其他消费者不可复用；推导属引擎数据面。拒绝。

**影响范围**：C4（本 ADR 修订）、需求文档 US-5（SDR-2026-09-18）、klient `providerDiscovery` 契约、web `ws-rpc-api.ts` 映射、ROADMAP B6/B7。
