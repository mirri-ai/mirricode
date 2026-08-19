# SDK v2 客户端能力补齐计划

**文档状态**：计划（未开工）
**目标对象**：`packages/node-sdk/src/sdk-rpc-client-v2.ts` 及其辅助层
**参考实现**：上游 kimi-code `packages/node-sdk/src/sdk-rpc-client-v2.ts`

---

## 1. 背景与问题定义

`SDKRpcClientV2` 是 SDK 面向 agent-core-v2 引擎的客户端。它继承 `SDKRpcClientBase`（v1 方法面），**未被 override 的方法一律落到 `getRpc()` 抛 `NOT_IMPLEMENTED`**。

CLI 默认已切 v2（`MIRRICODE_LEGACY_FLAG` 才回落 v1），因此任何未接线的方法都是运行期地雷。已发生的事故：启动路径调 `getConfig()` 直接崩溃（已修，commit `349af1c`）。

### 能力差距量化

| | 上游 kimi-code | 本仓库 |
|---|---|---|
| override 数 | 88 | 61 |
| 文件行数 | 2331 | 1557 |
| `src/v2/` 辅助模块 | 7 个 | 5 个（缺 `global-mcp.ts`、`import-context.ts`） |

缺口共 **27 个方法**。

---

## 2. 事实基线（调研结论）

以下事实决定了分组与优先级，均已核对源码。

### 2.1 基类声明的分界线

这是最重要的一条分界：**改动是否外溢到 SDK 契约层**。

- **基类已有声明**（`packages/node-sdk/src/rpc.ts`）：`reloadSession`、`getSessionWarnings`、`waitForBackgroundTasksOnPrint`、`handlePrintMainTurnCompleted`
  → 纯接线，只需在 v2 客户端写 override，不动契约。
- **基类无声明**：其余 23 个（含 `updateSessionMetadata`、`listCommands`、`runCommand`、`clearContext`、`importContext`、`listPluginCommandsGlobal`、`applyPersistedSecondaryModel`、`supportsAtomicSectionReplace`、`replaceConfigSections`、Global MCP 10 个、Workspace 4 个）
  → 需要同时新增基类方法声明与相关类型，属于 SDK 表面扩展。

### 2.2 klient facade 现状

`GlobalConfigFacade` 已完整具备 `getAll` / `inspect` / `set` / `replace` / `replaceSections` / `reload` / `diagnostics`；`GlobalFlagsFacade` 有 `list` / `explain` 等。
→ **config 组与 flags 组应走 facade**。当前 `349af1c` 走的是 `engineAccessor` 直调，属于架构债（文件注释自述 engineAccessor 是"迁移压力阀，非新公共 API 方向"）。

`packages/klient` 对 Global MCP、workspace trust **完全没有暴露面**（14 个方法名 0 命中）。
→ 这两组只能走 `engineAccessor`，与上游一致，不是取舍问题。

### 2.3 底层引擎能力

**已具备、只差接线**：`McpOAuthService`、`createMcpOAuthStore`、`McpConnectionManager`、`IWorkspaceTrust`、`IWorkspaceSkillCatalog`、`IWorkspaceMcpService`、`IHostFileSystem`、`IAtomicDocumentStore`、`loadMcpServers({includeProject})`、`atomicWrite`。

**根本缺失**：
- `src/v2/global-mcp.ts` 模块（上游 250 行：`GlobalMcpConfigStore` + 守卫 + 探针）
- `src/v2/import-context.ts` 模块
- 类型 `GlobalMcpServerConfig` / `McpTestResult` / `GlobalMcpServerAuthStatus` / `BeginGlobalMcpServerAuthResult` / `WorkspaceTrustInfo`
- `IAgentIdentity`（上游用它在物化 OAuth service 前冻结身份；我们引擎无对应机制，OAuth 客户端身份走 `clientLabel` 常量）

### 2.4 用户可感知度（决定优先级的硬事实）

- Global MCP 10 个方法在 `apps/mirri-code/src` 的调用点：**0 处**。
- Workspace trust 4 个方法在 CLI 的调用点：**0 处**。
- 且 web 端的 global MCP 功能**已有一条不经过 node-sdk 的工作旁路**：`apps/mirri-web` → daemon HTTP → `packages/kap-server/src/routes/mcpGlobal.ts`（793 行，直接读写 `<home>/mcp.json`）。

→ **这 14 个方法当前无人调用，补齐它们不解决任何用户问题**。原先设想的"全量 27 个对齐"是按上游行数对标，不是按需求对标。

### 2.5 schema 差异（移植时的陷阱）

上游 `global-mcp.ts` 注释称"v2 schema 丢掉了 `auth:'oauth'`"，故用 v1 schema 校验。**该前提在本仓库不成立且正好相反**：

- 本仓库 v2 `McpServerConfigSchema` **有** `auth: z.literal('oauth').optional()`（`agent-core-v2/src/mcpCore/config-schema.ts:22`）
- 本仓库 v1 `McpServerConfigSchema` **无** `auth` 字段（`agent-core/src/config/schema.ts:214-220`）

→ 若照抄上游用 v1 schema 校验，会把 `auth:'oauth'` 洗掉。**必须用 v2 schema**。

---

## 3. 分组与优先级

优先级按「用户可感知 × 契约成本」排序，不按上游文件顺序。

### P0 — 纯接线，基类已有声明

**状态：已完成。** 双引擎 e2e 10/10 通过（`test/e2e/session-service-batch.e2e.test.ts`）。

| 方法 | 引擎侧落点 | 用户可感知场景 |
|---|---|---|
| `reloadSession` | `IAgentActivityView` 忙检查 → `klient.global.config.reload()` + `plugins.reload()` → 引擎 close + `resumeSessionById` → 重新 wire | `/reload`、实验开关应用后重载 |
| `getSessionWarnings` | `IAgentProfileService.getAgentsMdWarning()`，空则 `prepareSystemPromptContext` 重算 | 启动时 AGENTS.md 超预算告警 |
| `waitForBackgroundTasksOnPrint` | 委托引擎 `IPrintModeService` | `mirri -p` 后台任务排空 |
| `handlePrintMainTurnCompleted` | 委托引擎 `IPrintModeService` | `mirri -p` steer 模式续跑 |

**与计划的两处偏离**：

1. **print 组比计划更省**：引擎**已有** `IPrintModeService`（agent scope，包索引已导出，此前无调用方），其 exit/drain/steer 三态与 ceiling / turn-cap 逻辑同上游 SDK 侧手工重建的实现等价，且 steer 状态由引擎持有。故直接委托，不在 SDK 侧重建策略，也不需要客户端维护 `printSteerStates` map。

2. **`getSessionWarnings` 只做一半，且是正确的一半**：上游 v1 的聚合含 secondary-model 警告，故其 v2 客户端追加 `ISessionSecondaryModelWarningService`。本仓库 v1 `Session.getSessionWarnings`（`agent-core/src/session/index.ts:690`）**只发 AGENTS.md 一项**。该引擎服务在本仓库存在，但补进去等于发明 v1 从未返回的警告，故不补，并在方法注释写明差异与将来接入点。

### P1 — 架构债偿还（无新功能）

**状态：已完成。** `test/v2/config-wiring.test.ts` 5/5 通过。

把 `349af1c` 引入的 `engineAccessor` 直调改为 klient facade：

- `getConfig` / `getConfigDiagnostics` / `setConfig` / `removeProvider` → `klient.global.config.*`
- `getExperimentalFeatures` → `klient.global.flags.list()`

`removeProvider` 的级联改用 `inspect().userValue`。

**对计划原文的更正**：计划称此举"修正 env overlay 值被写入用户配置的缺陷"。经验证**该缺陷不成立**——引擎的 `stripProvidersEnv`（`agent-core-v2/src/app/kosongConfig/configSection.ts:98`）在写入路径上已剥离 env 派生的 provider 条目。两次构造针对性回归测试、并用突变测试（把 `userValue` 改回 `value`）验证，均无法让测试失败，说明差异不可观测。

`userValue` 仍是正确写法：语义上用户层的级联只应基于用户层，且与 facade 的 `inspect` 契约一致——但它是**意图明确化**，不是 bug 修复。相关注释已改为诚实表述。

### P2 — 会话与命令面补齐（需扩基类契约）

**状态：可确定的部分已完成；其余 7 个受阻，需要产品决策。**

原计划假设这 9 个方法都是"上游有、我们缺、照着接上即可"。实际核对本仓库源码后：

| 方法 | 本仓库实际状态 |
|---|---|
| `updateSessionMetadata` | **已实现**。v1 CoreAPI 有（`agent-core/src/rpc/core-api.ts:514`，实现见 `session/rpc.ts:72`），语义可确定 |
| `clearContext` | **已实现**。v1 CoreAPI 有（`core-api.ts:490`），落到 v2 的 `IAgentPromptService.clear()` |
| `supportsAtomicSectionReplace` / `replaceConfigSections` | **已实现**。见下方说明——这不是产品扩展，而是引擎能力探测 |
| `listPluginCommandsGlobal` | **早已实现**（`sdk-rpc-client-v2.ts:1615`）。计划把它列为缺失是错的 |
| `listCommands` / `runCommand` | **上游 v1 CoreAPI 也没有**（已核对 kimi-code），是上游 v2 时代新增能力，非我们漏迁移 |
| `importContext` | 我们仓库**零命中**（无改名等价物）。上游 v1 有对应契约，属两仓库产品差异 |
| `applyPersistedSecondaryModel` | 我们仓库**零命中**（无改名等价物）。同上 |

#### 已完成部分

**`updateSessionMetadata` / `clearContext`** 已在 `rpc.ts` 补声明（含 `UpdateSessionMetadataRpcInput`）并在 v2 客户端实现，测试见 `test/v2/session-mutations.test.ts`（4/4）。

实现中发现并修复的真实缺陷：klient 的 `session.update()` 只在**顶层**浅合并，直接传 `{ custom: patch }` 会把整个 custom 文档替换掉，与 v1 的合并语义不符。实现改为先读现值再合并。该 bug 由回归测试抓出，非事后补测。

注：这两个方法在 `MirriHarness` / `Session` 公开层没有入口，只存在于 RPC 基类——即当前无 SDK 消费方。给公开层加入口属产品面扩展，未做。

**`supportsAtomicSectionReplace` / `replaceConfigSections`** —— 初判为"产品扩展、需决策"是**错的**，已更正并实现。

它们不是新功能，而是 SDK 向宿主暴露的**引擎能力探测对**：v2 引擎具备原子多段写（`IConfigService.replaceSections`，klient facade 也已透出），v1 只能整文档合并。上游用它解决一个真实的数据完整性问题——provider 刷新时把"删 provider + 写回"合成一次原子写（`kimi-code/apps/kimi-code/src/tui/controllers/auth-flow.ts:237`），避免进程中断留下"provider 已删、尚未恢复"的坏状态。

我们仓库的 `apps/mirri-code/src/tui/controllers/auth-flow.ts:169-171` 目前正走非原子的两次写路径，且 `removeProviderFromConfig`（`config-mapper.ts:135`）已存在却无调用方——显然就是为此准备的。基类默认 `false`（v1 语义），v2 客户端覆盖为 `true`，并在 `MirriHarness` 转发以便宿主调用。测试见 `config-wiring.test.ts`（含 replace 语义与 section 清除）。

TUI 侧接入不在本次 scope（只改 packages/node-sdk），可另行跟进。

#### 受阻部分（4 个）—— 底层能力缺失，非决策问题

已逐个核实到引擎层，结论比上一轮更明确：

| 方法 | 性质 | 依据 |
|---|---|---|
| `listCommands` / `runCommand` | **引擎能力缺失** | 上游基类注释写明这是 "contributed-command 接缝，只存在于 agent-core-v2 引擎"，与 `replaceConfigSections` 同为能力探测/降级对。但**我们的 agent-core-v2 没有这个接缝**：`AgentCommandInfo` / `IAgentCommand` / `contributedCommand` 全部零命中，klient agent facade 也只有 shell command |
| `importContext` | **产品差异** | 上游基类走 `getRpc()`（真实 v1 能力），我们 v1 CoreAPI 无此方法；v2 引擎的 prompt 服务也无 import 能力 |
| `applyPersistedSecondaryModel` | **产品差异** | 同上。我们 v2 只有 `recheckSecondaryModelWarning`（检查告警），没有"应用已持久化的次要模型"这一动作 |

**结论**：这四个不是"要不要对齐上游"的产品决策，而是**底层能力不存在**——前两个需要先在 agent-core-v2 建 contributed-command 接缝，后两个需要先在引擎实现对应动作。都超出本次 scope（只改 packages/node-sdk）。

按停止规则：实现可达的部分、记录差异、不伪造对齐。若将来需要这些能力，应作为 agent-core-v2 的引擎特性立项，SDK 接线是其下游步骤。

### P3 — 暂缓，不纳入本轮

Global MCP 10 个 + Workspace trust 4 个。

**暂缓理由**（非能力不足）：CLI 调用点为 0，web 端已有 kap-server 旁路覆盖同一需求。现在补齐等于为无人调用的接口引入 250+ 行移植代码、5 个新类型、14 个基类声明，且要处理 `IAgentIdentity` 缺失与 schema 陷阱。

**重新纳入的触发条件**（满足任一即启动）：
1. CLI/TUI 需要暴露 global MCP 管理界面；
2. 决定让 kap-server 的 `mcpGlobal.ts` 改为复用 SDK 而非自持一份读写逻辑（消除双实现）；
3. 外部 SDK 消费方明确提出需求。

---

## 4. 执行约束

### 架构方向
- **facade 优先**：能力在 klient facade 存在时必须走 facade；仅当 facade 无对应能力时才用 `engineAccessor`，并在方法注释注明"facade 无此能力"的理由。
- 不照抄上游的历史包裹。上游注释与本仓库事实冲突时（如 schema 的 `auth` 字段），以本仓库源码为准并写明分歧。
- deep import 在本仓库是既有惯例（`agent-core-v2` 的 `exports` 映射 `"./*"`），可用，但优先走包索引导出。

### 每组的完成证据
1. 对应的 vitest 用例通过（Given-When-Then 命名）；
2. `rg` 确认目标方法不再落到 `getRpc()`；
3. `pnpm --filter <pkg> run typecheck` 逐包通过（不看根目录聚合输出，它会被 rtk 污染）；
4. `pnpm --filter @mirri-ai/mirri-code-sdk exec vitest run` 全量绿。

### 边界
- 只动 `packages/node-sdk`（必要时 `packages/klient` 补 facade）；
- 不动 `packages/agent-core`（v1 引擎，仅作类型契约来源）；
- 不动 `packages/kap-server` 的既有 global MCP 实现；
- 不为通过测试而放宽断言；引擎确实缺能力时，记录差异而非伪造对齐。

---

## 5. 已知未决问题（已结算）

1. ~~`ISessionSecondaryModelWarningService` 在本仓库是否存在~~ → **存在**（`agent-core-v2/src/session/subagent/secondaryModelWarningService.ts`）。但本仓库 v1 的 `getSessionWarnings` 只发 AGENTS.md 一项，故不聚合它，见 P0 说明。
2. ~~`listCommands` / `runCommand` 的 v1 语义~~ → **v1 无此契约，v2 引擎也无 contributed-command 接缝**，见 P2 受阻部分。
3. ~~P1 改 facade 后 `engineAccessor` 使用面收敛~~ → 已在文件头注释逐条标注每个方法的落点与选择 facade / engineAccessor 的理由。

---

## 6. 最终状态

| 组 | 完成度 | 测试 |
|---|---|---|
| P0 | 4/4 | `test/e2e/session-service-batch.e2e.test.ts` 双引擎 10/10 |
| P1 | 5/5 | `test/v2/config-wiring.test.ts` 7/7 |
| P2 | 5/9（4 个因底层能力缺失受阻） | `test/v2/session-mutations.test.ts` 4/4 + 上述 config 用例 |

**验证**：node-sdk 全量 37 文件 / 255 测试绿；node-sdk、klient、mirri-code 三包 typecheck 退出 0；13 个已实现方法逐一确认 override、不再落到 `getRpc()`。

**实现过程中发现并修复的真实缺陷**：klient `session.update()` 的顶层浅合并会整体替换 `custom` 文档（由回归测试抓出，非事后补测）。

**发现的既有改进机会（不在本次 scope）**：`apps/mirri-code/src/tui/controllers/auth-flow.ts:169-171` 的 provider 刷新走非原子两次写，现在 SDK 已具备 `replaceConfigSections`，TUI 侧可接入以消除"provider 已删、尚未恢复"的中断窗口。

