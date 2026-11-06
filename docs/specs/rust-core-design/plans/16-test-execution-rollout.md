# 16 — 测试执行策略落地 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `16-test-execution-strategy.md` 设计：统一测试隔离设施、清除并行竞态、收敛测试二进制、治理 sleep，使 `cargo test --workspace --locked` 热缓存全量 ≤7 分钟且连续两次全绿。

**Architecture:** 新增 dev-only `mirri-test-support` crate 作为唯一 env 锁 / fake home / wait_for / 端口 helper 提供点；R2 把 14 把分叉锁与全部裸 `set_var` 收敛到唯一锁并注入化生产 HOME 读取；R3 以「纯移动 + `mod 文件名 {}` 包裹」机械规则合并集成测试二进制；R4 用 start_paused / wait_for 治理两个 sleep 重灾区。全程在隔离 worktree，最终由用户决定合并。

**Tech Stack:** Rust workspace（cargo 1.95）、libtest、tokio test-util（已有 dev-dep）、tempfile、clippy disallowed-methods、node 扫描脚本（对齐 check-zero-io.mjs 惯例）。

## Global Constraints（每个任务隐含遵守）

- 设计裁决：测试层级命名 **T0–T3**（禁用 L 编号，与 TUI 分层撞名）。
- 隔离契约八条（设计 §4.2），重点：唯一 `mirri-test-support::env::lock()`、禁跨 `.await` 持锁、T0/T1 禁裸 sleep、监听一律 `127.0.0.1:0`。
- 纯移动批零逻辑改动：R3 合并用 `mod 文件名 {}` 包裹原文件，import 不整理。
- 每任务结束：受影响 crate 测试全绿 + `cargo clippy -p <crate> --all-targets -- -D warnings` 零告警。
- 新 workspace 成员必须同时更新 `rust/Cargo.toml` 与根 `flake.nix` 的 `workspacePaths`/`workspaceNames`（AGENTS 硬规则）；flake 变更若本地有 nix 跑 `nix build .#mirri-code` 复验，无 nix 在最终报告注明依赖 CI。
- 测试命名 Given-When-Then。
- 提交信息不暴露 agent 身份、无 co-author。
- 基线事实（用于归因，不在 commit message 里写）：workspace 18 lib 目标 + 84 集成测试二进制；mirri-core 独占 27 个 `tests/*.rs`。

## 与设计的偏离声明

1. **机械守卫实现取舍（对应设计 §4.5 第 7 条）**：主门禁用 node 脚本 `check-test-hygiene.mjs`（可精确识别 test cfg + baseline 白名单机制），**不用** clippy `disallowed-methods`——clippy.toml 的该项是全局作用域，无法限定 test cfg，会误伤生产代码中合法的 `set_var` 准备（子进程注入等）。此取舍作为 SDR-016.3 回写设计文档（Task 3 一并做）。
2. **存量 `// layer:` 标签回填**：不在本计划单独设批，并入 R3 纯移动批（移动时在 mod 块头部顺手加 `// layer:` 标签，边际成本近零）；移动批未覆盖的存量由 baseline 白名单豁免，即裁决为「后续批次增量回填」。

## 任务编排总则

- **Task 4 → 5 → 6 严格串行**（三者都触碰所属 crate 的 Cargo.toml dev-dep 与卫生 baseline；Task 4 一次性为**全部**后续消费 crate 加好 dev-dep，Task 5/6 禁碰 Cargo.toml；baseline 的 regenerate 只在 Task 7 做一次）。
- Task 1→2→3 串行；Task 8→9→10 串行；Task 11/12 可并行（不同文件）；Task 13 依赖 8-10；Task 14 严格最后。
- 每个 R 批次验收任务（7/10/13/14）末尾把批次状态记入 `rust-core-design/ROADMAP.md` 对应条目。
- changeset：本计划为测试内部结构与内部 crate 变更，终端用户不可感知，预判**无需 changeset**；最终 PR 前由 gen-changesets skill 复核该判断。
- static 状态扫描的可判定判据：static 的类型含 `Mutex` / `RwLock` / `OnceLock` / `AtomicU*` / `lazy_static` / `RefCell` 即入暂缓审查名单；纯 const 型（`static X: &str` 等）忽略。

---

### Task 1: `mirri-test-support` crate 骨架（TDD）

**Files:**
- Create: `rust/crates/mirri-test-support/Cargo.toml`
- Create: `rust/crates/mirri-test-support/src/lib.rs`
- Modify: `rust/Cargo.toml`（members 加 `crates/mirri-test-support`）
- Modify: 根 `flake.nix`（workspacePaths + workspaceNames 各加一行）

**Interfaces:**
- Produces: crate `mirri-test-support`，模块 `env` / `home` / `wait_for` / `port`（本任务先建骨架与 `env`）。

- [ ] **Step 1: 建 crate 骨架**

`rust/crates/mirri-test-support/Cargo.toml`：

```toml
[package]
name = "mirri-test-support"
version = "0.1.0"
edition.workspace = true
license.workspace = true
publish = false

[dependencies]
tempfile = { workspace = true }
```

（先查 `rust/Cargo.toml` 的 `[workspace.dependencies]` 确认 tempfile 是否已在 workspace deps；若无则用与 mirri-core 相同版本并加入 workspace deps。）

`src/lib.rs`（Task 1 只落地 env 模块，其余三行先注释掉，Task 2 实现）：

```rust
//! Parallel-safe test infrastructure for the mirri rust workspace.
//!
//! Single provider of: env mutex guard, fake HOME, wait_for helpers,
//! ephemeral listeners. See docs/specs/rust-core-design/16-test-execution-strategy.md.

pub mod env;
// pub mod home;    // Task 2
// pub mod port;   // Task 2
// pub mod wait_for; // Task 2
```

- [ ] **Step 2: TDD env 模块 — 先写失败测试**

在 `src/env.rs` 中写测试（同文件 `#[cfg(test)]`，Given-When-Then 命名）：

```rust
use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// The single workspace-wide mutex guarding process-level env mutation.
/// Tests MUST NOT hold this across `.await` points.
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_allow_sequential_acquire_and_release_when_lock_is_dropped() {
        {
            let _g = lock();
        }
        let _g2 = lock();
    }
}
```

（实现与测试同批落地——锁本身无「失败测试」可言，验收改为编译 + 测试通过；此偏离在任务内注明。）

- [ ] **Step 3: 更新 workspace members + flake.nix**

`rust/Cargo.toml` members 追加 `"crates/mirri-test-support"`；根 `flake.nix` 的 `workspacePaths` 加 `"rust/crates/mirri-test-support"`，`workspaceNames` 加 `"mirri-test-support"`。

- [ ] **Step 4: 验证**

Run: `cargo check -p mirri-test-support --manifest-path rust/Cargo.toml && cargo test -p mirri-test-support --manifest-path rust/Cargo.toml`
Expected: PASS。

- [ ] **Step 5: Commit**

`git commit -m "feat(test): add mirri-test-support crate with workspace-wide env lock"`

---

### Task 2: `env::EnvGuard` + `home::temp_home()` + `port` + `wait_for`

**Files:**
- Modify: `rust/crates/mirri-test-support/src/env.rs`
- Create: `rust/crates/mirri-test-support/src/home.rs`
- Create: `rust/crates/mirri-test-support/src/port.rs`
- Create: `rust/crates/mirri-test-support/src/wait_for.rs`

**Interfaces:**
- Consumes: Task 1 的 `env::lock()`。
- Produces:
  - `pub struct EnvGuard { saved: Vec<(String, Option<String>)> }`；`EnvGuard::set(k: &str, v: &str)`（持 `env::lock()`，记录旧值）、`EnvGuard::set_many(&[(K, V)])`、`Drop` 恢复。
  - `pub fn temp_home() -> (std::path::PathBuf, tempfile::TempDir)`。
  - `pub fn ephemeral_listener() -> std::io::Result<tokio::net::TcpListener>`——注意：为避免引入 tokio 硬依赖，先用 `std::net::TcpListener` 版本 `pub fn ephemeral_listener() -> std::io::Result<std::net::TcpListener>`（`127.0.0.1:0` 绑定）；tokio 侧由调用方 `TcpListener::from_std` 转换。
  - `pub async fn wait_for<F, T, E>(timeout: Duration, step: Duration, f: F) -> Result<T, WaitTimeout>` 家族（泛型闭包轮询，步进默认 50ms）。

- [ ] **Step 1: TDD EnvGuard** — 写测试「should restore previous value when guard drops」「should preserve unset state when guard drops」（set 后 drop，断言 env 恢复），先跑红（函数不存在编译失败即可），再实现（Drop impl）。
- [ ] **Step 2: TDD temp_home** — 测试「should create writable home under tempdir」「returned tempdir outlives usage」（返回的 TempDir 不 drop 前 path 存在）。
- [ ] **Step 3: TDD ephemeral_listener** — 测试「should bind to an os-assigned port」（local_addr().port() != 0）。
- [ ] **Step 4: TDD wait_for** — 测试「should return immediately when condition holds」「should timeout when condition never holds」（步进 1ms，超时 10ms，控制测试时长）。
- [ ] **Step 5: 全 crate 测试 + clippy**

Run: `cargo test -p mirri-test-support --manifest-path rust/Cargo.toml && cargo clippy -p mirri-test-support --all-targets -- -D warnings`
Expected: PASS / 零告警。

- [ ] **Step 6: Commit**

`git commit -m "feat(test): add EnvGuard, temp_home, ephemeral_listener, wait_for helpers"`

---

### Task 3: `rust/AGENTS.md` 测试铁律 + 机械守卫脚本

**Files:**
- Modify: `rust/AGENTS.md`（新增「测试铁律」章节，内容 = 设计 §4.5 七条逐字 + 层级表）
- Create: `rust/scripts/check-test-hygiene.mjs`（node 脚本，对齐 `check-zero-io.mjs` 惯例：扫 `rust/crates/**` 的 test cfg，报告 (a) 缺 `// layer:` 标签的 tests/ 文件 (b) test 代码内裸 `std::env::set_var` / `remove_var`（无 `mirri_test_support` 守卫上下文的）(c) test 代码内裸 `thread::sleep`）
- Modify: 根 `package.json`（若 check-zero-io 有对应 script 条目，加 `gate:rust-test-hygiene` 同款）

**Interfaces:**
- Produces: `node rust/scripts/check-test-hygiene.mjs`，存量违规项先以 baseline 白名单文件形式容忍（`rust/scripts/test-hygiene-baseline.json`，记录 file:count，新违规 = 新增即 exit 1），避免一次性全量返工。

- [ ] **Step 1: 写 AGENTS.md 章节**（逐字从设计 §4.5 + §4.1 表格搬入，`// layer: T<n>` 标签规则一并写入）。
- [ ] **Step 2: 写脚本 + baseline**（扫描逻辑从简：正则 + `#[cfg(test)]`/`tests/` 路径识别；跑一次生成 baseline JSON 并提交；存量 `// layer:` 标签缺失项也在 baseline 内豁免，R3 移动批顺手回填）。
- [ ] **Step 3: 验证** `node rust/scripts/check-test-hygiene.mjs` exit 0（baseline 内豁免）。
- [ ] **Step 4: 设计回写**：在 `16-test-execution-strategy.md` §4.5 第 7 条追加 SDR-016.3（机械守卫主门禁 = node 扫描脚本 + baseline；clippy disallowed-methods 记为 ROADMAP 增量项，理由见计划偏离声明 1）。
- [ ] **Step 5: Commit** `git commit -m "docs(rust): add test iron rules to AGENTS.md + hygiene guard script"`

---

### Task 4: R2a — mirri-core 锁收敛 + 生产 HOME 读注入化

**Files:**
- Modify: `rust/crates/mirri-core/src/flags.rs`（文件级 ENV_LOCK → `mirri_test_support::env::lock()`）
- Modify: `rust/crates/mirri-core/src/tools/agent_swarm_tool.rs`（`MIRRI_AGENT_SWARM_MAX_ITEMS` 的本地静态锁 :1699 → `env::lock()`）
- Modify: `rust/crates/mirri-core/src/scope/workspace_scope.rs`（`home_lock()` 删除，写点换 `EnvGuard`；`resolve_*_from` 注入化 `:136,362` 两处生产 HOME 读）
- Modify: `rust/crates/mirri-core/Cargo.toml`，**并同批为以下全部后续消费 crate 加 dev-dependencies**（Task 5/6 禁碰 Cargo.toml）：`mirri-embedded`、`mirri-acp`、`mirri-os`、`mirri-credentials`、`mirri-mcp`、`mirri-test-harness`，条目 `mirri-test-support = { path = "../mirri-test-support" }`（路径按各 crate 相对位置调整）。

**Interfaces:**
- Produces: `workspace_scope` 生产路径新增带 `home_dir: Option<&Path>` 参数的解析变体（默认无参版本保留，内部走 env；测试走注入版）——具体签名以现有 `resolve_*_from` 模式（`mirri-os/src/home.rs:42,51`）为准，实现者先读该范本再对齐。

- [ ] **Step 1: 读范本** `rust/crates/mirri-os/src/home.rs:30-70`，理解 `resolve_*_from` 惯用法。
- [ ] **Step 2: 生产 HOME 读注入化**（两处调用点加参数穿透，TDD：新测试直接传 fake home 断言解析结果）。
- [ ] **Step 3: 锁替换**（flags.rs 的 static ENV_LOCK 删除，所有 `ENV_LOCK.lock()` → `mirri_test_support::env::lock()`；workspace_scope 的 home_lock 同理，HOME set/restore 换 `EnvGuard`）。
- [ ] **Step 4: 验证** `cargo test -p mirri-core --manifest-path rust/Cargo.toml`（mirri-core 测试体量大，允许 ~10 分钟后台跑）+ clippy。
- [ ] **Step 5: Commit** `git commit -m "refactor(core): converge env locks to mirri-test-support; inject home resolution"`

---

### Task 5: R2b — mirri-embedded 锁收敛 + 裸 set_var 消除

**Files:**
- Modify: `rust/crates/mirri-embedded/src/bootstrap.rs`（test_env::home_lock → test-support；:543,963,1067 生产 HOME 读注入化——config_factory.rs:295 回退链同批）
- Modify: `rust/crates/mirri-embedded/src/providers/mod.rs` + `config_factory.rs`（ENV_LOCK → test-support）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`（flag env 无锁写 :26500-26501/:41805-41810 → EnvGuard；`MCP_TEST_*` :18057-18114 → EnvGuard）
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests/credential_store_tests.rs`（锁换 test-support）
- Modify: `rust/crates/mirri-embedded/Cargo.toml`（dev-dep 加 mirri-test-support）

- [ ] **Step 1: 生产读注入化**（bootstrap 3 处 + config_factory 回退链，TDD 同 Task 4 模式）。
- [ ] **Step 2: 锁替换 + 裸写收编**（providers ENV_LOCK、home_lock、credential_store、`MCP_TEST_*`、flag env、以及 `embedded_server/tests.rs:22479` 的 `MANAGED_PLUGIN_ENV_LOCK`——逐一把本地静态锁定义删除，全部换 `EnvGuard`/`env::lock()`）。
- [ ] **Step 3: 验证** `cargo test -p mirri-embedded --manifest-path rust/Cargo.toml`（后台，~15 分钟预算）+ clippy。
- [ ] **Step 4: Commit** `git commit -m "refactor(embedded): converge env locks; inject home resolution"`

---

### Task 6: R2c — acp / os / persistence / credentials / mcp / harness 锁收敛

**Files（每个 crate 同模式：删本地锁 → test-support；Cargo.toml 加 dev-dep）：**
- `rust/crates/mirri-acp/tests/desktop_share_e2e.rs`（acquire_env_lock 贯穿全程的：改为 `Command::env` 子进程注入优先，确需全局的换 `env::lock()` 且收窄到 set/restore 区间，不跨 await）
- `rust/crates/mirri-acp/src/desktop_probe.rs:162`、`rust/crates/mirri-acp/src/run.rs:569`
- `rust/crates/mirri-os/src/search.rs:1308`（注意现存 async 锁；统一为同步锁 + 不跨 await 用法审查）
- `rust/crates/mirri-credentials/src/migration_tests.rs:20`
- `rust/crates/mirri-core/tests/terminal_pool_provider_tests.rs:18`、`turn_flow_tests.rs:7761`
- `rust/crates/mirri-embedded/tests/e2e_real_llm.rs:21`
- `rust/crates/mirri-test-harness/tests/cross_backend_contract.rs`（shared_home OnceLock set_var → `home::temp_home()` per-test + 或保留 shared 语义但经 EnvGuard；mirri-test-harness dev-dep 加 test-support，确认依赖无环）
- `rust/crates/mirri-mcp/tests/config_tests.rs:460,530`、`connection_tests.rs:409`（唯一变量名裸写也收编 EnvGuard，消除「理论安全」）

- [ ] **Step 1: 逐文件替换**（每文件改完即跑该文件所属 crate 的相关 test target，快速反馈）。
- [ ] **Step 2: 验证** 受影响 6 个 crate 各跑一次测试（可并行后台）+ clippy。
- [ ] **Step 3: Commit** `git commit -m "refactor(tests): converge all env locks to mirri-test-support across crates"`

---

### Task 7: R2 验收 — 卫生扫描 + 压力复跑

- [ ] **Step 1: 扫描零残留** `node rust/scripts/check-test-hygiene.mjs` 应报 0 个**非 baseline** 违规；然后收窄 baseline（已修复项从 JSON 移除直至剩 sleep 类存量）。
- [ ] **Step 2: 压力验证（5×）**：对本批（Task 4-6）改动过的**全部**测试目标——mirri-core lib、mirri-embedded lib、mirri-acp、mirri-credentials、mirri-mcp、mirri-test-harness、mirri-os——各以默认 `--test-threads` 重复跑 5×（后台并行、每 crate 独立），无红。（nightly 可用时补一次 `-Z unstable-options --randomize`，无 nightly 注明跳过。）
- [ ] **Step 3: ROADMAP 回写**：R2 批次状态记入 `rust-core-design/ROADMAP.md`。
- [ ] **Step 4: Commit（若 baseline 收窄产生了 diff）**

---

### Task 8: R3a — mirri-core 27 文件合并（批次 1：session 域）

**Files:**
- Create: `rust/crates/mirri-core/tests/session_group.rs`（域组二进制）
- Create: `rust/crates/mirri-core/tests/common/mod.rs`（共享 setup，先最小化——只放实际被复用的搭建；不预先抽象）
- Move: session 相关的 `tests/*.rs` → `tests/session_group.rs` 内 `mod 文件名 {}` 包裹（**纯移动**：文件内容原样进 mod 块，不改 use、不改逻辑）

**机械规则（写死，Task 9/10 沿用同文）**：
1. 目标文件列表先经 static 状态扫描（判据见 Global Constraints「static 状态扫描的可判定判据」）；有嫌疑的文件移入「暂缓合并清单」。
2. 移动 = 剪切文件全文 → 包进 `mod <原文件名> { … }` → 组二进制 `mod` 声明区。
3. 原文件内 `use super::` / 相对路径若因嵌套 mod 失效，用 `use crate::…` 最小修正（仅此允许的「改动」）。
4. 每文件移动后立即 `cargo test -p mirri-core --test session_group` 确认无编译错。
5. 移动时在 mod 块头部加 `// layer: T1`（或该组实际层级）标签——存量标签回填的落地机制。

- [ ] **Step 1: 扫描 + 定批次文件清单**（session 相关约 8-10 文件）。
- [ ] **Step 2: 逐文件机械移动**（每 2-3 文件跑一次组测试）。
- [ ] **Step 3: 全量验证** `cargo test -p mirri-core --manifest-path rust/Cargo.toml` 后台 + clippy。
- [ ] **Step 4: Commit** `git commit -m "test(core): consolidate session-domain test binaries (pure move)"`

---

### Task 9: R3b — mirri-core 合并（批次 2：tools/scope 域）

**Files:**
- Create: `rust/crates/mirri-core/tests/tools_scope_group.rs`
- Move: tools/scope 相关的 `tests/*.rs` → 组二进制内 `mod 文件名 {}` 包裹（纯移动）

**机械规则（与 Task 8 完全一致，逐条执行）**：
1. 目标文件列表先经 static 状态扫描（判据见 Global Constraints「static 状态扫描的可判定判据」）；有嫌疑的文件移入暂缓合并清单。
2. 移动 = 剪切文件全文 → 包进 `mod <原文件名> { … }` → 组二进制 `mod` 声明区。
3. 原文件内 `use super::` / 相对路径若因嵌套 mod 失效，用 `use crate::…` 最小修正（仅此允许的改动）。
4. 每文件移动后立即 `cargo test -p mirri-core --test tools_scope_group` 确认无编译错。
5. 移动时在 mod 块头部加 `// layer: T1` 标签（按该组测试实际层级，通常 T1）。

- [ ] **Step 1: 扫描 + 定批次文件清单**（tools/scope 相关文件）。
- [ ] **Step 2: 逐文件机械移动**（每 2-3 文件跑一次组测试）。
- [ ] **Step 3: 全量验证** `cargo test -p mirri-core --manifest-path rust/Cargo.toml` 后台 + clippy。
- [ ] **Step 4: Commit** `git commit -m "test(core): consolidate tools/scope-domain test binaries (pure move)"`

---

### Task 10: R3c — mirri-core 合并（批次 3：terminal/misc 域）+ 其他 crate 收敛

**Files:**
- Create: `rust/crates/mirri-core/tests/terminal_misc_group.rs`
- Move: mirri-core 剩余 `tests/*.rs`（机械规则同 Task 8 五条，逐条执行）
- Modify: 其余 crate（mirri-server 已有 common 可参照、mirri-os、mirri-cli、mirri-mcp、mirri-remote、mirri-kosong、mirri-transport、mirri-collaboration 等）>3 个 `tests/*.rs` 的按域合并（同机械规则）

- mirri-core 收敛后 `tests/` 二进制数 ≤3（暂缓合并清单文件除外，须在任务报告里列明原因）。
- **不冗余人工过目**：合并完成后逐组检查无同一行为被两个二进制重复覆盖（设计验收 6）。
- 验证：全 workspace `cargo test --workspace --locked`（后台）+ clippy。
- ROADMAP 回写：R3 批次状态记入 ROADMAP。
- Commit: `test(workspace): consolidate integration test binaries to ≤3 per crate`

---

### Task 11: R4a — embedded_server/tests.rs sleep 治理（98 处）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests.rs`
- Modify: baseline（若仍有裸 sleep 未治理项，逐条注明理由后保留在 baseline）

**策略（按优先级）**：
1. 「等待某个状态出现」型固定 sleep → `wait_for_*` 轮询（状态断言化）。
2. 「推进虚拟时间」型（通知/重试/超时语义）→ `start_paused` + `tokio::time::advance`（范本：同 crate `message_handlers.rs` 已有 10 处 paused 用法可参照）。
3. 真实子进程/IO 完成型（少数）→ 保留 + `// hygiene-allow: real-io wait` 注释。
- [ ] 分片治理（每 20-30 处一片，每片后跑 embedded lib 测试目标相关子集 `cargo test -p mirri-embedded --lib`，后台），全绿再下一片。
- [ ] Commit: `test(embedded): replace fixed sleeps with wait_for/paused clock (batch)`

---

### Task 12: R4b — b2_notification_tests.rs sleep 治理（91 处）

**Files:**
- Modify: `rust/crates/mirri-embedded/src/embedded_server/tests/b2_notification_tests.rs`
- Modify: baseline（未治理项逐条注明理由后保留）

**策略（与 Task 11 完全一致，按优先级）**：
1. 「等待某个状态出现」型固定 sleep → `wait_for_*` 轮询（状态断言化）。
2. 「推进虚拟时间」型（通知/重试/超时语义）→ `start_paused` + `tokio::time::advance`（范本：同 crate `message_handlers.rs` 已有 paused 用法可参照）。
3. 真实子进程/IO 完成型（少数）→ 保留 + `// hygiene-allow: real-io wait` 注释。

- [ ] 分片治理（每 20-30 处一片，每片后跑 `cargo test -p mirri-embedded --lib b2`，后台），全绿再下一片。
- [ ] Commit: `test(embedded): replace fixed sleeps in b2 notification tests with wait_for/paused clock`

---

### Task 13: R4c — 其余结构性慢点（terminal_pool_tests 24 处等）

- `mirri-core/tests/terminal_pool_tests.rs`（或已并入组二进制）的 `sleep(200ms)+5s deadline` 模式 → 统一 wait_for helper。
- 其余 crate 的 sleep 仅处理 >300ms 的固定值项；小额步进保留 + baseline 豁免注明。
- Commit: `test(workspace): convert structural sleeps to wait_for polling`

---

### Task 14: 终验收 — 全量三连跑 + 报告数据

- [ ] **Step 1: 卫生终扫** `node rust/scripts/check-test-hygiene.mjs`，baseline 中 sleep 类存量条目逐条有理由注释。
- [ ] **Step 2: clippy 全量** `cargo clippy --workspace --all-targets -- -D warnings`（后台）。
- [ ] **Step 3: zero-io 门禁** `node rust/scripts/check-zero-io.mjs` exit 0（防 Task 4/5 注入化时误引入）。
- [ ] **Step 4: 全量三连跑**：连续跑 3 次 `cargo test --workspace --locked`（后台、每次记录 wall time）。口径：若起跑时缓存已热（前序任务刚跑过测试），三次**全部计入**且均须全绿 ≤7:00；若首次明显含冷编译（wall time 远超后续两次），第 1 次不计入、第 2、3 次须全绿 ≤7:00。注意 Step 2 的 clippy 与本步共享增量缓存，「第 1 次触缓存」不成立是常态。
- [ ] **Step 5: 汇总数据**（基线 vs 终态耗时、二进制数 84→N、锁 14→1、sleep 治理数、baseline 余量）进最终报告。
- [ ] **Step 6: Commit** `docs: record test strategy rollout metrics`（数据进 context.md §16 追加「终态实测」小节或 ROADMAP）。

---

## Parallelization 裁决

- Task 1→2→3 串行（同 crate 递进）。Task 3 与 Task 4 可并行（不同文件域）。
- **Task 4 → 5 → 6 严格串行**（Cargo.toml dev-dep 与 baseline 文件域冲突；Task 4 一次性为全部消费 crate 加 dev-dep，Task 5/6 禁碰 Cargo.toml，baseline 只在 Task 7 regenerate）。
- Task 8→9→10 串行（同 crate 文件移动）；Task 11/12 可并行（不同文件）；Task 13 依赖 8/9/10 完成。
- Task 14 严格最后。

## 风险预案

- mirri-embedded lib 测试目标若在锁收敛后出现新 flake：优先怀疑跨 await 持锁违规（Task 6 的 search.rs async 锁统一），回滚该文件单独排查。
- R3 合并后组测试出现「顺序相关失败」：说明有未扫出的 static 依赖，把涉事文件从组二进制拆回独立文件（暂缓清单机制兜底）。
- 7 分钟仍不达标：按设计 §5 后备杠杆启用 `[profile.test]` 调优（debug=0 仅对 test profile），作为 R5 增量批。
