# rust/ 内核域工作约定

本目录是 rust 引擎 workspace。硬契约如下，任何任务开始前先读：
rust 引擎还在开发中，很多功能可能还不成熟甚至是错误的。

## 统一质量门禁入口（本地 agent 必用，防漂移）

一切本地质量检查**先走 `pnpm gate`**（`node scripts/gate.mjs`），不要自己拼各门禁命令——顺序、已知失败记账、fast-fail 都在入口里统一维护：

- `pnpm gate` — 快速面（约 3 秒，无编译无测试）：service-naming → sherif → rust-zero-io → rust-spawn → rust-test-hygiene，轻→重 fail-fast。
- `pnpm gate:rust` — 快速面 + clippy（分钟级）。
- `pnpm gate:full` — 全套：typecheck → lint → clippy → **rust 全量测试（最重，永远最后）** → TS 测试。
- `pnpm gate:real` — 快速面 + R 层（真实环境层）`#[ignore]` 测试（本地 opt-in，不进默认门禁；R 层语义见 `testing-guide` skill §3.4）。
- `node scripts/gate.mjs --list` — 查看全部阶段与顺序。
- 退出码：0 = 全绿；2 = 通过但有已知失败（`gate.mjs` 的 KNOWN_FAILURES 记账，见下方）；1 = 新失败（fast-fail 中止）。
- **agent 作业纪律**：改完代码先 `pnpm gate`（秒级），过了再按需 `gate:rust` / `gate:full`；全量测试只在批次收尾跑，不在每步迭代跑。
- 新增门禁时：加进 `scripts/gate.mjs` 的 STAGES（按耗时插入合适位置），不要另开入口。
- **KNOWN_FAILURES 是债务不是许可证**：清单里的失败必须带 owner 注记与 TODO(remove) 条款，债务清偿后立即删除条目。

## 测试（细节见 `testing-guide` skill）

动测试前先加载 `.agents/skills/testing-guide/SKILL.md`：执行阶梯、T0–T3/R 分层、八条隔离契约、全量红了怎么判、RPC pinned 计数清单都在那里。这里只留**违反即返工**的部分：

- **铁律摘要**：新测试必须在模块头声明 `// layer: T<n>`；T0/T1 禁止真实 IO、真实等待、进程环境变量读写；文件足迹只落 `tempfile::tempdir()`；监听一律 `127.0.0.1:0`；需要真实环境语义的测试归 R 层并加 `#[ignore]` + `// R-layer: <理由>`。
- **机械执行**：上述约束由 `check-test-hygiene.mjs`（挂 `pnpm gate`）实际校验，不靠自觉。设计事实源 `docs/specs/rust-core-design/16-test-execution-strategy.md` §4。
- **报告纪律**：全量红的测试在定性前必须隔离；隔离后仍要查失败集合是否稳定、是否共享一个具体特征（统一 502 → 查代理；e2e 整点超时 → 重建二进制；`models.dev` 空目录 → 离线刷新）。确属环境问题也只报「未验证」，不得写成全绿。

## 架构契约（违反即返工）

1. **契约事实源链**：RPC 方法面的唯一事实源是 `crates/mirri-proto/src/rpc/registry.rs` 注册表，经 `authoritative-methods.tsv` ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture 五环节联动。任何方法面变更必须全链同步，缺一环就是漂移。
2. **wire log append-only**：`wire.jsonl` 严格追加（torn-final-line 容忍除外），永不截断重写。**新增任何会话状态域（goal/cron/permission/plan/工具集…）必须同时提供写侧（`record_op`）与 reducer 恢复路径**——只有 reducer 分支没有生产写入的代码是死代码，等于功能不存在。
3. **事件词汇唯一源**：引擎事件投影在 `crates/mirri-core/src/session/event_projection.rs`（`event.*` 方法名）。新增客户端可见事件必须先扩投影，再同步 web 投影器（`apps/mirri-web/src/api/daemon/mappers.ts` 的 `toAppEvent` switch）、嵌入式服务端中继（`crates/mirri-embedded/src/embedded_server/event_relay.rs`）、ACP 翻译层（`crates/mirri-acp/src/acp_handler.rs`）与 TS wire 类型（`packages/protocol/src/events.ts`）四处消费端——**任何一处漏同步 = 该端功能静默失效**（历史教训：SDK 翻译层曾只放行 2/26 种事件导致 TUI 状态机整体停摆）。漏在 `mappers.ts` 的事件会落进 `default:` 分支变成 `unknown` 被静默丢弃。契约链的完整环节清单见 `codebase-investigation` skill §4.1。
4. **ACP 合规真源**：mirri-acp 的测试必须对照官方 ACP v1 schema（`SessionUpdate` 联合、`modeId`/`configId` 参数名）编写，不得按 rust 自认方言自证——自洽测试全绿不等于协议合规。
5. **缓存可观测**：涉及请求前缀/缓存命中的改动，验收必须给出 usage 三桶（uncachedInput/cacheRead/cacheWrite）数值或请求前缀对比证据，不接受"应该会更好"。
6. **zero-IO 已裁决方向 A（裁决 Z1，2026-09-07；迁移已完成，2026-09-09 plans/07 T13 全子批）**：mirri-core 的 IO 全部经 trait 注入（`FileSystem`/`SyncFs`/`PtyProvider`/`OsProcess`，实现与 `Local*` 默认在 mirri-os，组合根可覆盖）。**守卫双重强制执行**：本地 `node rust/scripts/check-zero-io.mjs`（挂载 `pnpm gate:rust-zero-io`）+ CI job `rust-zero-io-gate`（`.github/workflows/ci.yml`，每个 PR/push 必跑）——core src 生产代码禁 `std::fs`/`tokio::fs`/`std::process`/`tokio::process` 等直接 IO 调用（内嵌/外部 `#[cfg(test)]` 模块剥离后扫描），依赖树禁网络/DB crate（reqwest 仅经 jsonschema carrier 白名单、rusqlite 禁）；需要 IO 的新代码放 mirri-os 实现或经既有 trait（journal 读写等纯持久化操作放 mirri-persistence，如 `JsonlAppendLog::current_len`/`read_tail`），行级豁免必须注明 `// zero-io-allow: <reason>`。`tokio::spawn` 不在守卫禁配内（J2 裁决，任务调度非 IO；Clock/UuidGen 记 ROADMAP §5 候选）。
7. **TUI 铁律（ADR-001/002，2026-10-24）**：适用对象是**未来的** rust TUI（规格见 `docs/specs/tui-architecture.md` §3-§4）。TS TUI 壳已于 2026-10-28 退役（`docs/specs/ts-v1-engine-retirement-requirements.md`），`crates/mirri-tui` 不存在；下列约束在 TUI crate 重建时生效，不得以"还没有 TUI"为由跳过设计：
   - **reducer/view 零 IO、零真实时间**：`update.rs`/`view/` 内禁任何 IO 调用与真实等待（等待一律经注入 Clock 或暂停的异步时间）。违反 = 测试全绿但真机卡死，视同返工。
   - **引擎交互只经 `EngineClient` trait，终端交互只经 `TerminalIo` trait**：UI 逻辑测试永不触碰真实传输/终端；生产 RPC/重连逻辑在 L3 单独覆盖。任何 TUI 贡献（PR/批次）必须先声明"由哪一层测试覆盖"（L1 reducer 单测 / L2 TestBackend+insta 快照 / L3 vt100 集成 / L4 e2e feature-gated）再进入评审；L3/L4 断言只允许引用 vt100 屏幕可见文本，禁止引用内部状态字段。

## 质量门禁（本地 + CI）

- `cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets -- -D warnings` 零告警
- RustSec/许可/来源门禁：`cargo deny --manifest-path Cargo.toml check`（配置 `rust/deny.toml`，CI job `rust-deny`）；漏洞类告警在 CI 中直接失败，`deny.toml` 的 `ignore` 列表新增条目必须带 reason 与移除期限
- Windows 交叉编译验证用 `cargo xwin check --target x86_64-pc-windows-msvc`（裸 `cargo check --target x86_64-pc-windows-msvc` 会因 aws-lc/lzma 的 C 依赖缺 MSVC 头而失败，与本仓代码无关）
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
