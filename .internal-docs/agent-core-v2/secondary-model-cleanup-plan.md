# secondary model 清理计划（agent-core-v2）

> 已确认决策（2026-08-12）：
> - **D1 范围**：引擎 + kap-server + node-sdk + 文档全量清理；
> - **D2 关键字/字段**：`'primary'`/`'secondary'` 两个关键字、`AgentModelPreference` 类型、`modelPreference` 字段**全部删除**（不保留 'primary'）；模型解析化简为「显式 alias 或继承」；
> - **D3 changeset**：`minor`。

## 背景与结论

`secondary model` 是上游 kimi-code 的 subagent 默认模型槽（`/secondary-model` 命令 + `[secondary_model]` 配置段），随 v2 引擎实现一并被 port 进来，但：

- 该特性由 `MIRRICODE_EXPERIMENTAL_SECONDARY_MODEL` flag 控制，**默认关闭**；
- Mirri 已有更完整的两条能力链替代它：
  1. **custom-agents 体系**：per-agent `defaultModel` / `modelAlias`，subagent 每次 spawn 实时解析绑定（T6/T7 的 `applyPersistedSecondaryModel` 因此无意义）；
  2. **主模型 LLM 的模型感知**：curated aliases + capability hints（vision/video/audio/tool-use）+ 自由 alias，由 main agent 按任务难度自行选模型。
- 产品判断：secondary 是「叠床架的便利层」，用户价值被上面两条覆盖 → **从 v2 引擎整体移除**。

v1（`packages/agent-core`）中不存在 secondary 概念（无 `applyPersistedSecondaryModel` 契约），因此清理面**只涉及 `packages/agent-core-v2` + `packages/kap-server` + `packages/node-sdk` 注释**。

## 影响面（完整清单，删除前需逐一确认）

rg 全仓确认，所有引用点：

### 引擎核心（packages/agent-core-v2/src）

要删除/修改的模块：

| 文件 | 现状 | 动作 |
|------|------|------|
| `src/app/kosongConfig/configSection.ts` | L314-340: `SECONDARY_MODEL_SECTION`/`SECONDARY_MODEL_ENV`/`SECONDARY_MODEL_EFFORT_ENV`/`SecondaryModelConfigSchema`/`SecondaryModelConfig`/`parseNonEmptyEnv`/`secondaryModelEnvBindings`/`registerConfigSection(...)` | 删除这些声明与注册；`parseNonEmptyEnv` 若无其他用途一并删 |
| `src/app/kosongConfig/secondaryModelOverlay.ts` | 整文件：`SECONDARY_DERIVED_MODEL_ID='__secondary__'`、`secondaryModelPatch`、`secondaryModelOverlay`（`registerConfigOverlay`） | 整文件删除 |
| `src/session/subagent/flag.ts` | 整文件：`SECONDARY_MODEL_FLAG_ID`/`SECONDARY_MODEL_FLAG_ENV`/`secondaryModelFlag` + `registerFlagDefinition` | 整文件删除 |
| `src/session/subagent/secondaryModelWarning.ts` | 整文件（`ISessionSecondaryModelWarningService`、`SecondaryModelWarning`、两个 warning code） | 整文件删除 |
| `src/session/subagent/secondaryModelWarningService.ts` | 整文件（`SessionSecondaryModelWarningService` + DI `register(...)`） | 整文件删除 |
| `src/session/subagent/configSection.ts` | L102 `resolveSecondaryModel`、L157-168 Tier2 分支、L188-215 `buildSubagentModelDescriptions` 的 secondary 行（注意：`buildCuratedAliases` **保留**）、L322-335 `validateModelAlias` 的 `'secondary'` 关键字、L348 `stripSubagentModelParameter`（已无调用方，死代码）、L363-388 `wrapSubagentModelError` 的 derived/`MIRRICODE_SECONDARY_MODEL` 分支、文件头注释 | 按决策 D2（见下）删除对应片段 |
| `src/index.ts` | L117 import overlay、L133-138 导出、L343-344 导出 warning | 删除对应行 |
| `src/agent/tools/agent/agentTool.ts` | L90 import flag、L116-117 注释、L166 flag 检查、L282-287 `?? profile.modelPreference`、L523-524 "Model preference:" meta 行、`model` 参数描述文本 | 按 D2 清理 |
| `src/agent/tools/agent-swarm/agentSwarmTool.ts` | L60 description 文本、L95-96 注释、L182 `?? targetProfile.modelPreference` | 同上 |
| `src/agent/tools/agent/agent.ts` | L62 参数描述文本（提到 secondary） | 更新措辞 |
| `src/app/agentProfileCatalog/agentProfileCatalog.ts` | L43 `AgentModelPreference` 类型、L82 `demo.modelPreference?` | 按 D2：删类型+字段 |
| `src/workspace/workspaceAgentProfileLoader/internal/agentProfileFromFile.ts` | L46 `modelPreference: undefined` | 按 D2：删 |
| `src/workspace/sessionLifecycle/sessionLifecycleService.ts` | L47 注释提到 secondary-model warning | 删注释句 |
| `src/app/skillCatalog/builtin/update-config.md` | L23 提到 `secondary_model` | 删 token |

### 注意「保留」的（不清！）

- `buildCuratedAliases` + capability 提示（vision/video/audio/tool-use）——custom-agent 目标需要；
- `ModelOverrideSchema`（L197/L210 被 `overrides` 段使用，非 secondary 专属）；
- `model` 参数本身（给 LLM 选 alias / 显式指定）——保留，语义改为「别名或留空继承」。

### REST 服务侧（packages/kap-server）

| 文件 | 动作 |
|------|------|
| `src/routes/sessions.ts` | L45 注释、L1049-1051 取 warning、L1062-1070 拼 warnings、import | 删除 |
| `src/routes/config.ts` | L33 import `SECONDARY_DERIVED_MODEL_ID`、L147 `withoutDerivedSecondaryEntry(...)`、L172-184 函数 | 删除（overlay 移除后 `__secondary__` 不再出现） |
| `src/routes/modelCatalog.ts` | L57 import、L239 filter derived | 删除 |
| `src/services/legacyStatus/legacyStatus.ts` | L24 import、L152 filter derived | 删除 |
| `src/protocol/rest-config.ts` | L16、L40 `secondary_model: z.unknown().optional()` | 删除 |

### SDK 侧（packages/node-sdk）

| 文件 | 动作 |
|------|------|
| `node-sdk/src/sdk-rpc-client-v2.ts` | L1144-1151 注释（提到 engine 存在 warning 服务待接） | 顺带删末句 |

### 测试（agent-core-v2 / kap-server）

| 文件 | 动作 |
|------|------|
| `test/session/subagent/secondaryModelWarning.test.ts` | 整文件删 |
| `test/app/kosongConfig/secondaryModelOverlay.test.ts` | 整文件删 |
| `test/session/subagent/modelSelection.test.ts` | 删 secondary 用例，保留 inherit / curated 用例 |
| `test/app/config/config.test.ts` | 删 `secondaryModelFlags` helper +「resolves the spawn binding」「secondaryModel config section」等 describe |
| `test/tool/tool.test.ts` | 大量：derived binding（L1054-1242、L2259-2390）、description（L793-814）、`secondaryModelFlags` helper；`llm.tools_snapshot` 内联断言（L3189/3257 及 loop.test.ts:124）在**改描述文本后需重刷 hash** |
| `test/agent/swarm/swarm.test.ts` | L1050-1108 |
| `test/session/swarm/sessionSwarm.test.ts` | L1111-1148 |
| `test/workspace/sessionLifecycle/sessionLifecycle.test.ts` | L596-599 stub pair |
| `test/_mirri-harness/index.ts` | L81、L279 overlay import/apply |
| `kap-server/test/config.test.ts` | L103-114 POST secondary_model 用例删除 |
| `kap-server/test/modelCatalog.test.ts` | L163 附近 overlay 组合用例删除 |
| 注意：`test/app/plugin/pluginSystem.test.ts` L350-366 的 `secondary` 是**第二个 MCP server 名称**，与本次无关，勿动 | |

### 文档

| 文件 | 动作 |
|------|------|
| `packages/agent-core-v2/docs/config-manifest.toml` | L28、L36（section 列表/overlay 列表）+ L287-301（`[secondary_model]` 段）删除 |
| 用户文档 `docs/` | 已 rg 确认无 secondary 引用，无需同步 |

## 删除后语义（目标态）

subagent 模型解析简化为两级：
1. **显式 alias**：`model` 参数 = 显式模型别名（校验通过 catalog / curated），有则用之；
2. **继承调用方**：无参数或参数为空 → 用 caller 的模型（profile `defaultModel` 仍作为调用方可配置的默认）。

`MIRRICODE_SECONDARY_MODEL` / `MIRRICODE_SECONDARY_EFFORT` / `[secondary_model]` / `MIRRICODE_EXPERIMENTAL_SECONDARY_MODEL` 全部变为未识别项：环境变量被忽略，config 段未注册——行为与「实验关闭」时一致（老 config 不报错，只是不再生效）。

## 执行顺序（每步以 typecheck + 相关 vitest 自验）

1. **引擎核心**：删 flag 模块 → configSection(subagent) 移除 secondary 分支 → kosongConfig 的 section+overlay → warning 服务 → index.ts 导出 → tools（agent/agent-swarm）清理
2. **profile/schema**：按 D2 删 `AgentModelPreference` / `modelPreference` / `SubagentModelChoice`
3. **kap-server**：sessions/config/modelCatalog/legacyStatus/rest-config 清理
4. **node-sdk**：注释收尾
5. **测试**：删文件、改用例、刷 tools_snapshot hash
6. **文档**：config-manifest.toml + update-config.md
7. **验证**：`rg -ni 'secondary_model|secondaryModel|SECONDARY_MODEL|secondary-model|secondary model' packages/agent-core-v2/src packages/agent-core-v2/test packages/kap-server/src packages/node-sdk/src` → 期望仅剩无关词（如 MCP server 名 'secondary'）；再跑
   - `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
   - `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/session/subagent test/tool test/app/config test/agent/swarm test/session/swarm test/_mirri-harness test/app/kosongConfig`
   - `pnpm --filter @mirri-ai/kap-server run typecheck` + 相关 vitest
   - 最后跑 `./build.sh` 全量质量门禁

8. **changeset**：gen-changesets，`minor`（删除默认关闭的实验特性；不涉及 major 判据——无已发布行为变化。若认为影响已启用用户，需显式确认后升级）