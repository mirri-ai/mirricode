# rust/ 内核域工作约定

本目录是 rust 引擎 workspace。硬契约如下，任何任务开始前先读：
rust 引擎还在开发中，很多功能可能还不成熟甚至是错误的。

## 统一质量门禁入口（本地 agent 必用，防漂移）

一切本地质量检查**先走 `pnpm gate`**（`node scripts/gate.mjs`），不要自己拼各门禁命令——顺序、已知失败记账、fast-fail 都在入口里统一维护：

- `pnpm gate` — 快速面（约 3 秒，无编译无测试）：service-naming → sherif → rust-zero-io → rust-spawn → rust-test-hygiene，轻→重 fail-fast。
- `pnpm gate:rust` — 快速面 + clippy（分钟级）。
- `pnpm gate:full` — 全套：typecheck → lint → clippy → **rust 全量测试（最重，永远最后）** → TS 测试。
- `pnpm gate:real` — 快速面 + R 层（真实环境层）`#[ignore]` 测试（本地 opt-in，不进默认门禁；R 层语义见「测试铁律」规则 R）。
- `node scripts/gate.mjs --list` — 查看全部阶段与顺序。
- 退出码：0 = 全绿；2 = 通过但有已知失败（`gate.mjs` 的 KNOWN_FAILURES 记账，见下方）；1 = 新失败（fast-fail 中止）。
- **agent 作业纪律**：改完代码先 `pnpm gate`（秒级），过了再按需 `gate:rust` / `gate:full`；全量测试只在批次收尾跑，不在每步迭代跑。
- 新增门禁时：加进 `scripts/gate.mjs` 的 STAGES（按耗时插入合适位置），不要另开入口。
- **KNOWN_FAILURES 是债务不是许可证**：清单里的失败必须带 owner 注记与 TODO(remove) 条款，债务清偿后立即删除条目。

## 本地测试执行阶梯（改了什么就跑什么）

`pnpm gate` **不编译、不跑测试**（只跑 5 个静态守卫，~3s）；下一个能跑测试的档直接是 `gate:full` 的 8 分钟全量。所以日常迭代的主路径是**直接对单个 crate / 单个 test target 跑 cargo**，不要在两极之间二选一。

| 意图 | 命令 | 成本 |
|---|---|---|
| 每次编辑后 | `pnpm gate` | ~3s，不编译不测试 |
| 改了 rust 生产代码（跨 crate 面） | `pnpm gate:rust` | 分钟级（含 clippy） |
| **日常迭代主路径：改了某个 crate** | `cargo test -p <crate> --lib` | 秒级~分钟级 |
| **改动了 `tests/` 下的文件** | `cargo test -p <crate> --test <bin>` | 同上 |
| 单个测试定位 | `cargo test -p <crate> --lib <子串>` | 最快 |
| 批次收尾 / 提 PR 前 | `pnpm gate:full` | ~8min（热缓存） |
| 动了 R 层（`#[ignore]`） | `pnpm gate:real` | 1-2min |
| 改依赖 / 新增 crate | `cargo deny --manifest-path rust/Cargo.toml check` | 分钟级（**不在任何 gate 档里**；需先 `cargo install cargo-deny`） |
| 动了平台相关代码 | `cargo xwin check --target x86_64-pc-windows-msvc` | 分钟级（**不在任何 gate 档里**；需先 `cargo install cargo-xwin`） |

### 测试分类如何映射到执行（关键：不要解析注释标签）

`// layer: T<n>` 是**评审合规元数据**，只被 `check-test-hygiene.mjs` 校验「文件头 20 行内存在」，**没有任何 runner 消费它**。不要为了"按分类挑测试"去写解析注释标签的脚本——那条路维护成本高且必然漂移。

真正被 cargo 消费的分类轴是 **test target 本身**，映射是现成且零维护的：

| 层（AGENTS 分层表） | 物理位置 | 执行命令 |
|---|---|---|
| T0 单元测试 | `src/` 内 `#[cfg(test)]` | `cargo test -p <crate> --lib` |
| T1/T2 集成测试 | `tests/*.rs` | `cargo test -p <crate> --test <文件名>` |
| T3/R 真实环境 | `#[ignore]` gated（同上两个位置） | `pnpm gate:real` 或 `-- --ignored` |

当前 `tests/` 目标清单（失效时以 `ls rust/crates/<crate>/tests/*.rs` 为准）：

```
mirri-acp: conformance desktop_share_e2e e2e_mock
mirri-cli: cli_tests
mirri-collaboration: collaboration_tests
mirri-core: session_group terminal_misc_group tools_scope_group
mirri-embedded: e2e_real_llm   ← 仅此一个；主力测试在 src/ 的 #[cfg(test)]（用 --lib）
mirri-kosong: providers_group
mirri-mcp: mcp_group
mirri-os: all_group
mirri-persistence: persistence_group
mirri-plugin-host: plugin_group
mirri-proto: proto_group
mirri-remote: remote_adapter_tests
mirri-server: server_group
mirri-test-harness: cross_backend_contract harness_tests mock_llm_tests
```

### 全量红了怎么判（这轮踩过的坑）

全量 workspace 里某条测试红，**先隔离单跑再定性**，不要直接当回归：

```bash
cargo test -p <crate> --lib <该测试名子串>   # 单跑
```

- 单跑绿 → 机器负载 / 跨测试干扰 / 环境（比如本机的 `fseventsd` 暴走会拖慢并放大竞态）。按环境问题记录，**不要写进报告当全绿**。
- 单跑红 → 真回归，按正常流程修。

判据：本仓 CI 的 `rust-test` job 是权威全量；本地全量的作用是**早期发现**，不是最终证据。

### 本地档位 ↔ CI job 对照（推 PR 前自查哪一档漏了）

| CI job（`.github/workflows/ci.yml`） | 本地等价 | 是否在 gate 档里 |
|---|---|---|
| `rust-test`（全量 `cargo test --workspace`） | `pnpm gate:full` | ✅ |
| `rust-clippy`（位于 `gate:rust`） | `pnpm gate:rust` | ✅ |
| `rust-zero-io-gate` | `pnpm gate`（`rust-zero-io` 阶段） | ✅ |
| `rust-test-hygiene-gate` | `pnpm gate`（`rust-test-hygiene` 阶段） | ✅ |
| `rust-real-env`（非阻塞）/ `nightly-rust-real-env`（必过） | `pnpm gate:real` | ✅（opt-in 档） |
| `rust-deny` | `cargo deny --manifest-path rust/Cargo.toml check` | ❌ **需手动** |
| `rust-agent-md-parity` | `cargo test -p mirri-core --test tools_scope_group agent_md_cross_engine_` 与 `agent_list_cross_engine_` | ❌ 需手动（改 `AGENTS.md` 模板/工具清单时） |
| Windows 交叉编译 | `cargo xwin check --target x86_64-pc-windows-msvc` | ❌ 需手动 |

后三行是已知缺口：改依赖、改 `AGENTS.md` 工具模板、动平台相关代码时必须手动补跑，否则 PR 才会红。它们不进 `pnpm gate` 的理由是本地要额外装 `cargo-deny` / `cargo-xwin`（非 Rust 默认工具链），不适合作默认门禁；未安装时按 CI 为准。

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

### 测试分层（唯一允许的层级定义，设计 §4.1；命名裁决 SDR-016.2：用 T0–T3 前缀，与 TUI 规范的 L1–L4 分层无关；T3 行同时承载执行维度 R，见下方「执行维度」）

| 层 | 名称 | 位置 | 职责 | 并行安全 | 速度预算 |
|---|---|---|---|---|---|
| T0 | 单元测试 | `src/` 内 `#[cfg(test)]` | 纯逻辑：reducer、解析、纯函数。禁止 IO、禁止真实时间、禁止环境变量读写 | 天然可并行 | 单测 ≤5s |
| T1 | 组件集成测试 | `tests/` 目录 | 单 crate 内组件协作，使用注入的假件（Fake FS / 暂停时钟 / MockLlm） | 必须可并行 | 单测 ≤5s |
| T2 | 服务级集成测试 | `tests/` 目录（server/embedded 等） | 起真实服务实例（随机端口 + 私有 HOME），验证协议行为 | 必须可并行 | 单测 ≤30s |
| T3/R | 真实环境层 | `#[ignore]` gated | **失败判定依赖真实环境**的测试：真实时间语义窗口、真实子进程抽样、性能探针、真实链路 e2e | 不进默认并行池；CI R 层 profile 固定 `test-threads = 2`（见规则 R）。本地 opt-in `pnpm gate:real`，CI `rust-real-env`（非阻塞）+ nightly（必过） | 不设预算（不进默认门禁） |

- **层级判定规则**：一个测试属于哪层，取决于它**需要什么真实资源**。需要假件即可覆盖的归 T0/T1；需要真实进程/端口的归 T2；需要真实外部网络的归 T3。T3 不参与 `cargo test --workspace` 默认门禁，仅 `--ignored` 或专用命令触发。
- **执行维度（R/D，设计文档 `docs/superpowers/specs/2026-11-06-rust-test-execution-optimization-design.md` §3）**：在资源维度之上叠加执行维度判定——"放到慢 10 倍的机器上跑，结果会变吗？"会变（或可能变）→ R 层（`#[ignore]`）；不变 → D 层。真实子进程本身不是 R 的充分条件，断言依赖真实墙窗/真实世界不可观测检查/外部服务才是（条件驱动 + 宽 backstop 的子进程测试留 D 层）。`// hygiene-allow: real-...` reason 前缀 = 强制 R 归因，由 test-hygiene 规则 R 机械执行。R 层套件语义、执行入口见下方「机械守卫与速度预算」的规则 R。
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
- 存量测试不满足隔离契约的，按 ROADMAP 分批迁移，不阻塞新贡献（baseline 白名单：`rust/scripts/test-hygiene-baseline.json`）。**sleep 规则零容忍 + 覆盖判定 allow-list 化（批 6a/6b 起）**：baseline 的 sleep 条目**只有 reason 匹配 `^(poll-step|negative-wait|real-|D:)` 才算已覆盖**——其余（无 reason、`legacy:`、`Legacy:` 大写变体、随口写的 prose）一律不覆盖。`--write-baseline` 重锚时，**新条目（该 file+type 组内无对应旧条目）一律写不覆盖的 reason**，所以重锚不能把新裸 sleep 洗白：写完后下一次 gate 仍然红，直到有人给它一个真实 reason。
  - **已知机制边界（不要误以为绝对）**：sleep 的"守护上下文"判定扫命中行之前 15 行的原文找 `wait_for` 标记——同一测试里 15 行内只要出现过 `wait_for`，其后的裸 sleep 就被视为在轮询循环内；行内 `// hygiene-allow: <任意 prose>` 也被接受（规则 R 的 `real-` 分支之外不做 reason 校验）。收窄这个窗口能挡住更多存量盲区（审计就靠它找到了 4 处），但需要同步重排 150+ 条基线条目，属独立批次。
- **主门禁**：`node rust/scripts/check-test-hygiene.mjs`（挂 `pnpm gate:rust-test-hygiene`，与 zero-io 守卫同级别）——扫 test cfg 内裸 `std::env::set_var`/`remove_var`、裸 `thread::sleep`、`tests/` 文件缺 `// layer:` 标签、规则 R 的 real-env 信号（见下条）；行内 `// hygiene-allow: <reason>` 豁免（规则 R 的 `real-` 窗口分支除外）。clippy `disallowed-methods` 因全局作用域无法限定 test cfg（会误伤生产合法 `set_var`），仅作为 ROADMAP 增量项（SDR-016.3）。
- **规则 R（real-env 归因棘轮）**：测试代码里有两类信号，必须归因到 R 层，由 test-hygiene 机械执行：
  - **裸真实子进程 spawn**（`std::process::Command` / `tokio::process::Command` / `Command::new`）：所在行必须在 `#[ignore]` 的测试 fn 内，或带行级 `// hygiene-allow: <reason>`。helper fn 挂不了 `#[ignore]`，所以它的 spawn 行**默认保持可见**——只能靠行级 `// hygiene-allow` 豁免；caller 测试上的 `#[ignore]` **不为 helper 的 spawn 背书**。
  - **`// hygiene-allow: real-...` 语义窗口**：必须写在 `#[ignore]` 的测试 fn 内；helper 里的窗口 gate 不报（不检查 caller）——审计时按 caller 归因。此分支不接受 `hygiene-allow` 当逃生门——reason 前缀本身就是 R 信号。
  - 包装 spawn（mirri_os 族）不被覆盖，由审计台账逐处判定。
  - **R 层套件**（CI 口径）：`#[ignore]` 测试集 **减** `e2e_real_llm`（需真实 LLM 凭证，手动触发）**减** G-30 占位 stub（`should_handle_create_rpc_independently` / `should_handle_plugins_rpc_independently`，故意 panic 的 TODO 桩，按测试名排除）。
  - **本地入口**：`pnpm gate:real` = `cargo test -- --ignored` 减两个 stub（libtest `--skip`）。`gate:real` 不按名排除 `e2e_real_llm`——无凭证时它自行跳过；但本地若导出了 `OPENAI_API_KEY`，`gate:real` 会真的打真实 LLM（成本/挂起风险，跑前记得 `unset OPENAI_API_KEY`）。本地跑 R 层不碰 nextest——macOS 逐 exec 税实测 14×–80×+（"80×+" 是 ">18min 未跑完" 换算出的下界，不是天花板）。
  - **CI 选择**：`rust-real-env`（非阻塞）+ nightly（必过）跑 `cargo nextest run --workspace --locked --run-ignored only --no-fail-fast -E 'not (binary(e2e_real_llm) or test(should_handle_create_rpc_independently) or test(should_handle_plugins_rpc_independently))' --profile real`（`--no-fail-fast` 承重：一次红要列全所有失败测试，那是归因输入）；profile 在 `rust/.config/nextest.toml`（retries 1、slow-timeout 2m×3、`test-threads = 2`）。
  - **并发例外**：`test-threads = 2` 是**有意硬编码**——R 层对真实子进程/端口争抢敏感，不跟 nproc 走；这是 D 层「不硬编码并发、按 nproc/实测降并发」策略的明确例外（设计 §6；该节写的 `max-threads` 是笔误，nextest 的真实键名是 `test-threads`，不要照着改配置）。
  - **执行器采纳**：nextest 仅限 R 层；D 层采纳需过 A/B 门槛（≤1.1×，`.github/workflows/rust-nextest-ab.yml`）。
  - **`#[ignore]` 风格**：R 层测试用裸 `#[ignore]` + 紧邻注释 `// R-layer: <理由> (design §3)`。先例（都是这个形态）：`mirri-acp/tests/conformance.rs` 的 `should_not_spawn_the_real_homes_mcp_servers_when_the_conformance_child_creates_a_session`（pgrep 缺失抽样）、`mirri-embedded/src/embedded_server/tests.rs` 的 `should_not_connect_plugin_mcp_servers_when_installed_until_reload`（plugin 缺失抽样）、`mirri-embedded/src/skill_watcher.rs` 的 `should_mount_the_watcher_within_a_fixed_time_budget`（mount 探针）。（`b1_print_mode_tests.rs` 的 drain 工时测试曾是本列表先例，已迁 D 层 paused-clock——批 5 摘 `#[ignore]`。）`e2e_real_llm.rs` 用裸 `#[ignore]` + 模块头说明理由。`#[ignore = "reason"]` 形式也接受，但非必需。
  - **已知约束**：`#[ignore]` 当前一词两义（R 层 vs TODO 桩）。新 TODO 桩必须二选一：按同名约定加进 R 层排除列表，或先实现再落地（不留永不可运行的桩）。
  - **配置陷阱**：nextest 对未知配置键只警告不报错（exit 0）——改 `rust/.config/nextest.toml` 后必须核对 stderr 无 `ignoring unknown configuration key` 警告，否则写错键名的 profile 会静默失效。
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
