# SDK v2 补齐 —— 剩余工作拆分与委托计划

**前置状态**：P0 (4/4)、P1 (5/5)、P2 (5/9) 已完成并验证（37 文件 / 255 测试绿，三包 typecheck 退出 0）。
详见 `sdk-v2-capability-gap-plan.md`。

本文件规划**剩余工作**，按依赖顺序拆成可独立委托的小块。

---

## 任务全景

| ID | 任务 | 依赖 | 影响包 | 风险 |
|---|---|---|---|---|
| T1 | TUI provider 刷新接入原子写 | 无 | mirri-code | 低 |
| T2 | agent-core-v2 建 contributed-command 接缝 | 无 | agent-core-v2 | 高 |
| T3 | SDK 接线 `listCommands` / `runCommand` | T2 | node-sdk, klient | 中 |
| T4 | 引擎实现 context import | 无 | agent-core-v2 | 中 |
| T5 | SDK 接线 `importContext` | T4 | node-sdk | 低 |
| T6 | 引擎实现 apply-persisted-secondary-model | 无 | agent-core-v2 | 中 |
| T7 | SDK 接线 `applyPersistedSecondaryModel` | T6 | node-sdk | 低 |

**建议执行顺序**：T1 先做（独立、收益明确、风险最低），T2–T7 需产品确认是否要这些能力后再启动。

---

## T1 —— TUI provider 刷新接入原子写 ✅ 已完成

### 问题

`apps/mirri-code/src/tui/controllers/auth-flow.ts:165-183` 构造 `RefreshProviderHost` 时，`removeProvider` 与 `setConfig` 是**两次独立写**。provider 刷新的语义是"删旧 provider → 写回新记录"，中间进程退出会在 `config.toml` 留下"provider 已删、尚未恢复"的坏状态。

SDK 侧的原子写能力已经就绪（本次已实现）：
- `harness.supportsAtomicSectionReplace(): boolean` —— v2 返回 true，v1 返回 false
- `harness.replaceConfigSections(sections: Record<string, unknown>): Promise<void>` —— 一次原子写，值为 `undefined` 的段被清除

`removeProviderFromConfig(config, providerId)` 已存在于 `packages/node-sdk/src/v2/config-mapper.ts:135`，当前**无调用方**，就是为此准备的。

### 做法

在 `auth-flow.ts` 里按引擎能力分支构造 host：

- **不支持原子写**（v1）：保持现状的顺序写路径，一行不改。
- **支持原子写**（v2）：改为**暂存 + 单次提交**——
  - `getConfig` 读取后存入本地 `staged` 变量并返回；
  - `removeProvider` 只在 `staged` 上用 `removeProviderFromConfig` 计算，不落盘；
  - `setConfig` 把 patch 合并进 `staged`，然后调 `replaceConfigSections` 一次性写入。

注意：`Object.entries(patch)` 会保留值为 `undefined` 的键，这正是表达"清除某段"所需——不要过滤掉它们。

参考实现（语义参考，勿照抄包裹）：上游 kimi-code 的 `apps/kimi-code/src/tui/controllers/auth-flow.ts:230-274`。

### 边界

- 只改 `apps/mirri-code/src/tui/controllers/auth-flow.ts`；
- 不改 `refresh-providers.ts` 的 `RefreshProviderHost` 接口（两条路径共用同一契约）；
- 不动 `packages/oauth` 的 orchestrator。

### 验收

- 新增测试覆盖两条分支：支持原子写时 `replaceConfigSections` 被调用一次且 `removeProvider` 不触发落盘；不支持时退回顺序写。
- `pnpm --filter @mirri-ai/mirri-code run typecheck` 退出 0；
- `pnpm --filter @mirri-ai/mirri-code exec vitest run` 相关用例绿。

### 完成记录

**改动**：
- `apps/mirri-code/src/tui/controllers/auth-flow.ts` —— 抽出 `buildRefreshHost()`，按 `supportsAtomicSectionReplace()` 分两条路径；
- `packages/node-sdk/src/index.ts` —— 补导出 `removeProviderFromConfig`。

**测试**：`apps/mirri-code/test/tui/controllers/auth-flow.test.ts`（新建，8 条全绿），覆盖原子分支单次写、undefined 键保留、顺序写分支、暂存契约保护、scope 透传、变更后刷新本地模型。

**突变验证**：把 `Object.fromEntries(Object.entries(patch))` 改成过滤掉 `undefined` 值后，"保留 undefined 键"那条测试变红——确认该断言真正守护了"清除某段"的语义，不是摆设。

**验收结果**：8/8 测试绿；`mirri-code`、`node-sdk` 两包 typecheck 均退出 0。

---

## T2 —— agent-core-v2 建 contributed-command 接缝（需产品确认）

### 背景

上游 SDK 基类注释写明 `listCommands` / `runCommand` 是 "contributed-command 接缝，只存在于 agent-core-v2 引擎"。本仓库的 agent-core-v2 **没有这个接缝**：`AgentCommandInfo`、`IAgentCommand`、`contributedCommand` 全部零命中，klient agent facade 也只有 shell command。

### 前置决策（已调研定案）

上游 `kimi-code` 的 agent-core-v2 有完整 `agent/command/` 模块，该能力是"贡献式命令"接缝的实现（详见下方定义）。本仓库缺失，需在 agent-core-v2 重建。

**三者语义对照**（2026-08 调研核实）：

| 维度 | Skill activation | Plugin command | Contributed command |
|---|---|---|---|
| 本质 | 起一行 agent turn | prompt 模板替换 | 引擎内可执行函数（DI 上下文） |
| 入口 | slash `/技能名` 或模型 `Skill` tool | slash（用户）/ 程序化 | 仅程序化 RPC（`SDK runCommand`） |
| 生命周期 | skill 目录/插件 | agent | collection 按 provider 单元 |
| 本仓库现状 | ✅ 已有 | ✅ 已有 | ❌ 缺失 |

**边界决策**：三者路由天然分离——slash 层只消费 plugin command + 技能激活；contributed command 是独立 RPC 通道，不经 slash、不经 LLM，名字互不遮蔽。**不在 TUI 加 contributed command 的 slash 入口**（上游也没有）。若未来需聚合到同一 slash 面板，再定义"同名后注册者优先（= list() last-wins）"并分组展示。

### 定位与安全边界（2026-08 定案）

**定位结论**：contributed command 按**内部解耦架构**对待——它是把"确定性可执行动作"抽成注册表 + RPC 通道的引擎内部接缝，**不是用户功能，也不视为面向第三方（外部宿主）的公开契约**：

- 唯一消费面是 SDK 宿主**进程内**调用 `runCommand` / `listCommands`（受信代码）；
- 注册面是进程内 `register()`（无配置文件、无 CLI、无插件生态接入）；
- 网络后端（kap-server）**未暴露**这两个方法，无外部网络入口；
- 无用户入口（TUI / CLI 未接入），用户文档页已移除（`docs/reference/commands.md` 删除，2026-08，因无用户可见行为）。

**相应决策**：暂不采用对外安全对策——不接 permission gate、不做来源白名单与命令能力白名单、不加审计。`CommandRunContext.get` 保留全容器 DI（进程内受信代码可访问任意服务，无需收窄）。

**未来重新评估触发条件**（满足其一即重开本议题，先补安全层再外放）：

1. `runCommand` / `listCommands` 进入网络通道（如 kap-server 或 daemon 流暴露该方法）；
2. 插件/非受信代码获得向 `register()` 注册命令的能力（届时先做来源信任 + 权限，再补桥）。

### 实施范围（镜像上游，照搬语义、对齐本仓库 DI 路径）

- `packages/agent-core-v2/src/agent/command/` 三个文件：
  - `commandContribution.ts` — `CommandContribution` collection token + `CommandRunContext {args, get}`（`#/_base/di/collection`）
  - `agentCommand.ts` — `AgentCommandInfo {name, description?, source}` + `IAgentCommandService` 契约
  - `agentCommandService.ts` — 实现：`list()` 名字去重（last record wins，`source`=providerName）、`run()` 经 `invokeFunction` 执行、未知名抛 `REQUEST_INVALID`；`registerScopedService(LifecycleScope.Agent, ..., 'command')`
- `agent/rpc/core-api.ts` + `rpcService.ts` — 加 `listCommands` / `runCommand` 两个 handler，注入 `IAgentCommandService`
- `packages/klient` — `contract/agent/rpc.ts` 契约方法 + `core/facade/agent.ts` 的 `listCommands()` / `runCommand()`
- `packages/node-sdk` — 基类声明默认降级（`listCommands` 返回空数组、`runCommand` 抛 `NOT_IMPLEMENTED`），v2 override 走 klient agent facade
- 测试：agent 侧 unit（去重 / 未知名报错 / DI 解析）+ klient 契约对齐 + node-sdk e2e
- **不做**：TUI slash 暴露、插件/技能到 contributed-command 的桥接（上游 0 消费方，YAGNI）

---

## T3 —— SDK 接线 listCommands / runCommand（依赖 T2）

T2 完成后：在 `packages/node-sdk/src/rpc.ts` 补基类声明（默认降级：`listCommands` 返回空数组、`runCommand` 抛 `NOT_IMPLEMENTED`，与 `replaceConfigSections` 同模式），在 v2 客户端 override 走 klient agent facade。

---

## T4 / T5 —— context import（需产品确认）

引擎侧 `IAgentPromptService` 无 import 能力；我们 v1 也无此契约。上游 v1 有，属两仓库产品差异。

若确认需要：T4 在引擎实现（把外部内容作为上下文注入，需定义 source 语义与去重规则），T5 在 SDK 补基类声明与 v2 override。

---

## T6 / T7 —— apply-persisted-secondary-model（需产品确认）

我们 v2 只有 `recheckSecondaryModelWarning`（检查告警），没有"应用已持久化的次要模型"这个动作。

若确认需要：T6 在引擎实现该动作（读持久化的 secondary model 配方并绑定到 spawn 路径），T7 在 SDK 接线。

---

## 通用约束（所有任务）

- 逐包验证 typecheck：`pnpm --filter <pkg> run typecheck`，**不要**用根目录聚合命令（输出会被 rtk 污染成假象）；
- 测试用 Given-When-Then 命名；
- 不为通过测试放宽断言；引擎确实缺能力时记录差异而非伪造对齐；
- 改动完成后按 `gen-changesets` skill 生成 changeset，**不得自行写 major**；
- git 提交需用户确认。
