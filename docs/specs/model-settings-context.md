# Model 设置页现状盘点（Context）

> 快照日期：2026-09-08（评审后更新：display name 根因修订；感知面 curated 机制与 provider 删除级联已复核）；2026-09-18 增补：models.dev → 别名能力字段推导链路侦察（§6） ·
> 本文是 provider/model 配置域的**现状参考**（代码事实、两侧差异矩阵、侦察产出），随现状变化更新，不含目标与裁决。
> 目标定义见 [model-settings-page-requirements.md](model-settings-page-requirements.md)（下称「需求文档」）；技术架构见 [model-settings-architecture.md](model-settings-architecture.md)（下称「架构文档」）；契约基准为 TS v2 server（`packages/kap-server`）。

## 1. 已报告问题的根因盘点

用户在 rust 后端（`mirri web`）上报四类问题，根因均查明：

| 现象 | 根因 |
|---|---|
| 模型显示 raw id 而非 display name | **web 适配器 wire 字段命名 bug**：rust `model.list` 行按 klient 契约发 snake_case（`display_name`、`max_context_size`、`support_efforts`、`default_effort`），web 的 `RpcModel`（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:2463`）声明 camelCase（`displayName` 等）→ JSON 解析后全为 `undefined` → UI 回退 raw id。config.toml 里的 `display_name` 后端已正确发出（`to_mirri_config_wire` 有测试覆盖，`rust/crates/mirri-core/src/config/tests.rs:1491`），纯粹是 web 侧类型声明不匹配 |
| 模型选择器不好用 | 三因叠加：(1) 同上 casing bug——rust `model.list` 行**没有 `id` 字段**（契约如此），web `RpcModel.id` 为 `undefined`，选择器 key、`modelById` 查找全失效，`maxContextSize` 也拿不到（回退 0）；(2) `openModelPicker()`（`App.vue:433`）先 `refreshAllProviders()` → rust 适配器硬抛 → 选择器整体置为 unavailable；(3) **切换会话模型是静默 no-op**：web `setModel` → `session.updateMeta {session_id, model, …}`，rust 处理器只应用 `title`/`tags`（"Unsupported keys are ignored rather than rejected"，`session_handlers.rs:914`）→ 模型从不生效，`refreshSessionStatus` 回读旧值后乐观 UI 回滚；Composer pill 还直接渲染 `status.model` 原始串，无 display name 映射 |
| provider 面板状态失真 | `RpcProvider` 同病：声明 `hasApiKey`/`baseUrl`/`defaultModel`（camelCase），rust `model.listProviders` 发 `has_api_key`/`base_url`/`default_model`（snake_case）→ `hasApiKey` 恒 `undefined` → `?? false` |
| `refreshAllProviders` 报 "not supported on the rust backend" | web 的 rust 适配器 `ws-rpc-api.ts:1796` 硬抛；rust 后端已有 `providerDiscovery.refreshProviderModels`（models.dev 拉取 + 磁盘缓存 + 合并，支持 `scope:'all'` 与 `providerId`），web 未接线 |
| Chat 模型选择器整体不可用 | `openModelPicker()`（`apps/mirri-web/src/App.vue:433`）每次打开先 `refreshAllProviders()` → rust 路径抛错 → 选择器整体置为 unavailable（并入上表"模型选择器不好用"第 (2) 因，保留行以对应原始报告） |
| `agent.list` 报 "missing field sessionId" | web rust 适配器调 `agent.list`（无参数，期望 profile 列表，`ws-rpc-api.ts:1904`）；rust 的 `agent.list`（`rust/crates/mirri-server/src/rpc.rs:1067`）是会话内 live agent 枚举（必须 sessionId）。profile 列表的对应方法是 `agent.listProfiles`（REST `/api/v1/agents` 即其路由）。**用户已划走：agent profile 域由用户另行处理，不在本域范围** |

### 1.1 模型集合的现状口径（谁在哪能看到什么）

- **设置页 Models 面板**：provider 列表来自 `listProviders()`（= config 已配置 provider）；每个 provider 的模型清单来自 `getConfig().models` 中 `alias.provider === provider.id` 的条目——**只显示已配置别名**，不含目录数据。
- **chat 模型选择器**（Composer 下拉 + ModelPicker 全屏）：来源 `listModels()` = rust `model.list` = 已配置别名 + models.dev 目录 overlay；rust 现状 overlay 合并**整个目录**（含未配置 provider 的全部模型），选中未配置 provider 的模型实际无法调用（无凭据、别名不存在）——与 TS 基线不一致（TS 刷新只作用于已配置 provider 并把结果写入别名，选择器集合即别名集合，`agent-core-v2/src/app/kosongConfig/discoveryService.ts`）。
- **"Add provider" 对话框**：唯一合法浏览全量目录数据的地方（目录 provider 列表 + 各自模型 + env key 提示）。

### 1.2 Agent 工具模型感知面现状（目标契约见需求文档 D-M10/US-14）

rust `Agent` / `AgentSwarm` 工具的 `model` 参数与 "Available models" 清单注入机制已存在（`rust/crates/mirri-core/src/tools/agent_tool.rs:49` `build_agent_description`，数据源 `model_catalog.list_models()`——已配置模型注册表，非目录数据，spawn 前校验别名）；TS v2 侧对应物为 `packages/agent-core-v2/src/agent/tools/agent/agentTool.ts:166`（经 `src/session/subagent/configSection.ts:125-192` 构建：**description 门控的 curated 清单**——没写 description 的模型对 LLM 整体隐藏；条目 = 别名 + 描述 + 上下文长度 + 能力标注；错误 hint 列 curated 别名）。**信息密度缺口（rust 侧）**：rust 清单为裸别名（如 `- sonnet`），`ModelCatalogEntry` 的 `description` / `capabilities` / `max_context_size` 字段未被描述构建使用——LLM 只能凭别名猜模型特长，错误反馈也不含完整清单。**web 侧操作面已有**：模型编辑表单支持 description 编辑（`apps/mirri-web/src/components/settings/models/ModelEditForm.vue:452`，高级选项区）与「LLM 可见」开关（`:152-165`：开 = 保存 description、模型进入 Agent 工具清单；关 = 清空、对 LLM 隐藏——curated 门控的 UI 形态）；模型列表有 LLM 可见徽标（`ProviderDetail.vue:307`，文案『LLM 可见，可用于模型选择。』）。

## 2. web 端 API 面（设置页 + 选择器实际调用）

- **传输选择**（`apps/mirri-web/src/api/index.ts`）：`GET /meta` 返回 `backend === 'v3'` → 用 `WsRpcApi`（WS JSON-RPC `/api/v1/rpc`）；否则 `DaemonMirriWebApi`（TS REST）。
- **启动时序**：仅 `loadModels()`（`api.listModels()`）；`loadProviders()` 不在启动路径。TS 侧靠 `modelCatalogChanged` 事件驱动重载，rust 路径无此事件，故选择器打开时手动 `refreshAllProviders()`。
- **设置页 Models 面板**（`ModelsPanel.vue` + `ProviderDetail` / `AddProviderDialog` / `ModelEditForm`）：
  - 面板加载：`listProviders()` + `getConfig()`（模型列表按 `config.models` 中 `alias.provider === provider.id` 过滤，非 `listModels()`）；
  - 写操作：`setConfig(patch)` 路径已可用（编辑 key、手动加模型、编辑模型字段、默认模型均走 `config.set`）；
  - 抛错操作：refresh（全部/单个）、deleteProvider、deleteModel、addProvider、catalog 浏览/导入 —— rust 适配器全部硬抛。
- **展示层 display name**：`displayName ?? model` 回退（`Composer.vue:1017/1035`、`ModelPicker.vue:196`）；rust 适配器 `toAppModel` 不做回退、也不合并 config 别名里的 `displayName`。

## 3. 两侧差异矩阵（web 方法 ↔ TS v2 REST ↔ rust RPC）

| web 方法 | TS v2 REST（契约基准） | rust RPC 现状 | 差距动作 |
|---|---|---|---|
| `listModels()` | `GET /models` | `model.list` 已实现；wire snake_case（契约如此），web `RpcModel` 类型声明 camelCase | **web 修类型**（含 `id` 组装） |
| `listProviders()` | `GET /providers` | `model.listProviders` 已实现；同上 casing 问题 | **web 修类型** |
| `getConfig()/setConfig()` | `GET/POST /config` | `config.get/set` 已实现 | 无 |
| `refreshAllProviders()` | `POST /providers:refresh` | `providerDiscovery.refreshProviderModels({scope:'all'})` 已实现 | web 接线 |
| `refreshProvider(id)` | `POST /providers/{id}:refresh` | 同上（`{providerId}`）已实现 | web 接线 |
| `addProvider()` | `POST /providers` | `provider.set` 已实现（单记录；alias 批量需组合 `config.set` 分节写） | web 接线；组合 = 两次 `config.set` 分节写、别名键规范形态、全新环境种子默认（对齐 TS `createProvider`，见架构 C6） |
| `deleteProvider()` | `DELETE /providers/{id}` | `provider.delete` 已实现（**已核：刻意不级联**——仅删 provider 记录 + 清 defaultProvider 指针，`provider_handlers.rs:338-341`）；级联语义在 `config.removeProvider`（级联删别名 + 清 defaultModel 指针，`tests.rs:7881-7922`） | web 接线到 `config.removeProvider`（见架构 C6） |
| `deleteModel()` | `POST /models/{id}:delete` | `model.delete` 已实现（含默认模型指针清理） | web 接线 |
| `setDefaultModel()` | `POST /models/{id}:set_default` | `model.setDefaultModel` 已实现 | web 接线（现走 setConfig 亦可） |
| `listCatalogProviders()` | `GET /catalog/providers` | 无对应方法 | 后端新增（目录浏览） |
| `importCatalogProvider()` | `POST /providers:import_catalog` | 无对应方法 | 后端新增（provider + models 组合写） |
| `updateSession(model)` | `POST /sessions/{id}/profile` | `session.updateMeta` 已实现但**只处理 title/tags，忽略 `model`/`thinking` 字段**（模型切换静默 no-op） | **后端补语义**（会话模型切换生效） |

## 4. rust 侧 models.dev 目录生命周期现状

- 刷新方法：`providerDiscovery.refreshProviderModels`（`rust/crates/mirri-embedded/src/embedded_server/provider_handlers.rs`）——拉 `models.dev` `api.json` → 写 `<mirri-home>/models-dev.json` 缓存（best-effort）→ 全量或按 providerId 裁剪后换入内存 catalog → 返回 klient `refreshProviderModels` 结果形状（changed/unchanged/failed，按模型数增减推导）。
- 降级语义：拉取失败时保留内存目录或读磁盘缓存，失败以 `config.diagnostics` 警告呈现，不报 RPC 错误；`scope:'oauth'` 返回诚实的空成功。
- 生命周期缺口：内存 catalog 初始为 `None`，磁盘缓存仅在刷新调用的降级路径读取——**重启后 overlay 为空**，直到有人显式刷新。无启动加载、无定时器（TS 侧为 refreshOnStart 默认开 + 6 小时定时器，`packages/kap-server/src/services/modelCatalog/modelCatalogRefreshScheduler.ts`）。
- 配置开关现状：rust 侧无 `[model_catalog]` 配置节；TS 侧有（`refresh_interval_ms` / `refresh_on_start`）。

## 5. TS v2 契约要点（基准形状速览）

- envelope：`{code, msg, data, request_id}`，wire 字段 snake_case。
- `ModelCatalogItem`：`provider, model, display_name?, max_context_size, capabilities?, support_efforts?, default_effort?`。
- `ProviderCatalogItem`：`id, type, base_url?, default_model?, has_api_key, status('connected'|'error'|'unconfigured'), models[]`；详情路由额外回显 `api_key`。
- refresh 结果：`{changed:[{provider_id, provider_name, added, removed}], unchanged[], failed:[{provider, reason}]}`。
- 目录导入：`{catalog_id, id?, api_key?, base_url?}` → `{provider, models_imported}`；目录条目含 `name, wire_type, needs_base_url, env_key, models[]`。
- 配置存储：`[providers.<id>]`（type/apiKey/baseUrl/oauth）、`[models."prov/model"]`（provider/model/maxContextSize/displayName/…）、顶层 `default_model`。

## 6. models.dev → 别名能力字段的推导链路（2026-09-18 侦察）

用户报告：TS 引擎下从目录添加 provider 能自动获得正确 Capabilities，rust + web 下不行。本节盘点两侧链路事实（裁决与目标态见需求文档 SDR-2026-09-18 / 架构 ADR-004）。

### 6.1 TS 基线（目录事实如何变成别名字段）

- **v2 持久化映射** `modelsDevModelToRecord`（`packages/agent-core-v2/src/app/kosongConfig/modelsDevUpstream.ts:183-206`）：目录模型 → 别名记录，除 `provider/model/maxContextSize` 外按目录事实补写——`capabilities`（`capabilityToStrings` :116-125：`image_in`/`video_in`/`audio_in`/`thinking`/`tool_use`/`dynamically_loaded_tools`；`alwaysThinking===true` 时 `thinking` 替换为 `always_thinking`）、`maxInputSize`（`min(limit.input, context)`，modelsDev.ts:212-215）、`maxOutputSize`（`limit.output`）、`displayName`、`reasoningKey`（`interleaved.field`）、`supportEfforts`/`offEffort`（`reasoning_options` 解析，modelsDev.ts:239-270；无可关档且无 toggle → `alwaysThinking`）。
- **v2 导入语义**（`modelsDevImportService.ts:127-221`）：models **merge 而非替换**——已配置别名（含用户自定义 maxContextSize/capabilities/description）逐字节保留、`deletedModels` 墓碑跳过并保留；仅当 `default_model` 未设置时用第一个非墓碑模型播种。
- **v1 对应物**：`packages/node-sdk/src/catalog.ts` `catalogModelToAlias`（:40-50，capabilities/maxOutputSize/displayName/reasoningKey 全写）；daemon RPC `addProvider` 按 provider id 命中 catalog 时写入全部模型别名（`agent-core/src/services/modelCatalog/modelCatalogService.ts:130-188`，`catalogModelToCapabilityStrings` :369-378）；自定义注册表路径 `packages/oauth/src/custom-registry.ts`（`resolveCapabilities` :249-290：rich hint 推导，无 hint 默认 `['tool_use']` + ctx 131072；**`packages/oauth` 已于 2026-10-13 随 managed login 退役删除，自定义注册表实现现居 `packages/kosong/src/custom-registry.ts`，本条为退役前 v1 现状记录**）。
- **运行时（两版共同）**：能力解析 = **config 声明 ∪ 检测**（v1 名字前缀注册表 `provider-manager.ts:231-250`；v2 协议 trait hook `catalogService.ts:619-668`），**目录数据不参与运行时解析**——目录的作用止于导入/刷新写入 config。

### 6.2 rust + web 现状缺口

| # | 缺口 | 证据 |
|---|---|---|
| 1 | **importCatalog 只写最小别名** `{provider, model, max_context_size}`——无 capabilities / max_output_size / reasoning_key / support_efforts / default_effort。导入模型能力全空：运行时媒体能力只剩名字 marker 猜测、thinking 无 effort 档、Agent 工具 curated 清单能力标注为空。**用户报告的直接根因** | `provider_handlers.rs:374-394`；架构 C4 只裁决了「不写 displayName」，对 capabilities 无裁决记录（无意识遗漏，非决策） |
| 2 | **browse 契约裁掉能力字段**：klient `providerCatalogModelEntrySchema` 仅 `id/name/context_length`；rust `listCatalog` 照做；web rust mapper 被迫硬填 `reasoning: false`、capabilities 丢弃（`AppCatalogModel.capabilities` 恒 undefined）——与 TS REST 面（`catalogModelItemSchema` 含 `capabilities?`/`reasoning`，`rest-modelCatalog.ts:181-188`）不对称 | `packages/klient/src/contract/global/providerDiscovery.ts:37-41`；`apps/mirri-web/src/api/daemon/ws-rpc-api.ts:3711-3716` |
| 3 | **重复导入语义相反**：TS 导入 merge（§6.1）；rust 对已存在 id 直接 `provider.already_exists` 拒绝。web 对话框头注释与 alreadyAdded 徽标按 merge 假定设计——rust 后端下对已配置 provider 再导入必得错误 toast，与 UI 文案矛盾 | `provider_handlers.rs:351-359`；`AddProviderDialog.vue:5-7`；`modelsDevImportService.ts:189-210` |
| 4 | **导入不种子 default_model**：TS 在 unset 时种第一个模型；rust server 与 web 适配器都不种（仅 custom `addProvider` 有客户端种子逻辑，`ws-rpc-api.ts:2295-2300`） | `modelsDevImportService.ts:214-217` |
| 5 | **max_context_size 落 0 无校验**：目录缺 `limit.context` 时 rust 导入写 0；TS v2 手动建 schema 要求 `min(1)` | `provider_handlers.rs:387-390`；`rest-modelCatalog.ts:58-66` |
| 6 | **refresh 不落 config**（D-M2 有意裁决，非缺陷）：TS 刷新把新模型别名（含 capabilities）写入 config；rust 刷新只换内存目录。已配置 provider 上游新增模型在 rust 下不进选择器，且无任何 capabilities 自动补全通道 | `discoveryService.ts`；架构 T1/D-M2 |
| 7 | **无内置 models.dev 快照**（TS 有 `builtInModelsDev.ts` 构建期注入 + 10min 内存缓存链）：全新 rust 安装未刷新前 `listCatalog` 返回空数组，目录页空白且无「先刷新」引导。refreshOnStart/定时器为 D-M4 有意裁决不做，但**零网络的内置快照**与 D-M4 不冲突 | `builtInModelsDev.ts`；`mod.rs:1890-1900`（预热只读磁盘缓存） |
| 8 | **rust `ModelAlias` 无 `max_input_size`/`off_effort` 字段**：v2 导入的 maxInputSize/offEffort 在 rust 无落点 | `config/mod.rs:209-234` |
| 9 | **custom addProvider（web rust 路径）两次 `config.set` 非原子**（第二次失败留下无模型的 provider）；capabilities 仅 3 个硬编码预设 chips（`['image_in','thinking','tool_use']`），无目录预填 | `ws-rpc-api.ts:2276/2301`；`AddProviderDialog.vue:37-38` |
| 10 | **rust 引擎不消费 `always_thinking` 词表**（全仓无匹配）：TS v2 的 `alwaysThinking` 推导即使写入 rust 也不会被消费 | grep `always_thinking` rust/ 零命中 |

rust 侧已有能力承接面（缺口 1 的修复不需要 schema 改动）：`ModelAlias.capabilities/max_output_size/reasoning_key/support_efforts/default_effort` 全链路 round-trip（`config/mod.rs:209-226`、`wire.rs:688-740`）；`model.list` 行按 config 声明发 capabilities（`session_handlers.rs:4900-4902`）；`removed_model_ids` 墓碑机制已存在（`model.delete` 写入、`config.set` 重加别名时清除，`session_handlers.rs:4341-4375`、`wire.rs:578-630`）——merge 语义有现成地基。
