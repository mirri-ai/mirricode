# TS 内核与 CLI 全量退役 — 现状快照（context）

> **文档类型**：参考类（Reference）。只记录当前状态事实，不含决策；设计见 [ts-v1-engine-retirement-requirements.md](ts-v1-engine-retirement-requirements.md) / [ts-v1-engine-retirement-architecture.md](ts-v1-engine-retirement-architecture.md)。
> **快照日期**：2026-10-28（全量退役执行后的终态）

## 退役执行结果

- 删除的包：`packages/{agent-core,agent-core-v2,kap-server,klient,acp-adapter,acp-server,kaos,minidb,transcript,tree-sitter-bash,node-sdk,pi-tui,mirri-node,agent-profile,kosong,telemetry}`；`apps/{mirri-code,vis}`；`plugins/`；`build/`；`rust/tools/`（TS feature 提取死工具）；`packages/{oauth,kimi-code-oauth,api-shared}` 空壳目录。
- 存留 TS 面：`apps/mirri-web`、`apps/mirri-desktop`（+lite）、`packages/{protocol,e2e-harness,server-e2e,release}`。kosong 与 telemetry 经审计零消费者后删除（ADR-011）。
- rust 侧夹具迁移：agent-profile 对拍夹具 → `rust/crates/mirri-core/tests/fixtures/agent-profile/`；`mirri-proto` 错误码对拍夹具 → `rust/crates/mirri-proto/test-fixtures/error-codes.ts`；agent-core-v2 的 system.md 镜像 → `tests/fixtures/agent-profile/system-mirror.md`（评审中发现的第三处 include 依赖）。
- `yazl` 提升到根 devDependencies；`rust/scripts/release-package.mjs` 改从根解析。
- `scripts/check-nix-workspace.mjs` 锚点改绑 `@mirri-ai/mirri-desktop`。
- `server-e2e` docker 脚本改为构建并启动 rust `mirri serve` 二进制。
- web 改造：vite 收缩为 rust v3 唯一后端 preset；`backend-switch.e2e`/`subagent-projector-e2e` 重写为纯 rust 引擎路径（后者经 WsRpcApi 的 AppEvent 流断言，等价守护 projector 行为）。
- e2e-harness 缩减为 fake-provider-server + temp-dir。
- 用户文档：TUI 专属页（keyboard）删除，slash-commands 等改写为 Web UI 语境（zh/en 同步）。

## 验证记录（执行当日）

- `pnpm install --frozen-lockfile` 干净安装通过。
- `pnpm test`：web 121 文件 / 1807 用例 + desktop 20 文件 / 192 用例全绿（shell-env 测试有一次间歇红，复跑绿——与主工作区基线同款 flake）。
- `pnpm test:e2e`（rust 引擎）：121 文件 / 1807 用例全绿。
- `pnpm run typecheck`、`pnpm run build`、`pnpm sherif`、`node scripts/check-nix-workspace.mjs`、`node --test scripts/*.test.mjs` 全绿。
- `flake.nix` `fetchPnpmDeps` hash 无法本地重算（无 nix），已标注待 CI 重算。
