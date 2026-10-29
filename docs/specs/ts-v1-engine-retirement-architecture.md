# TypeScript 内核与 TS CLI 全量退役 — 架构

> **状态**: Approved（随需求 SDR-002）
> **日期**: 2026-10-28
> **关联需求**: [ts-v1-engine-retirement-requirements.md](ts-v1-engine-retirement-requirements.md)

## 1. 目标形态

```
rust/crates/*                    唯一 agent 引擎 + 唯一 CLI（mirri 二进制：web/serve/acp/
                                 print/migrate/session-list/config-show/doctor）
──────────────────────────────────────────────────────
apps/mirri-web                   Web 前端（dev 代理唯一后端 = rust v3）
apps/mirri-desktop (+lite)       桌面壳（spawn rust mirri web）
packages/protocol                wire 纯函数 + 契约 schema（web src/test 消费）
packages/kosong / telemetry      按 ADR-011 审计结论处置
packages/release                 版本锚（@mirri-ai/release）
packages/e2e-harness             缩减为 fake-provider-server（web rust e2e 用）
packages/server-e2e              wire 黑盒客户端（vs rust server）
rust/crates/mirri-core/tests/fixtures/agent-profile/   对拍夹具迁入 rust（原 packages/agent-profile）
```

## 2. ADR-010：删除 `apps/mirri-code`（TS CLI/TUI 壳）与全部引擎链

**决策**：一次性删除 v1 链 + v2 链 + TS CLI 壳（完整清单见需求 §2）。不做"TUI 壳 + NAPI"过渡形态。

**依据**（探索报告核实）：桌面端与发布产物已纯 Rust；install.sh 下载 Rust 二进制；TS SEA 打包在 CI 零引用；`mirri-node` NAPI 无 CI 构建、无分发、SEA 不打包——"NAPI 承载 TUI"路线需要新建整条分发管道，与 ADR-001 的终态（ratatui TUI）冲突，纯沉没成本。

**连带删除**（壳的直属依赖）：`packages/pi-tui`（95 处消费全在 apps/mirri-code）、`packages/node-sdk`（86 处消费全在 apps/mirri-code）、`packages/mirri-node`（NAPI 绑定，唯一消费者 node-sdk 的 rust 路径）。

**功能差集处置**（SDR-002 裁决"随删除丢弃"）：`rotate-token`、多实例端口分配（`MIRRICODE_EXPERIMENTAL_MULTI_SERVER`/`MIRRICODE_DEV_SERVER` 时代脚本）、`mirri vis`。`upgrade`/`update` 自升级命令随壳移除——安装脚本重跑即升级。

## 3. ADR-011：支撑包处置审计（删除前置门禁）

每个"疑似可删"支撑包在物理删除前执行消费者审计（`grep -rn "@mirri-ai/<name>" --include=package.json apps/ packages/ rust/` + src import grep），按结果处置：

| 包 | 预判 | 处置规则 |
|---|---|---|
| `kosong` | apps/mirri-web src 零依赖；主要消费者是 TUI 壳（provider/catalog/headers）与 node-sdk | 壳删除后若无消费者 → 删除；web 的两个内联引用函数若真依赖 kosong 则保留 |
| `telemetry` | 消费者是 apps/mirri-code（8 处）与 desktop 测试 | desktop 若零依赖 → 删除；有依赖 → 保留 |
| `pi-tui` | 95 处全在 apps/mirri-code | 随壳删除 |
| `agent-profile` | src 消费者全在被删引擎；`test/fixtures/` 被 rust CI 消费 | **fixtures 迁 `rust/crates/mirri-core/tests/fixtures/agent-profile/`**，更新 `agent_md_cross_engine_tests.rs:33-36` 与 `agent_list_cross_engine_tests.rs:55-58` 的路径常量；包删除 |
| `e2e-harness` | `fake-provider-server` 被 web rust e2e（fork-streaming、snapshot-contract）消费 | 缩减为 fake-provider-server（+wire-snapshot 如仍被引用）；engine-fixture/golden 删除 |
| `server-e2e` | 无代码消费者；boot 目标本就是真实 server 进程 | 保留；确认 scripts 的 boot 指向 rust 二进制 |

**执行顺序**：先删引擎与壳 → 跑审计 → 再删空巢支撑包（ kosong/telemetry 等），避免一次 grep 快照过期。

## 4. ADR-012：klient 契约对拍的退役

**决策**：`packages/klient` 整包删除；其 `src/contract/` 的 198 方法 schema 对拍测试（node-sdk `rust-engine-contract-e2e.test.ts` 消费）随 node-sdk 删除。契约守护职责由 Rust 侧 `rust/crates/mirri-test-harness/tests/cross_backend_contract.rs`（双后端对拍，以 `mirri-proto` 注册表 + `authoritative-methods.tsv` 为事实源）承接。

**被否决的选项**：*先做 ts-rs/schemars 契约真源迁移*（04 计划 §3.4 原裁决）——TS 侧真源的存在意义随 TS 引擎整体消失而消失；rust 注册表已是运行时事实源，再迁一层生成器是无消费者工程。

## 4b. ADR-013：rust 侧两处夹具数据迁移

**决策**：rust 侧存在两处对 TS 路径的编译期数据依赖，随 TS 包删除做纯数据迁移（不改 rust 产品代码）：

1. **agent-profile 对拍夹具**：`packages/agent-profile/test/fixtures/{cross-engine,cross-engine-list}` → `rust/crates/mirri-core/tests/fixtures/agent-profile/`；更新 `agent_md_cross_engine_tests.rs:33-36` 与 `agent_list_cross_engine_tests.rs:55-58` 的路径常量。
2. **`mirri-proto` 错误码对拍夹具**：`rust/crates/mirri-proto/src/rpc/error.rs:458` 的 `include_str!("../../../../../packages/agent-core/src/errors/codes.ts")`（`#[cfg(test)]` 模块）——将 `codes.ts` 夹具迁至 rust 侧固定路径（`rust/crates/mirri-proto/test-fixtures/error-codes.ts`），`include_str!` 相对路径同步更新。文件内容零改动。

**验收**：`cargo test --workspace --locked` 编译并全绿（验收 #4 的一部分）。

## 5. apps/mirri-web 改造

| 项 | 动作 |
|---|---|
| `package.json` devDeps | 删 `@mirri-ai/kap-server`；保留 `e2e-harness`（fake-provider）、`protocol` |
| `vite.config.ts` | 删 **5 条** alias（`agent-core/session/store`、`agent-core/base/common/event`、`mirri-code-sdk`、`agent-core`、`kaos`；e2e-harness 的 alias 保留——fake-provider 仍在用）；v1/v2 backend preset 删除、白名单收敛为 `'v3'` |
| `test/backend-switch.e2e.test.ts` | 删 v2 断言段与 `startKapServer`（`startServer` import 随 kap-server 删）；保留 v3 默认 + 未知后端拒绝 + WS 握手用例 |
| `test/subagent-projector-e2e.test.ts` | 重写为 fake-provider 驱动 rust 引擎事件（或改为录制事件 fixture）——断言语义不变 |
| `test/fork-streaming.e2e.test.ts` / `snapshot-contract.e2e.test.ts` | 不动（只用 fake-provider + rust 二进制） |
| `src/` 逻辑层 | 零改动（wire 类型本地重实现；`'v1'` 兜底枚举保留——R2.2） |

## 6. 发布/构建链改造

| 项 | 动作 |
|---|---|
| `rust/scripts/release-package.mjs:206-209` | `yazl` 解析源从 `apps/mirri-code/package.json` 改为根 `package.json`（yazl 提升到根 devDeps）；`rust-native-build.yml` 的对应注释同步 |
| `.github/workflows/ci.yml` | typecheck job：删 `apps/mirri-code/tsconfig.json` 循环条目 + **删除 `Typecheck vis-server`/`Typecheck vis-web` 两个显式步骤**；build job 的 `pnpm run build` 语义随包删除自然收缩 |
| 根 `package.json` scripts | 删 `dev:cli`/`dev:cli:marketplace`/`dev:v1`/`dev:v2`/`dev:server`/`dev:web:v2`/**`vis`**/**`build:plugin-marketplace`**；`dev:web` 保留（= rust）；`typecheck` 脚本内的显式 filter（`@mirri-ai/mirri-code`、`vis-server`、`vis-web`）逐一删除 |
| `plugins/`（marketplace.json + official/） | **随壳删除**：消费者是 TS CLI 的插件系统与 `apps/mirri-code/scripts/build-plugin-marketplace-cdn.mjs`；rust 侧零消费（grep 核实）。目录删除 |
| `scripts/check-nix-workspace.mjs` | **锚点 `START_PKG` 从 `@mirri-ai/mirri-code` 改绑 `@mirri-ai/mirri-desktop`**（壳删除后原锚点无法解析）；`nix-build.yml` 的失败提示文案同步。改锚后闭包覆盖面扩大（web/desktop 链），是改善 |
| `postinstall`（fix-node-pty-perms） | node-pty 唯一消费者是 TUI 壳 → 删除脚本 |
| `pnpm-workspace.yaml` | **显式条目手删**：`apps/vis/server`、`apps/vis/web`（非 glob 覆盖，必须显式移除） |
| flake.nix | workspacePaths/workspaceNames 大幅收缩（删除清单）；`fetchPnpmDeps` hash 重算（本地无 nix 时按 AGENTS.md 最低校验并标注待 CI 重算） |
| `.changeset/config.json` | ignore 列表收缩 |
| `.oxlintrc.json` | 删 `"packages/agent-core-v2/"` ignore 条目 |
| `build/`（raw-text-loader） | 消费者仅 apps/mirri-code 与被删引擎包 → 目录删除 |
| `docs/public/install.sh` / `install.ps1` | 无改动（已是 rust 下载链）；npm 引用仅为 legacy tag 前缀剥离逻辑，保留 |

## 7. 文档更新

根 `AGENTS.md`（架构总览改为"Rust 唯一引擎 + web/desktop 前端"；Project Map 全部重写）、`CONTRIBUTING.md`、根 `README.md`（`pnpm dev:cli` 等引用清理）、`rust/README.md`、`apps/mirri-web/AGENTS.md`（v1/v2 preset 段删除）、`.changeset/README.md`、`docs/specs/rust-core-design/plans/04-phase4-ts-retirement.md`（落地注记）、`docs/specs/ts-v1-engine-retirement-context.md`（快照刷新）、`docs/specs/skills-context.md:93`（apps/mirri-code 路径引用）。用户文档（docs/en|zh）：`mirri vis`、TUI 专属命令、`dev:v1/v2` 相关引用清理。

## 8. 测试策略

1. **基线先行**（worktree 内）：`pnpm test`、`pnpm test:e2e`（构建 rust 二进制后）、`cargo test --workspace --locked` 全绿基线。
2. **夹具迁移门禁**：`cargo test -p mirri-core --test agent_md_cross_engine_tests --test agent_list_cross_engine_tests`（fixtures 迁移后）。
3. **终局门禁**：需求 §5 的 9 条验收标准。
4. rust e2e（web 侧）是"web 在 rust core 下不受影响"的直接守护：`backend-switch`（v3 部分）、`fork-streaming`、`snapshot-contract`、`ws-*` 全绿。

## 9. 与 04-phase4 计划的关系

本设计 = 04 计划 ADR-001 全范围的执行版，含三处偏离裁决：①分阶段（A/B/C）改一次性执行（前置已满足）；②`server-e2e` boot 目标本就是真实 server，无"转换"工作量；③klient 契约对拍由 rust `cross_backend_contract.rs` 承接而非迁移真源（ADR-012）。执行后在 04 计划追加落地注记。
