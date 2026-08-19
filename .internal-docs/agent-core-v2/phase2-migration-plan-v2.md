# Phase 2 迁移计划 v2：借鉴 kimi-code 实践的优化方案

本文档替代 `phase2-migration-plan.md`（v1）。优化基于对 kimi-code 仓库（已完成 v2 迁移）的逆向调研，对比我们原有计划的差异后，采用更务实的迁移模式。

**核心转变**：原计划构建了 7 阶段 + wire-journal 差分回放 + dual 模式 + parity-matrix 的重型基础设施。kimi-code 的实际迁移证明——**method-by-method override + v2→v1 mapper 层 + 简单 env-var 开关**就够了，不需要差分回放基础设施作为前置。

## 修订记录

- 2026-08-10：初版（`phase2-migration-plan.md`）。
- 2026-08-10：基于 kimi-code 逆向调研重写为 v2。

---

## 事实依据

### 我们的状态（mirricode）

| 层面 | 状态 |
|---|---|
| v2 引擎 (`agent-core-v2`) | ✅ Phase 1 完成，21 个 L 特性 + 4 个 G 缺口已移植 |
| v2 服务端 (`kap-server`) | ✅ 服务 `mirri web` 路径 |
| v1 服务端 (`packages/server`) | 🔴 仍在运行，`mirri server run` 全链路依赖 |
| SDK (`node-sdk`) | 🔴 100% v1 绑定，12/13 源文件 import `agent-core` |
| ACP 适配器 | 🔴 100% v1 绑定（直接 + 经 node-sdk 传递） |
| TUI 会话 | 🔴 走 `MirriHarness → node-sdk → server → agent-core`（v1 全链路） |
| e2e-harness | 🔴 只有 v1 fixture |
| `MIRRICODE_SDK_ENGINE` 开关 | ❌ 不存在，原计划设计但未实现 |

### kimi-code 的状态（已完成迁移）

| 层面 | 状态 | 关键做法 |
|---|---|---|
| v2 引擎 | ✅ 默认引擎 | — |
| v1 服务端 (`packages/server`) | ✅ **已删除** | 提前删除，svc/ 移入 kap-server |
| SDK (`node-sdk`) | ✅ 双 backend 共存，默认 v2 | `SDKRpcClientBase` + 两个子类，override-by-method 迁移 |
| SDK 事件类型 | 保持 v1 re-export | v2→v1 mapper 层适配，不改公共契约 |
| ACP | ✅ 新建 v2-native 包 | `packages/acp-server` 直接对接 agent-core-v2 + klient |
| CLI/TUI | ✅ 默认 v2 | `KIMI_CODE_LEGACY_FLAG` env var，opt-out 模式 |
| e2e | ✅ 收入 klient | wire-level test client 合并到 `packages/klient/test/e2e/` |
| `mirri server run` | ✅ 已废弃 | 改为 deprecated shim → `mirri web` |

### kimi-code 迁移时间线（git 历史）

1. v2 引擎 + kap-server 落地，behind experimental flag
2. `packages/server` 删除 + svc/ 移入 kap-server（server 删除先于 SDK 迁移完成）
3. `kimi server` 命令树替换为 `kimi web`，deprecated shim
4. SDK method-by-method 迁移到 agent-core-v2（override 模式）
5. CLI 默认切 v2（`LEGACY_FLAG` opt-out），`kimi acp-v2` → 默认 `kimi acp`

---

## 原计划 vs kimi-code 实践：关键差异

| 维度 | 原计划 | kimi-code 实际做法 | 采纳 |
|---|---|---|---|
| SDK 抽象 | `Engine` interface + `engine-v1.ts` / `engine-v2.ts` | `SDKRpcClientBase`（v1 方法面作为冻结契约）+ 两个子类 | ✅ kimi-code 模式 |
| 事件类型 | SDK 本地定义，不再 re-export v1 | 保持 v1 re-export + v2→v1 mapper 层 | ✅ kimi-code 模式 |
| 引擎开关 | `MIRRICODE_SDK_ENGINE=v1\|v2\|dual` | `LEGACY_FLAG`（opt-out，默认 v2） | ✅ 简化为 opt-out |
| dual 模式 | 离线 wire journal 差分回放 | 不存在 | ❌ 删除 |
| Wire-journal 差分回放 | Phase 2.0 核心前置 | 不使用 | ❌ 删除 |
| Parity matrix | 中心跟踪看板 | 不使用 | ❌ 删除（靠参数化测试替代） |
| v1 server 删除 | 最后一步（Phase 2.6） | SDK 迁移期间就删 | ✅ 提前删除 |
| ACP 迁移 | 单适配器加 engine 抽象 | 新建 v2-native 包 | ✅ 新建包 |
| e2e-harness | 保留 + 加 v2 fixture | 合并到 klient | ✅ 合并 |
| klient/node-sdk 边界 | 需要前置设计决议 | 保留双包，klient=transport，node-sdk=封装 | ✅ 保留双包 |
| 阶段数 | 7 阶段 | 渐进式 | ✅ 简化为 4 阶段 |

### 删除原计划中这些重型基础设施的理由

1. **Wire-journal 差分回放**：kimi-code 没有它也完成了迁移。override-by-method 模式天然隔离了每个方法的迁移风险——一个方法没迁对，只有那个方法的调用出错，不需要全量差分来发现。参数化测试（v1/v2 双跑同一断言）足以覆盖。
2. **dual 模式**：设计初衷是"差分回放的执行引擎"，但差分回放本身不需要。删除。
3. **Parity matrix**：21 个特性已在 Phase 1 逐个移植验证（见 `migration-progress-2026-08-10.md` §2）。Phase 2 的对齐是 SDK 层面的行为等价，参数化测试比矩阵更直接。
4. **SDK 事件类型本地化**：kimi-code 保持 v1 re-export + mapper 层，公共契约零变更，TUI 侧零改动。重新定义事件类型会引入大量不必要 diff。

---

## 指导原则（修订）

1. **v1 类型面是冻结的公共契约**。SDK 对外暴露的事件、Session、RPC 类型保持 v1 形状。v2 引擎的输出通过 mapper 层适配回 v1 形状。迁移完成后如果需要清理 v1 类型痕迹，那是后续独立工作。
2. **Method-by-method override 迁移**。SDK 的 `SDKRpcClientBase` 定义 v1 方法面；`SDKRpcClientV2` 逐方法 override。未 override 的方法 fall through 到 v1 实现（或 loud-fail）。每个方法独立迁移、独立测试、独立 PR。
3. **v1 server 提前删除**。`packages/server` 在 SDK 迁移期间就删除——TUI 会话路径通过 node-sdk 走，不直接依赖 server。删除后 `mirri server run` 改为 deprecated shim 指向 `mirri web`。
4. **引擎开关用 opt-out**。`MIRRICODE_LEGACY_FLAG` 环境变量，设了走 v1，不设走 v2。初始默认 v1，迁移成熟后翻为默认 v2。不引入 `dual` 模式。
5. **ACP 新建 v2-native 包**。参考 kimi-code 的 `packages/acp-server`，直接对接 agent-core-v2 + klient，不走 adapter 改造。旧 `acp-adapter` 作为 legacy 保留到 v1 退役。
6. **每个 PR 自带出口标准**：合并条件 + 回滚方式 + 下一步启动条件。
7. **可回滚**。在默认切 v2 之前，环境变量一行回滚到 v1。

---

## 阶段划分

### Phase A — SDK 双 backend 落地

目标：在 node-sdk 层建立 v2 backend，TUI 零改动可切引擎。

**A1 SDKRpcClientBase 抽象**

- 位置：`packages/node-sdk/src/rpc.ts`
- 现有 `rpc.ts` 已有 `SDKRpcClientBase` 抽象类（v1 方法面）——确认它的完整性，不足处补齐
- 把 `sdk-rpc-client.ts`（v1 实现）中直接耦合 `KimiCore` 的逻辑确保全部走 base class 的 abstract 方法
- base class 的方法签名 = v1 公共契约，迁移期间冻结

**A2 v2→v1 mapper 层**

- 位置：`packages/node-sdk/src/v2/`（新建目录）
- 参考 kimi-code 的 7 个 mapper 文件：
  - `config-mapper.ts` — v2 config → v1 `MirriConfig` 形状
  - `event-mapper.ts` — v2 `DomainEvent` → v1 `Event`
  - `session-mapper.ts` — v2 → v1 `SessionSummary` / `SessionMeta`
  - `session-wiring.ts` — v2 session 生命周期接线
  - `resume-replay.ts` — v2 session resume / replay 适配
  - `import-context.ts` — v2 import context 适配
  - `global-mcp.ts` — v2 MCP 配置映射
- 每个 mapper 独立测试：输入 v2 对象 → 输出 v1 形状 → 断言字段等价

**A3 SDKRpcClientV2 实现**

- 位置：`packages/node-sdk/src/sdk-rpc-client-v2.ts`（新建）
- 继承 `SDKRpcClientBase`，通过 klient 内存 transport 对接 agent-core-v2
- 参考 kimi-code 的迁移注释模式：文件头维护"已迁移方法清单"
- 未 override 的方法 fall through 到 `getRpc()` 并 loud-fail（`NOT_IMPLEMENTED`）
- 工厂函数：`createMirriHarnessV2(options)` → 返回与 `createMirriHarness` 相同的 `MirriHarness` 类型

**A4 引擎开关**

- 位置：`apps/mirri-code/src/cli/experimental-v2.ts`（新建，参考 kimi-code 同名文件）
- `MIRRICODE_LEGACY_FLAG` 环境变量：truthy = v1，默认 = v1（初始阶段）
- `run-shell.ts` 按 flag 选择 `createMirriHarness` 或 `createMirriHarnessV2`
- `mirri web` 始终走 kap-server，不查 flag

**A5 参数化测试**

- 位置：`packages/node-sdk/test/`
- 现有 SDK 测试全部参数化为 v1/v2 双跑
- v2 侧未迁移的方法标记 `skip` 并记录在测试输出中

**Phase A 出口标准**：
- `SDKRpcClientV2` 覆盖核心方法：session 创建 / prompt 发送 / 事件接收 / tool approval / session 结束
- 参数化测试 v1/v2 双跑，核心路径绿，未迁移方法 skip 并列出
- `MIRRICODE_LEGACY_FLAG=0 apps/mirri-code` 能启动 TUI、发送 prompt、收到回复（smoke）
- 回滚：设 `MIRRICODE_LEGACY_FLAG=1` 即回 v1

---

### Phase B — v1 server 退役 + CLI 命令树清理

目标：删除 `packages/server`，`mirri server run` 废弃为 shim。

**B1 svc/ 迁移**

- 位置：`packages/server/src/svc/` → `packages/kap-server/src/svc/`
- service-manager 代码（launchd / systemd / schtasks）移入 kap-server
- 确认 kap-server 能注册 / 卸载 / 查询系统服务

**B2 `mirri server` 命令废弃**

- 位置：`apps/mirri-code/src/cli/sub/server/`
- 参考 kimi-code 的 `deprecated-server.ts` 模式：
  - `mirri server` → 打印废弃提示 + exit 1
  - `mirri server kill` 保留（清理旧版后台进程）
  - `mirri server ps` / `rotate-token` 等合并到 `mirri web` 子命令
- 删除 `server/run.ts`、`daemon.ts`、`lifecycle.ts` 等直接 import `@mirri-ai/server` 的文件

**B3 删除 `packages/server`**

- 删除整个 `packages/server/` 目录
- 更新 `pnpm-workspace.yaml`（glob 自动覆盖）
- 更新 `flake.nix`：`workspacePaths` 和 `workspaceNames` 移除 `server`
- 更新根 `AGENTS.md` Project Map：移除 `packages/server` 描述

**B4 更新依赖引用**

- 全仓 grep `@mirri-ai/server` import，逐个处理：
  - `apps/vis/server` — 迁离 v1 类型（P1-F，可能需要先完成 Phase A 的 v2 wire 稳定）
  - `packages/e2e-harness` — 如果有 server 引用，改为 kap-server 或 klient
  - `apps/mirri-web` 的 e2e devDep — 改为 kap-server

**Phase B 出口标准**：
- `packages/server` 不存在
- `mirri server run` 打印废弃提示并退出
- `mirri web` 全功能正常（启动 / ps / kill / rotate-token）
- `pnpm install` + `pnpm typecheck` 全绿
- 回滚：git revert（本阶段是删除操作，回滚靠 git）

**⚠ 依赖关系**：B4 中 `apps/vis/server` 的迁离可能需要 v2 wire 类型稳定。如果 vis/server 的迁移工作量较大，可拆为 B4a（其余引用清理）+ B4b（vis/server，延后到 Phase C wire 稳定后）。

---

### Phase C — TUI 全功能 v2 验证 + ACP v2-native

目标：TUI 在 v2 backend 下全功能 parity；ACP 新建 v2-native 包。

**C1 TUI method-by-method 迁移验证**

- 不改 TUI 代码（SDK 层切引擎，TUI 跟着切）
- 按 milestone 验证 v2 backend 下每个功能：
  - **M1**：session 启动 / prompt / assistant 消息 / 正常结束（smoke）
  - **M2**：全部 slash 命令 parity（`/skill`、`/plan`、`/goal`、`/cron`、`/web`、`/model` 等）
  - **M3**：approval / question reverse-RPC parity
  - **M4**：session-replay 从 v2 wire journal 恢复
  - **M5**：长会话 soak（compaction / swarm / background / cron 触发）
- 每个 milestone 发现的 gap 回到 Phase A 补 override 对应方法
- M1–M4 是 PR gate；M5 是发布 gate（nightly 跑，累计达标）

**C2 ACP v2-native 包**

- 位置：`packages/acp-server/`（新建）
- 参考 kimi-code 的 `packages/acp-server`：
  - 直接对接 `agent-core-v2` + `klient`
  - `server.ts`、`session.ts`、`start.ts`、`interaction-bridge.ts`
  - 事件映射、approval/question/replay/slash 等模块
- CLI 的 `mirri acp` 命令 lazy-import `acp-server` 的 `runAcpServer`
- 旧 `packages/acp-adapter` 保留为 legacy（`MIRRICODE_LEGACY_FLAG` 下可用）

**C3 补齐 v2 侧功能 gap**

- 以下 gap 在 Phase A/C 验证中可能暴露，提前列出：
  - **P1-C** cron per-agent 语义：`packages/agent-core-v2/src/session/cron/sessionCronServiceImpl.ts`
  - **P1-E** hook `updatedInput` / `additionalContext` wire-in：`packages/agent-core-v2/src/agent/externalHooks/runner.ts`
  - **P1-D** MCP stdio executor：`packages/agent-core-v2/src/mcpCore/client-stdio.ts`
  - **P1-A / P1-B** kap-server meta capabilities + terminals coded error：`packages/kap-server/src/routes/{meta,terminals}.ts`

**Phase C 出口标准**：
- TUI v2 backend 下 M1–M4 全绿
- `packages/acp-server` 能独立启动 ACP 会话
- 功能 gap 全部修复
- 性能：首 token 延迟、tool exec p95 不劣于 v1 基线 +10%
- 回滚：`MIRRICODE_LEGACY_FLAG=1` 回 v1

---

### Phase D — 默认切 v2 + v1 退役

目标：默认引擎改为 v2，删除 v1 引擎包和所有 v1 分支代码。

**D1 默认切 v2**

- `experimental-v2.ts` 中 `isLegacyEnabled` 逻辑反转：默认 v2，`MIRRICODE_LEGACY_FLAG` opt-out
- changeset 用 **minor**（提供 env var 回退路径，公开 API 未变）
- 观察窗口覆盖一个完整发布流程：telemetry、issue tracker、内部 dogfood
- 观察项：崩溃率、tool retry 率、session 中断率
- 如出现 regression：反转默认回 v1

**D2 历史数据兼容验证**

- 至少 3 个上一个大版本的 session records / workspace state 能在 v2 正常加载
- `apps/mirri-desktop` 冒烟通过

**D3 v1 退役**

**仅当** D1 观察期通过才启动。删除顺序按"先删依赖方、后删被依赖方"：

1. 删除 SDK 的 v1 实现（`sdk-rpc-client.ts`）及 v1 类型引用
2. 删除 ACP legacy 适配器（`packages/acp-adapter`）
3. 删除 `apps/mirri-code/src/cli/sub/server/`（如还有残留）
4. 删除 `packages/agent-core`（v1 引擎包）
5. 更新 `flake.nix`：移除 `agent-core`、`acp-adapter` 的 `workspacePaths` / `workspaceNames`
6. 更新根 `AGENTS.md` Project Map
7. 更新公共文档 / 发布说明（按 `gen-docs` 规则）

**changeset major**（唯一需要 major 的 PR；按 `AGENTS.md` 要求用户确认后才写）。

**D4 收尾**

- **P2-A**：删除 `agent-core-v2` 内 `agent-profile` 本地重复实现（`agentFileDiscovery.ts`、`agentRoots.ts`）
- **P2-D**：移除 `legacyStatus` 桥接层（`packages/kap-server/src/services/legacyStatus/`）
- **P2-E**：e2e-harness v2 fixture 收敛为唯一（或合并到 klient）
- `apps/mirri-web` 前端契约改造：直接消费 v2 wire，不走 `legacyStatus` 桥接
- 更新 `design.md` 中已过时的阶段描述

---

## 横切质量机制

| 机制 | 位置 | 触发时机 |
|---|---|---|
| **参数化 e2e 双跑** | `packages/node-sdk/test/` | 每次 PR |
| **Method-by-method 迁移清单** | `sdk-rpc-client-v2.ts` 文件头注释 | 每次方法 override 更新 |
| **性能基线对比** | 独立 CI 任务 | Phase C 每个 milestone 结束 |
| **Dogfood** | 内部 dogfood 池 | Phase C M2 起持续 |
| **长会话 soak** | Nightly job | Phase C M5 |
| **数据迁移兼容测试** | B4-COEX 扩展 | 涉及 records / 索引格式的 PR |

---

## 风险登记

| 风险 | 缓解 |
|---|---|
| SDK 事件类型细节差异导致 TUI 状态错乱 | v1 类型作为冻结契约，v2→v1 mapper 适配；参数化测试兜底 |
| v2 mapper 层遗漏字段 | mapper 层独立测试，每个 mapper 覆盖完整字段集 |
| 长会话跨引擎恢复 | B4-COEX 已验证读向；写向在 D1 前补双向测试 |
| ACP 外部集成 | 新建 v2-native 包，旧 adapter 保留为 legacy 直到 D3 |
| cron per-agent 语义差异 | Phase C P1-C 修完再进 D |
| MCP stdio 缺 | Phase C P1-D 补齐 |
| `apps/mirri-desktop` 未纳入验证 | Phase D D2 冒烟必过 |
| v1 server 提前删除导致 vis/server 挂掉 | B4 拆分：先清理其余引用，vis/server 延后到 Phase C wire 稳定 |

---

## 关键判断

- **不建差分回放基础设施**。kimi-code 没有它也完成了迁移。method-by-method override 天然隔离风险，参数化测试足够。
- **v1 server 提前删**。kimi-code 在 SDK 迁移期间就删了 server，因为 TUI 会话走 node-sdk 不走 server。提前删除减少了维护面。
- **v1 类型面是资产不是负债**。保持 v1 re-export + mapper 层，TUI 侧零改动。重新定义类型会引入大量不必要 diff。
- **ACP 新建包比改造好**。kimi-code 的 `acp-server` 直接对接 v2，结构清晰；改造旧 adapter 要同时处理 v1 类型耦合和 v2 适配，复杂度更高。
- **Phase D 前 v1 一行不删**。还在跑的 v1 是免费行为规范，只要它在，Phase A/C 都有 ground truth。

---

## 关联文档

- `.internal-docs/agent-core-v2/migration-progress-2026-08-10.md` — 起点进度快照
- `.internal-docs/agent-core-v2/design.md` — v2 主设计文档
- `.internal-docs/agent-core-v2/b4-coex-report.md` — B4-COEX 共存验收
- `.internal-docs/agent-core-v2/phase2-migration-plan.md` — 原计划（已被本文档替代）
- kimi-code 仓库（本地 clone，路径略）— 迁移实践参考
  - `packages/node-sdk/src/sdk-rpc-client-v2.ts` — override-by-method 迁移模式
  - `packages/node-sdk/src/v2/` — v2→v1 mapper 层
  - `apps/kimi-code/src/cli/experimental-v2.ts` — 引擎开关
  - `packages/acp-server/` — v2-native ACP 包
