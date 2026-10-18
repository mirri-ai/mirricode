# rust/ 内核域工作约定

本目录是 rust 引擎 workspace。硬契约如下，任何任务开始前先读：
rust 引擎还在开发中，很多功能可能还不成熟甚至是错误的。

## 架构契约（违反即返工）

1. **契约事实源链**：RPC 方法面的唯一事实源是 `crates/mirri-proto/src/rpc/registry.rs` 注册表，经 `authoritative-methods.tsv` ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture 五环节联动。任何方法面变更必须全链同步，缺一环就是漂移。
2. **wire log append-only**：`wire.jsonl` 严格追加（torn-final-line 容忍除外），永不截断重写。**新增任何会话状态域（goal/cron/permission/plan/工具集…）必须同时提供写侧（`record_op`）与 reducer 恢复路径**——只有 reducer 分支没有生产写入的代码是死代码，等于功能不存在。
3. **事件词汇唯一源**：引擎事件投影在 `crates/mirri-core/src/session/event_projection.rs`（`event.*` 方法名）。新增客户端可见事件必须先扩投影，再同步 web 投影器、SDK rust 翻译层（`packages/node-sdk/src/rust-engine/translate.ts`）、ACP 翻译层三处消费端——**任何一处漏同步 = 该端功能静默失效**（历史教训：SDK 翻译层曾只放行 2/26 种事件导致 TUI 状态机整体停摆）。
4. **ACP 合规真源**：mirri-acp 的测试必须对照官方 ACP v1 schema（`SessionUpdate` 联合、`modeId`/`configId` 参数名）编写，不得按 rust 自认方言自证——自洽测试全绿不等于协议合规。
5. **缓存可观测**：涉及请求前缀/缓存命中的改动，验收必须给出 usage 三桶（uncachedInput/cacheRead/cacheWrite）数值或请求前缀对比证据，不接受"应该会更好"。
6. **zero-IO 已裁决方向 A（裁决 Z1，2026-09-07；迁移已完成，2026-09-09 plans/07 T13 全子批）**：mirri-core 的 IO 全部经 trait 注入（`FileSystem`/`SyncFs`/`PtyProvider`/`OsProcess`，实现与 `Local*` 默认在 mirri-os，组合根可覆盖）。**守卫双重强制执行**：本地 `node rust/scripts/check-zero-io.mjs`（挂载 `pnpm gate:rust-zero-io`）+ CI job `rust-zero-io-gate`（`.github/workflows/ci.yml`，每个 PR/push 必跑）——core src 生产代码禁 `std::fs`/`tokio::fs`/`std::process`/`tokio::process` 等直接 IO 调用（内嵌/外部 `#[cfg(test)]` 模块剥离后扫描），依赖树禁网络/DB crate（reqwest 仅经 jsonschema carrier 白名单、rusqlite 禁）；需要 IO 的新代码放 mirri-os 实现或经既有 trait（journal 读写等纯持久化操作放 mirri-persistence，如 `JsonlAppendLog::current_len`/`read_tail`），行级豁免必须注明 `// zero-io-allow: <reason>`。`tokio::spawn` 不在守卫禁配内（J2 裁决，任务调度非 IO；Clock/UuidGen 记 ROADMAP §5 候选）。

## 质量门禁（本地 + CI）

- `cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets -- -D warnings` 零告警
- mirri-core zero-IO 守卫：`node rust/scripts/check-zero-io.mjs`（根 `pnpm gate:rust-zero-io`）0 命中 exit 0
- 受影响 TS 包 `pnpm typecheck` + vitest；测试命名 Given-When-Then
- changeset 用 gen-changesets skill；major 需用户确认

## Shell 工具超时规则（防后台任务被误杀）

- 前台超时已对齐规格（ADR-4，`0b82e83d`）：超时自动转后台（保留 task_id、输出入行、完成自动通知），转后台后重布防后台超时（默认 600s、上限 86400s、`disable_timeout` 豁免）——前台命令不会再被 60s 杀灭。
- 后台超时已对齐 TS（`5c2182be`）：默认 600s、上限 86400s、超限拒绝。
- 仍建议：超长命令（全量 workspace cargo test/clippy、长 vitest）用 `run_in_background=true` + `disable_timeout=true`，避免占用转后台路径、确保一次拿到完整结果。

## 执行协议

- 计划与批次状态：`docs/specs/rust-core-design/plans/`（06-parity-completion 已收口，当前推进 07-followup-completion）；开放项状态唯一源 `docs/specs/rust-core-design/ROADMAP.md`
- 每批走 SDD：brief（`.superpowers/sdd/`）→ TDD → mutation 评审 → 门禁 → changeset → commit
- 设计文档只写终态（见 `docs/specs/AGENTS.md` 治理约定），现状进 `*-context.md`
