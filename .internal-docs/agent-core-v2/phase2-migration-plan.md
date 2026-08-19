# Phase 2 迁移计划：保证功能一致性与质量

本文档给出 v1 → v2 引擎迁移在 Phase 2（v1 退役阶段）的完整推进计划。基础事实来自 `.internal-docs/agent-core-v2/migration-progress-2026-08-10.md`（2026-08-10 进度快照）与 `.internal-docs/agent-core-v2/design.md`（主设计文档）。

**核心判断**：这场迁移最大的风险不是"某个模块没移植"，而是**行为语义差异在下游积累后集中爆发**。所以整个计划的骨架是——**先建"能测出差异"的能力，再动"会产生差异"的代码**。

## 修订记录

- 2026-08-10：初稿。
- 2026-08-10：第一轮 self-review 修订 17 项（阶段依赖、契约冻结、抽象层前置决议、CI/soak 定位、退役范围等）。
- 2026-08-10：第二轮 self-review —— 修复 6 处文档内部逻辑/结构缺口；移除全部工期预估表述（周数、日期、估算口径等）。

---

## 指导原则

1. **可回滚是每一步的强制约束**。禁止大规模一次性切换。SDK、TUI、ACP 每一层都要经历"引入 v2 backend 但默认 v1 → 双跑对比 → 切默认 → 删 v1"四阶段。
2. **差异先量化再迁移**。在动代码之前，用 wire-journal 差分回放跑通一批 canonical 会话，把 v1↔v2 的"预期差异清单"和"未预期差异清单"分开。未预期差异必须在迁移前修完。
3. **公共契约优先冻结**。SDK 事件类型、ACP 协议、CLI 子命令、wire 协议 —— 这四个是外部可见的边界，迁移过程中不能悄悄改。要改则单独走 breaking change 流程（changeset major，`AGENTS.md` 明确要求用户确认）。
4. **测试基础设施先于重构**。目前 `packages/e2e-harness` 只有 v1 fixture，`node-sdk` 数百处调用点没有可替换的抽象层——在没有这两个东西之前动 TUI 是自杀。
5. **每个 P0 PR 自带出口标准**：明确"合并时必须过的测试"、"如何回滚"、"下一步启动条件"三段说明。
6. **Wire 协议冻结窗口（双跑期）**。从 Phase 2.2 到 Phase 2.6 之间，禁止非兼容 wire schema 变更；确有必要的变更走独立 breaking flow（wire-parity-report 单独一版，双引擎均同步更新，用户确认后合并）。防止 SDK 双 backend 期间一边改 schema、一边跑差分，diff 全是噪声。

---

## 阶段划分

### Phase 2.0 — 差异检测能力（先行）

目标：在动任何迁移代码前，能可靠地回答"v1 和 v2 在场景 X 下行为是否等价"。

**2.0.1 v2 engine fixture 落地**
- 位置：`packages/e2e-harness/src/engine-fixture.ts` 旁边新增 `engine-fixture-v2.ts`（基于 klient 内存 transport）
- 现有 v1 测试全部改为参数化（v1/v2 双跑），并把当前只跑 v1 的用例明确标记
- **本阶段含 harness 抽象层重构**：先把当前 `engine-fixture.ts` 拆成 `engine-fixture-base` + `engine-fixture-v1`，再做 v2 fixture 与测试参数化。这部分工作量不能低估，它是后续所有双跑对比的地基
- 出口：CI 里同一测试文件双引擎跑绿，或明确列出 known-diff

**2.0.2 Wire-journal 差分回放器**

分两步走，避免"边写规范边写实现"两边都返工：

- **2.0.2a Wire 映射规范先出**：在 `.internal-docs/agent-core-v2/wire-mapping-spec.md` 定义"可比字段集"（哪些字段参与 diff）和"已知差异清单"（DI Scope 新字段、`legacyStatus` 拼合差异等允许项）。这份规范先落地，再动回放器。
- **2.0.2b 回放器实现**：新建 `packages/wire-replay-harness`（或放在 e2e-harness 下）
  - 输入：v1 上录制的 20–30 个 canonical session（覆盖：单 agent 对话 / swarm / plan mode / background task / cron / hook / permission approval / compaction / handoff）
  - **录制 canonical session 本身也计入本阶段工作量**（搭建录制环境、固化 journal、并按 2.0.2a 的可比字段集整理），不要只算"写回放器"的功夫
  - 输出：在 v2 上重放同样的 user prompt + tool result 序列后，wire journal 的字段级 diff（按 2.0.2a 规范筛选）
  - 未匹配"固定差异清单"的都是 bug
  - 位置：`.internal-docs/agent-core-v2/wire-parity-report.md`（每次跑更新）

**2.0.3 行为一致性矩阵**
- 位置：`.internal-docs/agent-core-v2/parity-matrix.md`
- 每一行：L1–L18 + G1–G4 + 进度快照中列出的行为一致性 TODO 项 → 对应测试文件 → v1 通过 / v2 通过 / 双跑一致
- 这个矩阵是阶段 2 每次评审的强制看板，红点项即 Phase 2.1 的补齐清单

**2.0.4 性能基线**
- 冷启动、首 token 延迟、tool exec p95、内存峰值 —— 在 v1 上跑一遍存基线
- v2 后续粒度变更必须报对比数字，不允许"感觉差不多"

**Phase 2.0 交付判据**：wire 差分回放跑绿 + 参数化 e2e 双引擎跑绿 + parity-matrix 全部标黄或绿（红 = block）+ **性能基线已入库**（供后续 Phase 的 +10% 阈值对比）。

---

### Phase 2.1 — 补 v2 侧的功能 gap（并行）

在 Phase 2.0 建的看板上会看到红点，这个阶段是把红点变黄/绿。优先级靠前者先做，因为一旦 TUI/ACP 迁上来才发现 v2 语义缺失，成本翻倍。

| 顺序 | 任务 | 位置 | 理由 |
|---|---|---|---|
| 1 | **P1-C** cron per-agent 语义确认 | `packages/agent-core-v2/src/session/cron/sessionCronServiceImpl.ts` | 语义分歧最容易被 downstream 误解，需要设计决策 + 实现 + 迁移测试，别按"小任务"低估 |
| 2 | **P1-E** hook `updatedInput` / `additionalContext` wire-in | `packages/agent-core-v2/src/agent/externalHooks/runner.ts` | 解析了但丢弃的功能，属于"看着像 v1 兼容但其实没有" |
| 3 | **P1-D** MCP stdio executor 未落地 | `packages/agent-core-v2/src/mcpCore/client-stdio.ts` | 生产用得到 |
| 4 | **P1-A / P1-B** kap-server meta capabilities + terminals coded error | `packages/kap-server/src/routes/{meta,terminals}.ts` | 服务端契约完整性 |

**说明**：原计划里的 `P2-A`（`agent-profile` v2 侧本地去重）已移到 Phase 2.7 —— 它属于代码整洁项、非迁移必要路径，插进 Phase 2.1 会挤压真正的 gap 修复。

每项是独立 PR + 独立 changeset + 更新 parity-matrix。**不允许一个 PR 塞多个 gap。**

---

### Phase 2.2 — SDK 抽象层：`node-sdk` 引入 v2 backend（P0-C）

这是整个阶段 2 的**核心转折点**。TUI 和 ACP 难以迁移，是因为它们直接依赖 v1 类型。所以先在 SDK 层建"引擎切换点"，让上层无感。

**⚠ 前置决议：klient / node-sdk 职责边界**

进入 Phase 2.2 之前必须解决一个模糊点：`packages/klient`（v2 的 typed facade）和 `packages/node-sdk`（面向 CLI/TUI 的 harness SDK）在 v2 完全接管后的职责边界。可能路径：

- (a) 保留双包：klient 只做 typed transport（HTTP+WS client），node-sdk 管理 harness/session 生命周期，node-sdk 的 engine-v2 依赖 klient。
- (b) 合并：node-sdk 依赖 klient 作为内部模块，klient 不发布为独立 SDK。
- (c) 倒转：klient 成为对外 SDK，node-sdk 降级为 CLI 内部 harness。

产出：`.internal-docs/agent-core-v2/sdk-engine-interface-design.md` 里包含这份决议，评审通过后才开始 refactor engine 接口。**不做决议直接建 Engine interface，Phase 2.6 删 v1 时必撞返工**。

**结构**：

```
packages/node-sdk/
├── src/
│   ├── index.ts               ← 公共 API（不变）
│   ├── engine/
│   │   ├── engine.ts          ← Engine interface（v1、v2 都实现）
│   │   ├── engine-v1.ts       ← 现在的 MirriHarness → server 逻辑
│   │   └── engine-v2.ts       ← klient → kap-server
│   ├── events.ts              ← 类型改为 SDK 本地定义，不再直接 re-export v1
│   ├── session.ts             ← 走 engine 接口
│   └── ...
```

**关键约束**：

- SDK 对外暴露的**事件类型、Session 形状、RPC 契约**保持字节级兼容 —— 以 v1 现有类型作为公共契约的"事实标准"，engine-v2 输出适配到该契约（可复用 `legacyStatus` 的思路）
- 引擎选择：环境变量 `MIRRICODE_SDK_ENGINE=v1|v2`，默认 v1
- **`MIRRICODE_SDK_ENGINE=dual` 模式仅用于离线回放已录制的 wire journal**，禁止在活会话上跑；不启用于生产 / dogfood 环境。dual 两个引擎并跑会双倍消耗 LLM token、可能触发副作用（真实工具执行、shell、文件写入），diff 记录本身也有开销。它是"差分回放的执行引擎"，不是"用户 canary 手段"。
- 参数化的 SDK 测试全部双跑

**出口标准**：
- 所有 SDK 测试双引擎跑绿
- `apps/mirri-code` 一行代码不改，`MIRRICODE_SDK_ENGINE=v2` 下 `pnpm test` 全绿
- wire 差分回放跑绿

**回滚方式**：不改默认值即可回滚；engine-v2 有致命 bug 就走 `MIRRICODE_SDK_ENGINE=v1`。

**建议**：本阶段产出物即上面的职责边界决议 + Engine 接口设计（`.internal-docs/agent-core-v2/sdk-engine-interface-design.md`），评审通过再进入 TUI 侧。这是整个链路的"契约面"，设计错了后面全部返工。

---

### Phase 2.3 — TUI 迁移（P0-A）

SDK 双 backend 就位后，TUI 层的迁移**不改 import**，只保证 v2 backend 下每个功能都工作。因此本阶段是"验证/修复驱动"而非"重写驱动"。

**里程碑**（每个是可单独回滚的 milestone）：

- **M1**：v2 下能启动 session、发送 prompt、接收 assistant 消息、正常结束（smoke）
- **M2**：全部 slash 命令 parity（`/skill`、`/plan`、`/goal`、`/cron`、`/web`、`/model` 等）
- **M3**：approval / question reverse-RPC parity（权限对话框、AskUserQuestion 后台任务）
- **M4**：session-replay 从 v2 wire journal 恢复正常
- **M5**：长会话 soak（4 小时连续对话，含 compaction、swarm、background、cron 触发），无泄漏、无 hang。**以 nightly job 方式跑，累计满足通过标准即视为通过；不阻塞 PR 合并（PR gate 是 M1–M4 的 e2e 双跑绿）。soak 是发布前置 gate，不是 PR gate。**

**关键工具**：
- **复用 `MIRRICODE_SDK_ENGINE`，不引入独立 TUI flag**。TUI 层不做二次分流 —— SDK 层能切，TUI 就跟着切；避免 SDK=v2 而 TUI=v1 这类错配组合。
- **每个 milestone 后进入 canary 验证**：内部 dogfood，同一 session 每次启动随机 50% 走 v1、50% 走 v2（环境变量注入，不是 dual 模式），日志聚合对比差异
- 同步推进 parity-matrix 更新

**出口标准**（M5 完成后）：
- 内部 dogfood 期间无 severity-high 的 v2-specific 回归 issue（严重度口径与风险登记一致）
- CI 里 TUI 相关 e2e 双引擎全绿
- 首 token 延迟、tool exec p95 相对基线不超过 +10%

---

### Phase 2.4 — ACP 适配器迁移（P0-D）

ACP 是**外部协议**，客户可能有既有集成。因此：

- 结构同 SDK：`packages/acp-adapter` 引入 engine 抽象
- 录制历史 ACP 对话（含 `authenticate`、`prompt`、`session/update`、`file_open` 等全部方法）作为 golden 数据集，位置 `packages/acp-adapter/test/golden/`
- **Golden 回放必须 mock LLM 层**：同一份 LLM response fixture 同时喂给 v1 和 v2 engine，diff 只针对 engine 侧输出（工具调用序列、事件流、Session 状态）。不 mock 会引入 LLM 非确定性，diff 全是噪声，测试白写。
- v2 backend 重放，字段级 diff，全部差异有明确说明或修复
- 先在 CI 双引擎验证通过，再切默认
- **ACP 默认切换与 Phase 2.5 的 SDK 切换同步进行**（避免"SDK 已 v2、ACP 仍 v1"的历史分叉窗口）

---

### Phase 2.5 — 默认引擎切换与烧机

- 把 SDK 的 `MIRRICODE_SDK_ENGINE` 默认改为 v2
- 保留 v1 分支代码不删除，用户可用环境变量回滚
- **前置 check 1**：长会话跨引擎**双向写**验证通过（v1 写 v2 读、v2 写 v1 读）—— 从 B4-COEX 已验的"读向"扩展到"写向"
- **前置 check 2**：ACP 默认 backend 同步 v2 就绪（见 Phase 2.4）
- 发布方式：**changeset 用 minor**（因为提供 `MIRRICODE_SDK_ENGINE=v1` 环境变量回退路径，公开 API 未变，故按 minor）；**若变更引入无法用环境变量回退的行为差异（例如默认 tool 行为不同、wire schema 外扩等），则升 major 并按 `AGENTS.md` 要求用户确认**
- 观察窗口覆盖一个完整发布流程 + 升级/回滚/长期运行三类场景：telemetry、issue tracker、内部 dogfood
- 观察项：崩溃率、tool retry 率、session 中断率、AI 生成质量（人工抽检 canary 会话，抽样比例与判定标准在烧机启动时确定）
- 如出现 regression：拉回默认 v1，回到对应 milestone 修

**出口标准**：
- 无明显回退性 issue（severity 口径同风险登记），无 v2-specific 性能倒挂
- 性能基线达标，parity-matrix 全绿
- **历史数据兼容测试通过**：至少 3 个上一个大版本的用户 dump（真实 session records / workspace state）能在 v2 正常加载和继续
- **`apps/mirri-desktop` 冒烟通过**（其 SDK 依赖形状在 Phase 2.0 阶段确认）

---

### Phase 2.6 — v1 退役（P0-B）

**仅当** Phase 2.5 出口达成才启动。

**本阶段要删的是"v1 引擎包 + v1 服务端 + v1 分支代码"三个层面，不只是服务端**。**删除顺序按"先删依赖方、后删被依赖方"执行**，每删一批过一遍 typecheck + 测试，避免中间态编译失败：

1. 删除 SDK 的 engine-v1 分支及 v1 类型引用
2. 删除 ACP 适配器 v1 backend
3. **`apps/vis/server` 迁离 v1 类型（P1-F）** —— 必须在删 `packages/agent-core` 前完成，否则它挂掉；建议赶在 Phase 2.5 期间做掉
4. 删除 `apps/mirri-code/src/cli/sub/server/`（7 个文件）；`mirri server run` 命令要么废弃（打印迁移提示后退出），要么内部改指 kap-server
5. 删除 `packages/server`
6. **删除 `packages/agent-core` 本身**（v1 引擎包，最容易被漏掉；只有 v1 引擎也删掉，依赖图 / `flake.nix` / Project Map 才干净）
7. 更新 `flake.nix`（`workspacePaths` / `workspaceNames` 移除 `server` 与 `agent-core`；注意同步 `engine` 名称列表）、根 `AGENTS.md` Project Map（P2-C）
8. 更新公共文档 / 发布说明（`docs/`，按 `gen-docs` 规则同步 CLI 文档）

**changeset major**（唯一需要 major 的 PR；`AGENTS.md` 明确要求用户确认后才写）。

---

### Phase 2.7 — 收尾

- **P2-A**（从 Phase 1 移来）：`agent-core-v2` 内 `workspaceAgentProfileLoader` 本地去重删除
  - 位置：`packages/agent-core-v2/src/workspace/workspaceAgentProfileLoader/internal/{agentFileDiscovery,agentRoots}.ts`
- **`apps/mirri-web` 前端契约改造**：v1 已死，web 直接消费 v2 wire，不再走 `legacyStatus` 桥接
  - 位置：`apps/mirri-web/src/`（事件类型 / RPC 契约相关模块，需专项盘点）
- P2-B：`flake.nix` 的 `agent-profile` 注册确认（如 P2-A 未顺手处理）
- P2-D：移除 `legacyStatus` 桥接层（届时 v1 已不存在，web 直接消费 v2 wire）
- P2-E：`e2e-harness` v2 fixture 从"与 v1 并行"收敛为"v2 唯一"，删除 v1 分支
- P2-F：capability cleanup —— 注意该计划本身在 v1 侧，v1 已退役则归档
- 更新 `design.md` 中已过时的阶段描述，替换为"已完成"总结

---

## 横切质量机制（每个阶段都要跑）

| 机制 | 位置 | 触发时机 |
|---|---|---|
| **Wire journal 差分回放** | `packages/wire-replay-harness` | 每次 v2 侧改动 PR + 每日 CI |
| **参数化 e2e 双跑** | `packages/e2e-harness` | 每次 PR |
| **Parity-matrix 看板** | `.internal-docs/agent-core-v2/parity-matrix.md` | 每个 milestone 结束 |
| **性能基线对比** | 独立 CI 任务 | 每个 P0 milestone 结束 |
| **Golden ACP 回放** | `packages/acp-adapter/test/golden/` | ACP 相关 PR |
| **Dogfood 池** | 内部 dogfood 池 | Phase 2.3 M2 起持续 |
| **数据迁移双向测试** | B4-COEX 的扩展 | 每次涉及 workspaces / 索引 / 记录格式的 PR |
| **长会话 soak** | Nightly job | 每晚跑，累计成功达标视为通过；发布前提，非 PR 阻塞项 |

---

## 风险登记（每个 P0 PR 描述里逐个应答）

| 风险 | 缓解 |
|---|---|
| SDK 事件类型细节差异导致 TUI 状态错乱 | v1 类型作为 SDK 契约事实标准，engine-v2 适配；差分回放兜底 |
| 长会话跨引擎恢复（v1 起、v2 恢复） | B4-COEX 已验证读向；**写向**在 Phase 2.5 前补双向双写验证（已列入阶段 2.5 前置 check） |
| ACP 外部集成 | 记录 golden 数据集，双引擎窗口回放 |
| cron per-agent 语义差异 | Phase 2.1 的 P1-C 项修完再往下走 |
| 性能回归（v2 多一层 DI 包装） | 有基线 + 阈值卡位 |
| MCP stdio 缺 | Phase 2.1 P1-D 补齐后再让 TUI 依赖 |
| user config 与 profile 格式 drift | agent-profile 统一（Phase 2.7 P2-A）+ v1/v2 兼容窗口保持到 2.6 |
| `apps/mirri-desktop` 未纳入切换验证 | 阶段 2.0 确认其依赖形状；Phase 2.5 冒烟必过 |
| klient / node-sdk 边界不清 | Phase 2.2 前置决议文档，评审通过才动 refactor |
| wire schema 被动改 | 指导原则 6 冻结窗口 + 未预期 diff 报警 |

---

## 关键判断

- **不跳过 Phase 2.0**。大规模迁移失败项目共同特征就是"测试基础设施浪费时间"。2.0 的投入会在 2.3 / 2.4 连本带利赚回。
- **Phase 2.2 是核心架构决策**。Engine 接口与 klient/node-sdk 边界不议清，2.3 反复回炉。
- **dual 模式是"离线差分"不是"canary"**。canary 用随机 50/50 环境变量分流，dual 只在 CI 回放已录制 journal，二者勿混淆。
- **Phase 2.6 前 v1 一行不删**。还在跑的 v1 是免费行为规范，只要它在，2.2 / 2.3 / 2.4 都有 ground truth。
- **退役目标＝`packages/agent-core` + `packages/server`**，不是只删 server。不写清这行，执行中很容易只删一半。

---

## 关联文档

- `.internal-docs/agent-core-v2/migration-progress-2026-08-10.md` — 起点进度快照
- `.internal-docs/agent-core-v2/design.md` — v2 主设计文档
- `.internal-docs/agent-core-v2/b4-coex-report.md` — B4-COEX 共存验收
- `.internal-docs/agent-core-v2/wire-parity-report.md` — 阶段 2.0.2 产出（每次跑更新）
- `.internal-docs/agent-core-v2/wire-mapping-spec.md` — 阶段 2.0.2a 产出
- `.internal-docs/agent-core-v2/parity-matrix.md` — 阶段 2.0.3 产出（评审看板）
- `.internal-docs/agent-core-v2/sdk-engine-interface-design.md` — 阶段 2.2 前置产出
- `packages/agent-core-v2/README.md` — v2 引擎入口（索引到本计划与设计文档）