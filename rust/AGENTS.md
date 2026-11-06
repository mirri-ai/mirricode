# rust/ 内核域工作约定

本目录是 rust 引擎 workspace。硬契约如下，任何任务开始前先读：
rust 引擎还在开发中，很多功能可能还不成熟甚至是错误的。

## 统一质量门禁入口（本地 agent 必用，防漂移）

一切本地质量检查**先走 `pnpm gate`**（`node scripts/gate.mjs`），不要自己拼各门禁命令——顺序、已知失败记账、fast-fail 都在入口里统一维护：

- `pnpm gate` — 快速面（~3 秒，无编译无测试）：service-naming → nix-workspace → sherif → zero-io → spawn → test-hygiene，轻→重 fail-fast。
- `pnpm gate:rust` — 快速面 + clippy（分钟级）。
- `pnpm gate:full` — 全套：typecheck → lint → clippy → **rust 全量测试（最重，永远最后）** → TS 测试。
- `node scripts/gate.mjs --list` — 查看全部阶段与顺序。
- 退出码：0 = 全绿；2 = 通过但有已知失败（`gate.mjs` 的 KNOWN_FAILURES 记账，见下方）；1 = 新失败（fast-fail 中止）。
- **agent 作业纪律**：改完代码先 `pnpm gate`（秒级），过了再按需 `gate:rust` / `gate:full`；全量测试只在批次收尾跑，不在每步迭代跑。
- 新增门禁时：加进 `scripts/gate.mjs` 的 STAGES（按耗时插入合适位置），不要另开入口。
- **KNOWN_FAILURES 是债务不是许可证**：清单里的失败必须带 owner 注记与 TODO(remove) 条款，债务清偿后立即删除条目。

## 架构契约（违反即返工）

1. **契约事实源链**：RPC 方法面的唯一事实源是 `crates/mirri-proto/src/rpc/registry.rs` 注册表，经 `authoritative-methods.tsv` ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture 五环节联动。任何方法面变更必须全链同步，缺一环就是漂移。
2. **wire log append-only**：`wire.jsonl` 严格追加（torn-final-line 容忍除外），永不截断重写。**新增任何会话状态域（goal/cron/permission/plan/工具集…）必须同时提供写侧（`record_op`）与 reducer 恢复路径**——只有 reducer 分支没有生产写入的代码是死代码，等于功能不存在。
3. **事件词汇唯一源**：引擎事件投影在 `crates/mirri-core/src/session/event_projection.rs`（`event.*` 方法名）。新增客户端可见事件必须先扩投影，再同步 web 投影器、SDK rust 翻译层（`packages/node-sdk/src/rust-engine/translate.ts`）、ACP 翻译层、rust TUI 翻译层（`crates/mirri-tui/src/msg.rs`，2026-10-24 起纳入）四处消费端——**任何一处漏同步 = 该端功能静默失效**（历史教训：SDK 翻译层曾只放行 2/26 种事件导致 TUI 状态机整体停摆）。
4. **ACP 合规真源**：mirri-acp 的测试必须对照官方 ACP v1 schema（`SessionUpdate` 联合、`modeId`/`configId` 参数名）编写，不得按 rust 自认方言自证——自洽测试全绿不等于协议合规。
5. **缓存可观测**：涉及请求前缀/缓存命中的改动，验收必须给出 usage 三桶（uncachedInput/cacheRead/cacheWrite）数值或请求前缀对比证据，不接受"应该会更好"。
6. **zero-IO 已裁决方向 A（裁决 Z1，2026-09-07；迁移已完成，2026-09-09 plans/07 T13 全子批）**：mirri-core 的 IO 全部经 trait 注入（`FileSystem`/`SyncFs`/`PtyProvider`/`OsProcess`，实现与 `Local*` 默认在 mirri-os，组合根可覆盖）。**守卫双重强制执行**：本地 `node rust/scripts/check-zero-io.mjs`（挂载 `pnpm gate:rust-zero-io`）+ CI job `rust-zero-io-gate`（`.github/workflows/ci.yml`，每个 PR/push 必跑）——core src 生产代码禁 `std::fs`/`tokio::fs`/`std::process`/`tokio::process` 等直接 IO 调用（内嵌/外部 `#[cfg(test)]` 模块剥离后扫描），依赖树禁网络/DB crate（reqwest 仅经 jsonschema carrier 白名单、rusqlite 禁）；需要 IO 的新代码放 mirri-os 实现或经既有 trait（journal 读写等纯持久化操作放 mirri-persistence，如 `JsonlAppendLog::current_len`/`read_tail`），行级豁免必须注明 `// zero-io-allow: <reason>`。`tokio::spawn` 不在守卫禁配内（J2 裁决，任务调度非 IO；Clock/UuidGen 记 ROADMAP §5 候选）。
7. **mirri-tui 可测试性两条铁律（ADR-001/002，2026-10-24，规格见 `docs/specs/tui-architecture.md` §3-§4）**：
   - **reducer/view 零 IO、零真实时间**：`update.rs`/`view/` 内禁任何 IO 调用与真实等待（等待一律经注入 Clock 或暂停的异步时间）。违反 = 测试全绿但真机卡死，视同返工。
   - **引擎交互只经 `EngineClient` trait，终端交互只经 `TerminalIo` trait**：UI 逻辑测试永不触碰真实传输/终端；生产 RPC/重连逻辑在 L3 单独覆盖。任何 mirri-tui 贡献（PR/批次）必须先声明"由哪一层测试覆盖"（L1 reducer 单测 / L2 TestBackend+insta 快照 / L3 vt100 集成 / L4 e2e feature-gated）再进入评审；L3/L4 断言只允许引用 vt100 屏幕可见文本，禁止引用内部状态字段。


## 质量门禁（本地 + CI）

- `cargo test --workspace` 全绿 + `cargo clippy --workspace --all-targets -- -D warnings` 零告警
- RustSec/许可/来源门禁：`cargo deny --manifest-path Cargo.toml check`（配置 `rust/deny.toml`，CI job `rust-deny`）；漏洞类告警在 CI 中直接失败，`deny.toml` 的 `ignore` 列表新增条目必须带 reason 与移除期限
- Windows 交叉编译验证用 `cargo xwin check --target x86_64-pc-windows-msvc`（裸 `cargo check --target x86_64-pc-windows-msvc` 会因 aws-lc/lzma 的 C 依赖缺 MSVC 头而失败，与本仓代码无关）
- mirri-core zero-IO 守卫：`node rust/scripts/check-zero-io.mjs`（根 `pnpm gate:rust-zero-io`）0 命中 exit 0
- 受影响 TS 包 `pnpm typecheck` + vitest；测试命名 Given-When-Then
- changeset 用 gen-changesets skill；major 需用户确认

## 测试铁律（违反 = 返工；源自 `docs/specs/rust-core-design/16-test-execution-strategy.md` §4）

### 测试分层（唯一允许的层级定义，设计 §4.1；命名裁决 SDR-016.2：用 T0–T3 前缀，与 TUI 规范的 L1–L4 分层无关）

| 层 | 名称 | 位置 | 职责 | 并行安全 | 速度预算 |
|---|---|---|---|---|---|
| T0 | 单元测试 | `src/` 内 `#[cfg(test)]` | 纯逻辑：reducer、解析、纯函数。禁止 IO、禁止真实时间、禁止环境变量读写 | 天然可并行 | 单测 ≤5s |
| T1 | 组件集成测试 | `tests/` 目录 | 单 crate 内组件协作，使用注入的假件（Fake FS / 暂停时钟 / MockLlm） | 必须可并行 | 单测 ≤5s |
| T2 | 服务级集成测试 | `tests/` 目录（server/embedded 等） | 起真实服务实例（随机端口 + 私有 HOME），验证协议行为 | 必须可并行 | 单测 ≤30s |
| T3 | 真实二进制 e2e | `#[ignore]` gated 或独立命令 | 真实网络/真实 LLM/真实子进程链路 | 显式串行/手动触发 | 不设预算（不进全量门禁） |

- **层级判定规则**：一个测试属于哪层，取决于它**需要什么真实资源**。需要假件即可覆盖的归 T0/T1；需要真实进程/端口的归 T2；需要真实外部网络的归 T3。T3 不参与 `cargo test --workspace` 默认门禁，仅 `--ignored` 或专用命令触发。
- **层级声明**：每个测试模块（`mod` 或文件）头部必须携带 `// layer: T<n>` 注释标签（如 `// layer: T1`）。新测试缺标签不得进入评审；存量缺标签由 baseline 白名单豁免，R3 移动批回填。

### 隔离契约（八条，设计 §4.2）

1. **环境变量**：T0/T1 禁止读写进程环境变量。需要环境语义的代码必须经注入式解析（`resolve_*_from` 模式），测试传值而非改环境。T2 如确需设置环境变量（如真子进程需要），必须：(a) 只作用于自己 spawn 的子进程命令（`Command::env`），(b) 若必须 `set_var`，则使用**全 workspace 唯一**的 `env::lock()` 守卫并覆盖最小变量集合。
2. **HOME / 目录**：所有测试的文件足迹必须落在 `tempfile::tempdir()`（或等价一次性目录）内。禁止读写真实 `~` 下任何路径。需要 HOME 语义的，注入 `home_dir` 参数或使用隔离的 fake home 目录。
3. **端口**：一切监听必须 `127.0.0.1:0` 随机分配，禁止硬编码端口。客户端侧对端口冲突需有重试容忍。
4. **时间**：T0/T1 禁止真实等待（`thread::sleep` / 固定 `tokio::time::sleep`）。异步测试默认 `start_paused`；需要推进时间用 `tokio::time::advance`。轮询等待一律用带 deadline 的 `wait_for_*` helper（禁止对象是测试代码内的**裸固定 sleep**，helper 内部 ≤50ms 步进机制除外），禁止裸固定 sleep 充当同步原语。
5. **网络**：默认门禁测试禁止真实外网。HTTP 一律 wiremock / 本地 listener。
6. **数据库 / 锁**：测试用 db 文件与锁文件必须在 tempdir 内。
7. **工作目录**：测试不得假设 CWD 可写；不得在真实 CWD 下留痕。
8. **锁与异步**：`env::lock()` 为同步 Mutex，**禁止跨 `.await` 持锁**（多线程 runtime 下线程饥饿死锁风险）。需要异步上下文内改环境的，在进入异步块前完成 set/restore，或改为子进程级注入（`Command::env`）。

### 机械守卫与速度预算

- 新测试必须声明层级（`// layer: T<n>` 模块头标签），按上表判定。T3 必须 `#[ignore]`。
- 新测试必须满足全部八条隔离契约，方可进入评审。
- 需要环境变量语义 → 注入式优先；确需 `set_var` → `mirri-test-support::env::lock()` + 最小集合 + 不跨 await。
- 存量测试不满足隔离契约的，按 ROADMAP 分批迁移，不阻塞新贡献（baseline 白名单：`rust/scripts/test-hygiene-baseline.json`）。
- **主门禁**：`node rust/scripts/check-test-hygiene.mjs`（挂 `pnpm gate:rust-test-hygiene`，与 zero-io 守卫同级别）——扫 test cfg 内裸 `std::env::set_var`/`remove_var`、裸 `thread::sleep`、`tests/` 文件缺 `// layer:` 标签；行内 `// hygiene-allow: <reason>` 豁免。clippy `disallowed-methods` 因全局作用域无法限定 test cfg（会误伤生产合法 `set_var`），仅作为 ROADMAP 增量项（SDR-016.3）。
- 速度预算：单测试 ≤5s（T0/T1）/ ≤30s（T2）；全量 workspace ≤7min（热缓存口径）。超预算的测试须拆分或降层（改用假件）。
- 每个测试 crate 维护共享 `tests/common/` 模块；新增测试搭建代码必须进 common，不得在各测试文件内重复。

## Shell 工具超时规则（防后台任务被误杀）

- 前台超时已对齐规格（ADR-4，`0b82e83d`）：超时自动转后台（保留 task_id、输出入行、完成自动通知），转后台后重布防后台超时（默认 600s、上限 86400s、`disable_timeout` 豁免）——前台命令不会再被 60s 杀灭。
- 后台超时已对齐 TS（`5c2182be`）：默认 600s、上限 86400s、超限拒绝。
- 仍建议：超长命令（全量 workspace cargo test/clippy、长 vitest）用 `run_in_background=true` + `disable_timeout=true`，避免占用转后台路径、确保一次拿到完整结果。

## 执行协议

- 计划与批次状态：`docs/specs/rust-core-design/plans/`（06-parity-completion 已收口，当前推进 07-followup-completion）；开放项状态唯一源 `docs/specs/rust-core-design/ROADMAP.md`
- 每批走 SDD：brief（`.superpowers/sdd/`）→ TDD → mutation 评审 → 门禁 → changeset → commit
- 设计文档只写终态（见 `docs/specs/AGENTS.md` 治理约定），现状进 `*-context.md`
