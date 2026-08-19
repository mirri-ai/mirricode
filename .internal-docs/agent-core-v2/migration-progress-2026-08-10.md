# v1 → v2 迁移进展快照（2026-08-10）

本文档是 v1 (`@mirri-ai/agent-core` + `@mirri-ai/server`) → v2 (`@mirri-ai/agent-core-v2` + `@mirri-ai/kap-server` + `@mirri-ai/klient`) 迁移在 **2026-08-10** 的进度快照，覆盖依赖对接、功能对齐、CLI/TUI 双路径与剩余任务清单，作为 Phase 2 启动前的现状记录。

- 上下文源头：`.internal-docs/agent-core-v2/design.md`（722 行主设计文档）、`.internal-docs/agent-core-v2/b4-coex-report.md`（B4 共存验收）、`plan/agent-profile-unification.md`、`plan/capability-cleanup-and-expand.md`。
- 本文只补记**已完成 vs 未完成**的对齐结论，不重复 `design.md` 的设计推导。

## 1. 依赖对接现状

按引擎归属汇总（package.json + 源码 import 双向核对）：

| 包 / 应用 | 引擎 | 关键路径 |
|---|---|---|
| `packages/agent-core` | v1 自身 | — |
| `packages/agent-core-v2` | v2 自身 | — |
| `packages/server` | **v1** | `mirri server run` 后端；全量 `import '@mirri-ai/agent-core'` |
| `packages/kap-server` | **v2** | `mirri web` 后端；全量 `import '@mirri-ai/agent-core-v2'` |
| `packages/node-sdk` (= `@mirri-ai/mirri-code-sdk`) | **v1** | events/types/session/rpc/mirri-harness 全部 v1 绑定 |
| `packages/acp-adapter` | **v1** | 9 个 src + 测试均依赖 v1 Session/approval |
| `packages/klient` | **v2** | v2 typed facade（IPC / 内存 transport） |
| `packages/e2e-harness` | **v1** | `engine-fixture.ts` 只有 v1 版本 |
| `apps/mirri-code` (CLI/TUI) | **双路径** | `mirri server run` → v1；`mirri web` → v2；TUI 会话本体仍跑 v1 SDK |
| `apps/mirri-web` | **v2** | 仅经 kap-server；`@mirri-ai/server` 只出现在 e2e devDep |
| `apps/vis/server` | **v1** | 直接 re-export `@mirri-ai/agent-core` 类型 |

关键结论：**v2 引擎层已建立并跑通 `mirri web` 路径；v1 服务层（server + node-sdk + acp-adapter + TUI 会话）完全未开始迁移。**

## 2. 功能对齐差距

设计文档 §3.3/§3.5 列出的 21 个 mirri 特有能力（L1～L18 + 相关 G 缺口），当前状态：

| 能力 | v1 位置 | v2 对应 | 状态 |
|---|---|---|---|
| flags/registry | `src/flags/` | `app/flag/` | ✅ 已移植 |
| agent profile | `src/profile/` + `services/profile/` | `workspace/workspaceAgentProfileLoader/` + `app/agentProfileCatalog/` | ✅ 已移植（P2-A 有清理尾巴）|
| permission policies (11) | `agent/permission/policies/` | `agent/permissionPolicy/policies/` | ✅ 已移植 |
| plan | `agent/plan/` | `agent/plan/` | ✅ 已移植 |
| compaction (micro/full/handoff) | `agent/compaction/` | `agent/fullCompaction/` + `agent/contextMemory/` | ✅ 已移植 |
| background (persist/process-task/question) | `agent/background/` | `agent/task/` + `session/process/` | ✅ 已移植 |
| cron | per-agent `agent/cron/` | session 级 `session/cron/` | ⚠️ **部分对齐**（见 P1-C） |
| goal | `agent/goal/` | `agent/goal/` | ✅ 已移植 |
| swarm | `agent/swarm/` | `agent/swarm/` + `session/swarm/` | ✅ 已移植 |
| hooks (mcp + session hooks) | `mcp/` + `session/hooks/` | `agent/externalHooks/` + `session/externalHooks/` | ✅ 已移植（但 `updatedInput`/`additionalContext` 未 wire，见 P1-E） |
| plugin marketplace/injections | `plugin/` | `app/plugin/` | ✅ 已移植 |
| skill (6 builtin + discovery) | `skill/` + `agent/skill/` | `app/skillCatalog/` + `session/sessionSkillCatalog/` + `agent/skill/` | ✅ 已移植 |
| capability registry | `agent/tool/capabilities/` | `agent/capability/` + `agent/capabilityGap/` | ✅ 已移植 |
| image compress/format | `tools/support/image-*` | `agent/media/` | ✅ 已移植 |
| services layer | `services/` (23 subdirs 平铺) | 4-tier `LifecycleScope` 内各 domain service | ✅ **架构重写** |
| records / wire journal | `agent/records/` + migrations | `wire/` + `agent/blob/`，migrations v1.1–v1.5 | ✅ 已移植 |
| G1: unexecuted tool-call recording | — | `agent/loop/loopService.ts:975` (`recordUnexecutedToolCalls`) | ✅ 已补 |

**v2 新增、v1 没有的概念**：`_base/di/scope.ts` 的 4-tier Scope（App/Workspace/Session/Agent）、`app/sessionIndex/sqliteSessionIndex`、`workspace/workspaceTrust`、`workspace/workspaceMcpConfig`、`agent/toolActivation` / `toolDedupe` / `toolResultTruncation`、`agent/capabilityGap`、`app/hostFolderBrowser`。

### 2.1 已移植、但仍带 TODO / legacy 标记

| 位置 | 说明 |
|---|---|
| `packages/kap-server/src/routes/meta.ts:9` | meta endpoint 未声明全部 capabilities（WebSocket / file upload / fs query / mcp / terminal） |
| `packages/kap-server/src/routes/terminals.ts:248` | `TODO: push a coded error into assertAllowed` |
| `packages/kap-server/src/services/legacyStatus/legacyStatus.ts` | 临时桥接层，拼合多个 v2 service 输出成 v1 `agent.status.updated`；等 wire contract 演进后移除 |
| `packages/agent-core-v2/src/mcpCore/client-stdio.ts:52` | 部分 MCP stdio executor `NOT_IMPLEMENTED` |
| `packages/agent-core-v2/src/app/event/eventBus.ts:11` | 部分事件（model catalog / session lifecycle / auth）仍走 legacy 路径 |
| `packages/agent-core-v2/src/agent/task/configSection.ts:6` | legacy config 兼容窗口，等 caller 迁完 |
| `packages/agent-core-v2/src/app/config/config.ts:17` | 旧 config key 重命名兼容 |
| `packages/agent-core-v2/src/app/sessionIndex/sqliteSessionIndex.ts:~103` | legacy file index fallback 保留 |

## 3. CLI / TUI 侧的双路径

**不是运行时 flag 分支**，而是两条独立子命令共存：

- **v1 路径**：`apps/mirri-code/src/cli/sub/server/`（7 个文件全部 `import '@mirri-ai/server'`）
  - `run.ts:16`、`daemon.ts:25`、`kill.ts:19`、`ps.ts:12`、`lifecycle.ts:20`、`rotate-token.ts:9`、`shared.ts:11`
  - `server/index.ts:45` 注释明确 `mirri server run` 留在 v1 server
- **v2 路径**：`apps/mirri-code/src/cli/sub/web/`（3 个文件 `import '@mirri-ai/kap-server'`）
  - `run.ts:14`、`rotate-token.ts:9`、`shared.ts:10`
- **TUI 探测已运行 v2**：`apps/mirri-code/src/tui/commands/web.ts:25` 注释 + `findRunningV2Server()`（L71–123）通过 `/api/v1/meta` 的 `backend === 'v2'` 判断是否复用
- **SDK 层**：`packages/node-sdk/src/{events,types,session,mirri-harness,sdk-rpc-client,rpc}.ts` 全部绑定 v1。**没有并行 v2 双文件**。
- TUI 会话链路仍是 `MirriHarness → @mirri-ai/mirri-code-sdk → @mirri-ai/server → agent-core`。
- 无 `useV2` / `engineVersion` 运行时开关；`MIRRICODE_DEV_SERVER=1` 仅控制 dev 模式，不切换引擎。

## 4. 服务端归属

- `packages/server`：`name: @mirri-ai/server`、`private: true`。README 明说"hosts agent-core sessions"，被 `mirri server run` 和 TUI 会话全链路依赖，`apps/mirri-web` 的 e2e 也用它冒烟。**尚未退役。**
- `packages/kap-server`：仅被 `apps/mirri-code/src/cli/sub/web/` 三个文件消费，服务 `mirri web` 和 `apps/mirri-web`。
- 根 `AGENTS.md:26` 附近的 Project Map 仍描述 `packages/server` 为 "the Mirri Code server"，未反映 kap-server 已并列存在，需要在 P2 清理时更新。

## 5. 计划文档现状

| 文档 | 摘要 | 与本次迁移的关系 |
|---|---|---|
| `.internal-docs/agent-core-v2/design.md` | Phase 1 = 机械移植 + 功能对齐；Phase 2 = TUI 迁移 / ACP 迁移 / SDK 过渡 / packages/server 下线 | Phase 1 完成，Phase 2 未启动。文档里"pnpm dev:v2 只跑 v1 server"的注释已过时（现在真启的是 kap-server）|
| `.internal-docs/agent-core-v2/b4-coex-report.md` | B4-COEX 验收（2026-08-01）：workspaces.json 双写、wire 协议 v1.5 跨恢复、并发写锁全部通过 | v1/v2 共存基础已验证 |
| `plan/agent-profile-unification.md` | 抽出 `@mirri-ai/agent-profile` 纯函数包 | 包已建（9 src + 6 test），v1 全量接入，v2 只接了 `agentFile.ts` + `types.ts`；`agentFileDiscovery.ts`（7KB）和 `agentRoots.ts`（4.3KB）仍是本地实现。计划文档 checkbox 未同步。|
| `plan/capability-cleanup-and-expand.md` | 清理幽灵 `code.navigate`、给 Read/WebSearch/FetchURL 补 capability 声明 | v1 侧独立清理，与 v2 迁移平行 |

## 6. 剩余迁移任务清单

### P0 — 阻塞 v1 下线（Phase 2 主线，按依赖顺序）

| # | 任务 | 位置 | 工作量 | 依赖 |
|---|---|---|---|---|
| **P0-C** | `packages/node-sdk` (`@mirri-ai/mirri-code-sdk`) 脱离 v1：改造为 klient 薄包装或整体退役；events / Session / MirriHarness / RPC 类型全部要重做 | `packages/node-sdk/src/`（17 文件），`apps/mirri-code` 数百处 import | 大 | klient 已具备基础 |
| **P0-A** | TUI 会话路径迁移到 v2：用 klient 替换 `MirriHarness → packages/server` 链；重写 session-event-handler、session-replay、approval/question reverse-RPC、slash 命令的 SDK 绑定 | `apps/mirri-code/src/cli/run-shell.ts`、`src/tui/controllers/session-event-handler.ts`、`src/tui/controllers/session-replay.ts`、`src/tui/reverse-rpc/{approval,question}/` | 大（Phase 2 单项最大） | P0-C |
| **P0-D** | `packages/acp-adapter` 迁移到 v2：Session / approval 类型全绑定 v1，需重接 v2 接口 | `packages/acp-adapter/src/`（9 src + 测试） | 大 | P0-A（v2 approval 接口稳定后） |
| **P0-B** | `packages/server` 正式退役：删除包、废弃或重接 `mirri server run` 子命令、同步 `flake.nix` 与 Project Map | `packages/server/` 全包、`apps/mirri-code/src/cli/sub/server/` 7 文件 | 中 | P0-A、P0-D |

### P1 — 功能对齐 / 剩余 gap

| # | 任务 | 位置 | 工作量 |
|---|---|---|---|
| P1-A | kap-server meta 路由声明全部 capabilities | `packages/kap-server/src/routes/meta.ts:9` | 小 |
| P1-B | kap-server terminals 路由 coded error 补全 | `packages/kap-server/src/routes/terminals.ts:248` | 小 |
| P1-C | 确认 v2 cron 的 per-agent 语义，或补上 `agentId` 过滤 | `packages/agent-core-v2/src/session/cron/sessionCronServiceImpl.ts` | 小～中 |
| P1-D | MCP stdio executor `NOT_IMPLEMENTED` 分支实现 | `packages/agent-core-v2/src/mcpCore/client-stdio.ts:52` | 中 |
| P1-E | Hook `updatedInput` / `additionalContext` 结果 wire 进 tool execution | `packages/agent-core-v2/src/agent/externalHooks/runner.ts` | 中 |
| P1-F | `apps/vis/server` 从 v1 类型迁走 | `apps/vis/server/src/lib/agent-record-types.ts`、`context-projector.ts` | 中（等 v2 wire 稳定） |

### P2 — 清理与文档

| # | 任务 | 位置 | 工作量 |
|---|---|---|---|
| P2-A | 删除 v2 里 agent-profile 的本地重复实现，全部接入 `@mirri-ai/agent-profile` | `packages/agent-core-v2/src/workspace/workspaceAgentProfileLoader/internal/agentFileDiscovery.ts`、`agentRoots.ts` | 中 |
| P2-B | `flake.nix` 确认 `agent-profile` 已加入 `workspacePaths` / `workspaceNames` | `flake.nix` | 小 |
| P2-C | 根 `AGENTS.md` Project Map 更新，明确 v1/v2 服务端并列现状 | `AGENTS.md:26` 附近 | 小 |
| P2-D | 移除 `legacyStatus` 临时桥接层 | `packages/kap-server/src/services/legacyStatus/` | 中（等 wire contract 演进 + web 前端消费分片事件） |
| P2-E | `packages/e2e-harness` 增加 v2 engine fixture | `packages/e2e-harness/src/engine-fixture.ts` | 中 |
| P2-F | 执行 `capability-cleanup-and-expand.md`（清理 `code.navigate`、补 Read/WebSearch/FetchURL 声明） | `packages/agent-core/src/loop/types.ts:159`、`test/capabilities.test.ts` | 小 |

## 7. 整体进度

| 阶段 | 状态 |
|---|---|
| Foundation（F1–F4） | ✅ 完成 |
| Feature Batch B0–B4（21 个 L 特性 + 4 个 G 缺口） | ✅ 完成 |
| B4-COEX 共存验证 | ✅ 通过（2026-08-01） |
| **Phase 2（v1 退役）** | 🔴 **未启动**，四个 P0 任务是主线 |

**下一步的关键判断**：v2 引擎"可用且已在 `mirri web` 落地"，但 v1 服务层生态（server + node-sdk + acp-adapter + TUI）没有任何一个开始迁移，`packages/server` 短期内会持续存在。真正的下一步是**先动 SDK（P0-C）**——TUI 和 acp-adapter 都长在 node-sdk 上，SDK 不脱离 v1，其余 P0 任务都启动不了。
