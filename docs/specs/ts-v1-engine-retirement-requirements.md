# TypeScript 内核与 TS CLI 全量退役（v1 + v2 + TUI 壳）

> **文档类型**：设计（需求侧）。
> **状态**: Approved（用户裁决 2026-10-28：引擎+壳全删；rotate-token/vis 随删除丢弃）
> **日期**: 2026-10-28
> **关联文档**: [rust-core-design/00-overview.md](rust-core-design/00-overview.md) §7 ADR-001（全量退役路线）、[rust-core-design/plans/04-phase4-ts-retirement.md](rust-core-design/plans/04-phase4-ts-retirement.md)（原全量退役计划）、[ts-v1-engine-retirement-context.md](ts-v1-engine-retirement-context.md)（现状快照）

## 0. 背景与最终裁决（SDR 记录）

**SDR-001（2026-10-28，superseded by SDR-002）**：原裁决为"仅退役 v1 引擎链（Stage A），保留 v2 引擎与 TS 前端"——设计文档 `2e402edef` 提交了 Stage A 版本（ADR-002..009）。该裁决在同日被用户范围升级取代。

**SDR-002（2026-10-28，生效）**：ADR-001 于 2026-10-12 已裁决退役范围从「TS 引擎」扩大为「整个 TS CLI 面」，并设硬前置「Rust 二进制先完成分发接管」。截至本设计日，前置条件经核实**已满足**：

- 桌面端打包与运行时**已完全 Rust 化**（打包 `cargo build -p mirri-cli`，运行时 spawn `mirri web`，零 TS 引擎依赖）。
- 发布产物**已是纯 Rust**：GitHub Release 上的 CLI zip 只含 `mirri` 二进制；ACP 已折叠进主 CLI（`mirri acp`）；install.sh 从 GitHub Release 解析 `v*` 标签并下载 Rust 二进制。TS SEA 打包脚本在 CI 中零引用。
- Web 开发代理默认后端**已是 Rust（v3）**。

用户裁决（SDR-002，2026-10-28）：**v1 引擎、v2 引擎、TS CLI/TUI 壳（apps/mirri-code）一并退役**，不做"保留 TUI 壳 + NAPI"的过渡形态；`rotate-token`、多实例端口分配、`mirri vis` 三个 TS 独有能力**随删除丢弃**（后续按需在 Rust 侧重建）。本裁决对应 ADR-001 的完整范围一次性落地。

退役后仓库 TypeScript 面收敛为：**web 前端、desktop 壳、协议/工具支撑包**——不含任何 agent 引擎实现。

## 1. 需求（Requirements，产品语言）

### R1：命令行用户迁移到统一的原生 CLI

- **R1.1** 用户安装后得到的是单一原生可执行文件（`mirri`），提供：启动 Web 服务（`mirri web` / `mirri serve`）、IDE 集成（`mirri acp`）、单次问答（`mirri print`）、历史会话迁移与列表（`mirri migrate` / `mirri session-list`）、配置查看与校验（`mirri config-show` / `mirri doctor`）。这些均已存在且经测试守护。
- **R1.2** 旧 TS CLI 的交互终端界面（TUI）从仓库移除。终端用户的使用入口收敛为 Web 与桌面应用（二者已默认连接 Rust 内核）。
- **R1.3** 三个 TS 独有能力（服务令牌轮转命令、多服务实例端口自动分配、会话可视化工具）不再提供。若未来用户提出需求，在 Rust 侧按需重建。
- **R1.4** 历史会话不丢失：旧引擎创建的会话通过已有的 `mirri migrate` 命令带入新存储（该能力已存在并由 Rust 侧测试守护）。

### R2：Web 与桌面在 Rust 内核下功能不受影响

- **R2.1** Web 的全部现有功能、开发流程（默认 Rust 后端、可切换的对照后端已随 v2 删除而消失，仅保留 Rust）照常运转。
- **R2.2** Web 的"未知/老服务检测"兜底语义保留（对老版本服务器的兼容检测）。
- **R2.3** Web 的现有全部测试在变更后照常通过，包括连接真实 Rust 内核的端到端测试；原依赖 TS v2 引擎做对照的测试用例随 v2 删除。

### R3：安装与升级流程不回退

- **R3.1** 新用户通过安装脚本获得原生二进制的流程不变；升级通过发布新版本完成（原 TS CLI 的自升级命令随壳移除，安装脚本重跑即为升级路径）。
- **R3.2** 桌面端安装包的构建与自更新流程不变（已不依赖任何被删组件）。

### R4：开发者与贡献者体验

- **R4.1** 仓库构建、测试、代码检查流程在变更后照常通过；与被删代码相关的检查项随其一起消失。
- **R4.2** 会话可视化工具（vis）从仓库移除后，`mirri vis` 命令入口同步消失，不留断链入口。
- **R4.3** 文档与仓库说明同步更新：架构描述从「TS 引擎 + 迁移中」改为「Rust 唯一引擎」。

### R5：对用户的告知

- **R5.1** 这是用户可感知的重大变更（交互终端入口移除），需要按发布流程生成变更记录，级别为 **major**（移除已分发的命令入口与交互形态），用户裁决已确认。

## 2. 保留与删除清单（裁决记录）

### 保留（消费者已核实）

| 对象 | 保留理由 |
|---|---|
| `apps/mirri-web` | Web 前端（本地重实现 wire 类型；`@mirri-ai/protocol` 仅 2 个纯函数消费点） |
| `apps/mirri-desktop`、`apps/mirri-desktop-lite` | 桌面壳（已纯 Rust 后端） |
| `packages/protocol` | web 前端内联引用解析 + 契约 schema；server-e2e 依赖 |
| `packages/kosong` | web src 不依赖，但 model catalog / provider 元数据被谁用需最终确认——见 ADR-011 |
| `packages/telemetry` | desktop 无依赖面残留、TS CLI 删除后消费者待最终审计——见 ADR-011 |
| `packages/release` | 版本锚（changesets + rust sync-version） |
| `packages/e2e-harness` | 缩减为 fake-provider-server（被 web 的 Rust e2e 使用） |
| `packages/server-e2e` | wire 黑盒客户端 vs Rust server（04 计划 §3.7 意图保留；boot 目标已是 Rust） |
| `packages/agent-profile/test/fixtures/` | Rust CI 对拍夹具（`rust-agent-md-parity` job）——fixtures 迁入 rust 仓库路径，TS 包删除 |

### 删除（SDR-002 裁决）

`packages/agent-core`、`agent-core-v2`、`kap-server`、`klient`、`acp-adapter`、`acp-server`、`kaos`、`minidb`、`transcript`、`tree-sitter-bash`、`node-sdk`（mirri-code-sdk）、`pi-tui`、`mirri-node`、`apps/mirri-code`、`apps/vis`，及 `packages/oauth`/`kimi-code-oauth`/`api-shared` 空壳目录。

## 3. 非目标（OUT of scope）

- Rust 侧新增任何功能（rotate-token 等价物、vis 等价物）——裁决为"随删除丢弃"。
- 修改 `rust/crates/` 的**产品代码**。rust 侧仅有的两处触碰均为纯数据/夹具迁移：①agent-profile 对拍夹具迁入 rust 仓库路径；②`mirri-proto` 错误码对拍夹具（当前 `include_str!` 指向被删的 TS 源文件）随源迁移到 rust 侧路径（详见架构文档 ADR-013）。
- `apps/mirri-web` 的功能改动（只动依赖面与 v2 相关测试）。
- npm 已冻结发布物的处理（维持冻结，不动 `latest.json`）。

## 4. 风险与缓解

| 风险 | 等级 | 缓解 |
|---|---|---|
| web e2e 依赖被删包（backend-switch 的 v2 用例、subagent-projector 的 v1 fixture） | 高 | 逐文件改造为纯 Rust 路径或删除 v1/v2 对照断言；全量 `pnpm test` + `pnpm test:e2e` 守护 |
| 贡献者依赖 dev:cli / TUI 的工作流中断 | 中 | `dev:web`（默认 rust）不受影响；文档更新说明新的 CLI 使用方式（rust 二进制直跑） |
| 删除后 workspace/lockfile/flake 不一致导致构建红 | 中 | 每步同步 `pnpm install` + sherif + check-nix-workspace；flake 双清单手工同步 |
| rust release 打包脚本对 TS CLI 的 `yazl` 硬依赖 | 高 | 将 `yazl` 提升到根 devDependencies（会改 nix pnpmDeps hash，按流程重算） |
| agent-profile fixtures 迁移路径错导致 rust 对拍 CI 红 | 中 | 只做数据移动 + 两处路径常量更新；`cargo test -p mirri-core --test agent_md_cross_engine_tests` 守护 |
| 契约对拍测试（klient schemas，198 方法）随 klient 删除而消失 | 中 | 已有 Rust 侧 `cross_backend_contract.rs` 双后端对拍承接同等职责（裁决见 ADR-012） |
| 误删仍被引用的支撑包 | 中 | 删除前逐包 grep 消费者（ADR-011 审计清单在架构文档） |

## 5. 验收标准

| # | 标准 | 验证 |
|---|---|---|
| 1 | 仓库无被删包残留 | `ls packages/ apps/` 无 §2 删除清单中的目录 |
| 2 | 全量默认测试绿 | `pnpm install && pnpm test`（web + desktop 套件） |
| 3 | Rust 内核 e2e 绿 | 构建 `rust/target/debug/mirri` 后 `pnpm test:e2e` |
| 4 | Rust 套件不回归 | `cargo test --workspace --locked` + clippy + 对拍夹具迁移后 `agent_md_cross_engine_tests` 绿 |
| 5 | 全仓 typecheck 绿 | 存续包的 typecheck（前置：根 `typecheck` 脚本中的显式 filter 条目已随包删除清理，见架构 §6） |
| 6 | 构建绿 | `pnpm run build`（存续包）；`rust-native` 打包脚本 dry-run 验证 `yazl` 解析成功 |
| 7 | 一致性 | `pnpm sherif`、`node scripts/check-nix-workspace.mjs`、`pnpm install --frozen-lockfile` |
| 8 | grep 干净 | 对被删包名 + TS CLI 入口符号的全仓 grep 为空；排除范围：`*.md`（含 rust 源码 doc 注释中的历史路径引用）、rust 侧迁移后的夹具数据文件（agent-profile fixtures、error-codes 夹具）、`docs/specs/` 与 `plan/` 历史快照 |
| 9 | changeset 已生成 | `.changeset/` 有 major 条目（`@mirri-ai/release`，用户已确认 major） |

## 6. 回滚

单一（或少量）squash commit 合入主工作区分支；`git revert` 即整体回滚。纯删除性变更无数据迁移，无灰度面。
