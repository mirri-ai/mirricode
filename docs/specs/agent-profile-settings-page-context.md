# Agent Profile 设置页现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-09-08 ·
> 配套需求文档：[agent-profile-settings-page-requirements.md](agent-profile-settings-page-requirements.md)（配置面）· [agent-profile-consumption-requirements.md](agent-profile-consumption-requirements.md)（使用面，引用本文 §1.3/§2/§4）·
> 本文不描述目标、不含裁决。

## 1. TS 侧角色模型（v2 引擎为契约基准）

### 1.1 字段全集

纯数据层 `packages/agent-profile/src/schema.ts`（`AgentFileDef`，v1/v2 共享解析内核）：

| 字段 | 类型 | 缺省语义 | 备注 |
|---|---|---|---|
| `name` | string | 从文件名推导 | kebab-case 校验（`AGENT_NAME_PATTERN`，schema.ts:22） |
| `description` | string | — | 非空单行，供 UI 与 LLM 选择 |
| `whenToUse` | string? | 无 | LLM 选择该角色的时机提示；兼容 `when_to_use` |
| `defaultModel` | string? | 继承父级 | 模型别名；兼容 `default_model` |
| `tools` | string[]? | **全部工具** | 白名单；支持 `mcp__*` glob 门控 MCP 工具；字符串可逗号分隔 |
| `subagents` | string[]? | 不可派发任何子角色（默认拒绝，消费域 SDR-012） | 可派发子角色白名单（profile 名引用）；未声明与空列表等价 |
| `capabilitiesRequired` | string[]? | 无 | 能力标签（如 `code.explore`）；兼容 `capabilities_required` |
| prompt 正文 | string | 兜底通用提示 | Markdown body 即系统提示词模板 |

v2 运行时扩展（`packages/agent-core-v2/src/app/agentProfileCatalog/agentProfileCatalog.ts:71-104`）：`override?`（覆盖内置必需角色的开关）、`disallowedTools?`（黑名单）、`systemPromptTemplate?`（内置角色差异化编辑面）、`promptVars?`、`summaryPolicy?`。**无权限、职责（responsibility）、产出契约（deliverable）、角色用途类字段**——角色语义完全由 prompt 文本承载。

文件形态：`.md`（YAML frontmatter + Markdown 正文，主形态）> `.yaml` > `.yml`；`packages/agent-profile/src/frontmatter.ts` 支持解析/序列化 round-trip（页面回写的基础）。

### 1.2 发现与优先级

来源优先级（`agentProfileContribution.ts:32-39`，同名高者胜）：`builtin(0) < plugin(5) < user(10) < extra(20) < workspace(30) < explicit(40)`。

- **user**：`<brandHome>/agents`（`~/.mirri-code/agents`，随 `MIRRICODE_HOME`）；`<home>/SYSTEM.md` 作为全局主 agent prompt 覆盖 profile（见 §2）
- **workspace**：项目根（自 workDir 向上找 `.git`）`.mirri-code/agents` 优先，否则 `.agents/agents`
- **extra**：config `extraAgentDirs`
- **explicit**：运行时指定的 agent 文件；解析失败 fail-fast（显式用户意图不可静默丢弃）
- 扫描：递归深度 8，跳过隐藏目录与 `node_modules`；单文件解析失败 skip（不拖垮整轮），警告上限 5 条

合并裁决（`sessionAgentProfileCatalogService.ts:117-208`）：非内置同名高优先级赢；覆盖 builtin 同名直接替换，唯 `'agent'` 需 frontmatter `override: true`（否则 warn + 跳过）；`inspect(name)` 暴露胜出来源与被压制候选。

### 1.3 消费场景

| 场景 | 关键文件 | 行为 |
|---|---|---|
| 主 agent 启动 | `agentLifecycleService.ts`（bindBootstrap，:193） | 按默认角色 `agent` 绑定 |
| Agent 工具（子 agent 派发） | `agent-core-v2/src/agent/tools/agent/agentTool.ts:124-160` | `subagent_type` 列出可用子角色（经 `subagents` 白名单过滤）；模型解析：显式 model → `defaultModel` → 调用方模型 |
| AgentSwarm 工具 | `agentSwarmTool.ts:102-238` | 批量并行派发 |
| 子 agent 装配 | `agent/profile/profileService.ts`（`applyProfile/useProfile`，:451-472） | 渲染系统提示、`disallowedTools`、能力标签决定激活工具集 |
| Swarm 模式 | `agent/swarm/swarmService.ts` | 会话级纯状态标记 + 提醒注入，非角色属性 |
| 能力缺口提示 | `agentCapabilityGapService.ts` | 主模型缺模态时建议派发具备能力的子 agent |
| Skills | `profileService.ts:875-920` | skill 列表注入提示上下文 |

### 1.4 消费时刻现状对照（M1–M7）

按消费时刻逐项盘点现状与差距（编号供需求文档引用）：

| # | 消费时刻 | 现状 | 差距 |
|---|---|---|---|
| M1 | 会话启动 · 主 agent 装配 | 固定用内置缺省角色 `agent`，无选择入口（§2） | 选择入口缺失 |
| M2 | 运行中 · 主 agent 切换 | 不支持；引擎内 fork+binding 机制存在但无用户入口（§2） | 切换语义未定义 |
| M3 | 派发 · LLM 选择 | 工具描述动态列出可用子角色及描述 | 启停在引擎侧无消费（§6.8） |
| M4 | 派发 · 引擎装配 | 按角色装配提示词、工具面、模型 | 定义变更对运行中派发的生效边界未定义 |
| M5 | 能力路由 | 能力缺口时建议派发具备能力的子角色 | 无可用角色时的降级提示缺失 |
| M6 | 团队组建 | 不支持；rust 协作骨架未接真实执行（§4.2） | 团队消费语义全部缺失 |
| M7 | 用量归因 | 会话级可查 | 角色级归因未定义 |

### 1.5 能力标签机制（使用域 SDR-001 裁决依据）

三层结构（`agent-core-v2/src/agent/capability/`；rust 侧 `mirri-core/src/capability.rs` 为同构移植）：

- **工具侧**：内置工具经 contribution 元数据声明能力；MCP 服务器的工具经项目 `.mirri-code/integrations.yaml` 按服务器名挂能力标签，并可声明 `preferOver`（优先于内置同类工具）
- **角色侧**：profile 的 `capabilitiesRequired` 声明能力标签（如 `code.explore`）；内置 explore/plan 即用标签而非工具名（跨环境可移植）
- **解析**：`augmentToolNames(tools, capabilitiesRequired, availableTools)` 把标签展开为工具名并集（含自动发现符合条件的 MCP 工具），与 `tools` 白名单合并为激活工具集——两个概念面向用户收敛到同一件事

模态路由（`agentCapabilityGapService`）：检查用户消息的模态（图/视频/音频）vs 绑定模型支持情况；建议派发的子 agent 按各角色 `defaultModel` 在模型目录中的模态属性（`image_in` 等）匹配，**不经能力标签**——工具面概念合并不影响模态路由。

## 2. 主 agent 选择机制现状（Epic B 的依据）

**结论：现状产品不可选择主 agent，只能全局替换默认角色。**

1. **引擎内部有绑定机制、无用户面暴露**：`CreateAgentOptions.binding`（`agentLifecycle.ts:32`）→ `sessionLifecycleService.ts:203-214` 的 `CreateSessionOptions.mainAgentBinding` → `profileService.ts:334` 的 `bind({profile, model, thinking})`；`fork`（`agentLifecycleService.ts:208-229`）支持 binding override。`mainAgentBinding` 仅存在于 agent-core-v2 内部及其测试（repo 全局检索仅 3 处：类型、实现、test），klient wire 契约的 `session.create` 无对应参数。内部唯一消费方：sessionInit 派发 init 子 agent（`sessionInitService.ts:80-86`）。
2. **CLI 明确拒绝**：`apps/mirri-code/test/cli/options.test.ts:400-408` 断言 `--agent=default`、`--agent-file=x` 直接 throw——参数名已预留但当前不可用。
3. **唯一的"定制主 agent"路径是全局替换默认**：
   - `SYSTEM.md`（`workspaceAgentProfileLoader/internal/systemFile.ts:1-16`）：`<brandHome>/SYSTEM.md` 永久替换内置默认角色的系统提示词——**仅替换 prompt**，工具/描述复制内置；支持 `${base_prompt}` 嵌入原提示词；项目级 `agent.md`、`--agent-file` 可压过它
   - 同名 override（§1.2）：替换那一个 `agent` profile
   - 共性：都是"替换"，不是"可选的多个"，且 SYSTEM.md 路径工具面固定为内置默认

## 3. 现有配置页与接口面

- **wire 类型**：`packages/protocol/src/rest/agents.ts` `profileEntrySchema`（name/description/source/builtin/essential/enabled/has_override/file_path/extends/default_model/tools/when_to_use/system_prompt_template/effective_system_prompt/prompt_vars）；`createProfileRequestSchema` 另含 capabilities/capabilities_required（update 为其 partial）。**未覆盖**：`disallowedTools`；`source` 枚举仅 `builtin/user/project/extra` 四值——引擎侧的 `plugin`、`explicit` 来源未进 wire 枚举。
- **REST（v2）**：`packages/kap-server/src/routes/agents.ts`——`GET/POST /agents`、`GET/PUT/DELETE /agents/{name}`、`POST /agents/{name}:enable|:disable|:reset`。CRUD 由 route 直接写 `<home>/agents/*.md` 后触发重扫（v2 无引擎级 ProfileService 写路径）；`enabled` 写进 frontmatter（v2 无 `disabled_agents` config）。错误码 40418/40419/41306/41307。
- **Web 设置页**：`apps/mirri-web/src/components/settings/AgentProfilesPanel.vue`——master-detail：列表（builtin/custom/essential 徽章、启用开关、essential 锁定）、创建/编辑/删除、内置角色 `effectiveSystemPrompt` 只读预览；入口 `apps/mirri-web/src/App.vue:476-488`；wire 映射 `ws-rpc-api.ts:2714`；前端类型 `api/types.ts:846-864`。
- **终端**：`apps/mirri-code/src/tui/commands/agents.ts`（`/agents` 斜杠命令，仅列表与启停开关），能力远弱于设置页。
- **v1 遗留**：`packages/agent-core/src/services/profile/profileService.ts` 提供 ProfileService CRUD（v1）；v1 server 包已退役，kap-server 的路由为逐路径镜像。

## 4. rust 内核现状

### 4.1 profile 体系（`rust/crates/mirri-core/src/profile.rs`，2348 行）

- **数据结构**：`name`、`extends`（继承链 + 环检测；`subagents` 不继承）、`model`、`provider`（anthropic/openai/openai-responses/google/kimi/custom）、`system_prompt`/`system_prompt_file`、`max_turns`、`max_steps_per_turn`、`permission_mode`、`enabled_tools`/`disabled_tools`、`capabilities_required`、`thinking_effort`、`subagents`（**内联结构体**，非 profile 名引用）、`description`、`when_to_use`、`default_model`、`capabilities`、`prompt_vars`。
- **ProfileManager**：TOML/JSON 加载（按扩展名）、`upsert_profile` 持久化 `<dir>/<name>.toml`、`delete_profile`（删内置名回退内置定义 = TS `:reset` 语义）、`resolve` → `AgentScopeConfig`。
- **内置**：`agent`（essential 不可改删）/ `coder` / `explore` / `plan`。
- **发现**：仅 `<mirri-home>/profiles/*.toml|json` 单根（bootstrap 加载 `rust/crates/mirri-embedded/src/bootstrap.rs:392`）；CLI 有热重载 watcher（`mirri-cli/src/watcher.rs`）。
- **消费**：`mirri-core/src/subagent/host.rs` spawn 子 agent 时 resolve → 真实执行。
- **API**：mirri-proto 注册 5 个 `agent.*Profile` 方法（both 可用）；`mirri-embedded/src/embedded_server/profile_handlers.rs`（305 行，策略对齐 TS 路由：essential `agent` 不可写删、裸内置只读、输出 TS ProfileEntry wire 形状）；`mirri-server/src/http.rs:779-852` REST `/api/v1/agents`。

### 4.2 mirri-collaboration（协议骨架，未接执行）

931 行 src（含 error.rs 等）+ 测试，四层（`src/lib.rs` 自述）：

- `domain.rs`：`RoleDefinition`（name/description/system_prompt/capabilities/max_agents——**独立结构，不绑 profile 系统**）、`CollaborationRules`（CommunicationPattern: Sequential/Parallel/Hierarchical；InterventionPolicy；ConflictResolution: Human/MainAgent/MajorityVote；max_rounds）
- `protocol.rs`：`AgentMessage` 8 种（TaskAssignment/TaskResult/Question/Answer/ConflictEscalation/Intervention/StatusUpdate/Unknown 前向兼容）+ TaskStatus/AgentStatus
- `session.rs`：状态机 NotStarted/InProgress{round}/Paused/Completed/Failed；`AgentHandle.agent_id` 为合成 UUID，**不关联真实 agent**
- `manager.rs`：纯内存台账，不驱动 LLM 执行
- 接入：`mirri-core/src/tools/collaboration.rs` 注册 7 个内置工具；无 RPC wire 条目、无持久化、permission 层无协作工具策略

### 4.3 双内核差异矩阵

| 维度 | TS（agent-profile + agent-core-v2） | rust（mirri-core） |
|---|---|---|
| 文件格式 | `.md` frontmatter（主）+ `.yaml/.yml` | `.toml` / `.json` |
| 发现根 | plugin/project/user/extra/explicit 五层 | 单一 `<mirri-home>/profiles/` |
| 覆盖语义 | 分层优先级 + essential `agent` 需 `override: true` | 同名替换 + essential 不可改删 |
| 错误容错 | 单文件失败 skip + 警告 | all-or-nothing，一个坏文件整目录失败 |
| 独有字段 | override/disallowedTools/systemPromptTemplate/summaryPolicy | provider/permission_mode/max_turns/thinking_effort/system_prompt_file |
| subagents | profile 名引用 | 内联结构体 |
| 主 agent 选择 | 无（§2） | ProfileManager 可注册任意 profile，但主 agent 装配路径同样固定默认 |

### 4.4 roadmap 定位

`plan/rust-v2-method-parity.md`：rust 定位为 TS 引擎对等替代；profile 的 5 个 RPC 方法已落地（B2/P6 批），**格式对齐与多源发现不在 parity 计划内**，"N2 collaboration" 列为非目标。`rust-engine-packaging.md` 与 agent profile 无关（Cargo release profile）。

### 4.5 rust 独有字段用途调查（配置面 SDR-003 裁决依据）

| 字段 | 用途 | 评价 |
|---|---|---|
| `provider` | `ProfileManager::resolve`（profile.rs:415-485）不经过模型目录：`model` 存裸模型 id，必须显式配对 provider 才能定位供应商（`model_id: merged.model, provider_type`） | rust 侧也有 `model_catalog.rs`（别名 → 供应商），profile 路径未用；TS 的 `defaultModel` 为别名、绑定时经目录解析——别名机制下 provider 字段冗余 |
| `thinking_effort` | 每 agent 思考力度；TS 侧为 agent 级既有概念（`setThinking`、派发 binding 可带 thinking），仅未暴露进 profile schema | 既有概念进 schema，采纳无争议 |
| `max_turns`（默认 200）/ `max_steps_per_turn` | agent 轮次与每轮步数上限，防子 agent 失控的护栏 | 无用户故事驱动，不进用户字段面 |
| `permission_mode`（默认 Manual） | 每角色权限模式；TS 侧权限模式在会话/agent 级（`agentPermissionModeService`、`setPermission` RPC），不在 profile | 与协作 spec N3（协作非权限豁免区）有张力，需专门设计 |

## 5. 协作域 spec 的关联条目

[collaboration-requirements.md](collaboration-requirements.md)（v1，2026-09-04）与角色域相关的条目：

- **US1.1 角色职责声明（P0）**：职责与产出契约以 agent profile 的行为提示词正文承载，不设独立字段（其 SDR-001，随角色域 SDR-009 修订）；未声明向后兼容视为"通用执行者"；职责与工具白名单独立生效
- **US1.3 团队模板（P1）**：模板引用角色 profile（缺失报错指名）；项目 `.mirri-code/` 或用户目录分层
- **N6 单向依赖**：协作域依赖 agent 域（角色 = agent profile 扩展），反向禁止
- **开放点 Q4**：mirri-collaboration 协议骨架回收为 v1 实现基础（初步倾向）
- 配套 [collaboration-context.md](collaboration-context.md) 的团队模型差距速览 7 条（2026-09-04 快照）：其中 1（角色/职责）、5（看板）与配置页直接相关

## 6. 差距速览（配置页视角）

1. **主 agent 无选择入口**：引擎内绑定机制完备（binding/fork），wire/CLI/UI 三面皆无暴露；CLI 参数名已预留但被拒绝（§2）
2. **全局替换与多套并存的概念混淆**：SYSTEM.md/同名 override 是"替换默认"，与用户想要的"多个专用主 agent"是两条路径，现无 UI 区分呈现
3. **双内核配置不互通**：同一台机器切换引擎，角色集合完全不同（§4.3）
4. **wire 字段缺口**：`disallowedTools` 未进 `profileEntrySchema`；`capabilities` 仅在 create/update 请求中出现；`source` 枚举缺 `plugin`/`explicit` 两值（被压制的候选列表也未进 wire——引擎 `inspect()` 有，wire 无）
5. **终端与设置页能力不对等**：终端仅列表+启停
6. **页面与外部并发编辑**：无冲突处理规则（文件 watcher 存在但页面保存侧未定义）
7. **协作域承载方式已裁决**：职责/产出契约以提示词正文承载（协作 spec SDR-001），不进 schema；rust 侧 RoleDefinition 与 profile 系统未合流仍是现状
8. **启停在引擎侧无消费（R5：存在但无效）**：v2 的 `enabled` 仅 REST 层读写（`kap-server/src/routes/agents.ts:35-36`：v2 无 `disabled_agents` config，启停写 frontmatter；:469-491 以正则读回）——引擎侧 catalog（`sessionAgentProfileCatalogService.ts`）与 Agent 工具候选列表（`agentTool.ts:146-174`）均不消费该字段，全仓验证 `enabled` 在 agent-core-v2 仅出现于 MCP 服务器与 flags 上下文。后果：设置页显示已禁用的角色，LLM 仍可在派发时选中。v1 的 `disabled_agents` config 有引擎消费，v2 迁移到 frontmatter 时丢了消费方。rust 引擎的 profile 解析入口对启停有消费检查（`ensure_not_disabled`，config 驱动）——v2 补消费方时，v1 与 rust 两个参照实现均存在
