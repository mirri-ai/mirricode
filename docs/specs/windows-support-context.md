# Windows 平台支持域 — 现状盘点（context）

> 快照日期：2026-10-25（`date` 实测）。本文只记录**代码事实与实测证据**，不含目标、不含裁决（开放项与处置见 [windows-support-ROADMAP.md](windows-support-ROADMAP.md)）。
> 行号以快照日代码为准，随代码演进漂移。
> **取证边界**：除标注「2026-10-25 实测」的条目外，其余结论来自本机（macOS）读代码 + 读 CI 配置。**未在 Windows 真机或 Windows CI 上执行过任何一步**。凡依赖运行时行为的判断在 §3 单列，不混入 §2。

## 1. 构建与测试覆盖矩阵

| 环节 | Windows 是否覆盖 | 代码事实 |
|---|---|---|
| 引擎测试 | **否** | `.github/workflows/ci.yml:91` `rust-test: runs-on: ubuntu-latest`——`cargo test --workspace` 与 clippy 只在 Linux 跑 |
| 桌面端打包 | 是 | `.github/workflows/desktop-build.yml:83-85`（`windows-2025-vs2026` / `win32-x64` / `x86_64-pc-windows-msvc`） |
| 桌面端测试 | **否** | 同一 job 只有 `Build & package desktop app`（`:196`），全文无 `pnpm test` / `vitest` / `playwright` 调用 |
| 原生二进制构建 | 是 | `.github/workflows/rust-native-build.yml:74-81`（`win32-x64`、`win32-arm64`） |

结论（事实陈述）：Windows 只验证「编译得过、打得成包」，不验证「跑起来对不对」。

## 2. 代码可确认的缺陷

### 2.1 数据目录解析无 `USERPROFILE` 兜底 — ✅ 已修复（2026-10-25）

**实测证据（2026-10-25，用户 Windows 真机，Git Bash + CLI 启动 `mirri web`）**：`HOME is not set` 复现，进程退出。该项定性为阻断级，已在同日修复。

修复前的代码事实（`rust/crates/mirri-core/src/config/home.rs`，修复前 `:22-29`）：

1. `MIRRICODE_HOME`（非空则原样采用）
2. `HOME`（缺失即返回 `EngineError::RequestInvalid("HOME is not set; ...")`）

无 `USERPROFILE` 兜底。对比：同仓库 `rust/crates/mirri-core/src/agents_md.rs:40` 与 `rust/crates/mirri-os/src/search.rs:304` 都用了 `dirs::home_dir()` 兜底，即这两处**不是**同一套解析。

修复：`resolve_mirri_home_dir_from` 在 `HOME`（过滤空值）之后补 `USERPROFILE`（过滤空值）兜底，`HOME` 优先顺序不变。测试覆盖见 `rust/crates/mirri-core/src/config/tests.rs`（仅 `USERPROFILE`、`HOME` 优先、空 `USERPROFILE` 三个用例）。

### 2.7 wire.jsonl 追加日志 create 截断在 Windows 报 os error 5 — ✅ 已修复（2026-10-25）

**实测证据（2026-10-25，用户 Windows 真机，Git Bash + CLI 启动 `mirri web` 发消息）**：session 目录创建成功、`state.json` 写入成功、`wire.jsonl` 创建成功但 0KB，消息写入报「拒绝访问 os error 5」。

根因（代码事实 + Windows API 语义推演，与实测现象完全吻合）：修复前 `rust/crates/mirri-persistence/src/append_log.rs` 的 `JsonlAppendLog::create`（`:162-186`）先以 `OpenOptions::append(true)` 打开句柄，再对该句柄调 `set_len(0)` 模拟 truncate。Windows 上 append 模式的句柄只被授予 `FILE_APPEND_DATA`（无 `FILE_WRITE_DATA`），而 `set_len` 的底层 `SetEndOfFile` 要求 `FILE_WRITE_DATA` → 恒返回 `ERROR_ACCESS_DENIED (os error 5)`。故：

- 文件由 open 步骤成功创建（解释「文件创建了」）；
- `set_len(0)` 失败使 create 整体报错，且截断对 0KB 文件本是 no-op，报错点紧跟在 open 之后（解释「0KB」）。

Unix 上 `O_APPEND` 句柄的 `ftruncate` 完全合法，故该缺陷只影响 Windows。

修复：`create` 改为两步 open——先 `write(true).create(true).truncate(true)`（截断由 open 的 TRUNCATE 标志完成，Windows 合法），再以 `append(true)` 重开持有 O_APPEND 语义的写入句柄（保留「任何写入永远落在文件末尾」的防覆盖语义，见该函数 doc comment）。回归测试：`should_truncate_a_preexisting_file_and_keep_appending_when_create_reopens_a_log`。

### 2.2 rg 缓存兜底在 Windows 上恒不命中

`rust/crates/mirri-os/src/search.rs:305` 拼缓存路径为 `home.join("bin").join("rg")`——无平台后缀。

同仓库 `docs/en/configuration/data-locations.md` 明确该缓存 Windows 位置是 `bin/rg.exe`（`bin/` 一节：「managed ripgrep binary for Grep (`rg.exe` on Windows)」）。故 `is_executable_file`（`:309-327`）在 Windows 上对 `bin/rg` 恒为 false，`resolve_rg_path`（`:278-288`）落到裸 `"rg"`。

连带：`which_exists_on_path("rg")`（`:291-297`）以裸名 `rg` 拼路径，Windows 上 `PATH` 里是 `rg.exe`，该快速路径恒 false。此分支本身无害（`Command::new("rg")` 仍按 `PATHEXT` 解析），但说明该函数只在 Unix 上被验证过。

用户可见后果：这恰是 `grep-glob-capability` changeset 声称要解决的场景（「PATH 里没有 rg 时回退到 `~/.mirri-code/bin/rg`」），在 Windows 上不成立。

### 2.3 Bash 工具在 Windows 走 `cmd /C`，但系统提示词与文档称走 Git Bash

| 位置 | 内容 |
|---|---|
| `rust/crates/mirri-core/src/tools/bash.rs:495-498` | `#[cfg(windows)] let (shell, shell_flag) = ("cmd", "/C");` |
| `rust/crates/mirri-core/src/tools/bash.rs:745-748` | 同一处硬编码（后台/第二条执行路径） |
| `rust/crates/mirri-core/src/scope/system_prompt.rs:57` | `WINDOWS_NOTES`：「The Bash tool runs through Git Bash, so use Unix shell syntax ... `/dev/null` not `NUL`」 |
| `docs/en/reference/tools.md:47` | 「On Windows, Git Bash is used by default.」 |
| `docs/en/configuration/env-vars.md:127` | `MIRRICODE_SHELL_PATH`：「Override the Git Bash path on Windows」 |

`MIRRICODE_SHELL_PATH` 的实际消费面是交互终端池（`rust/crates/mirri-os/src/shell.rs:26-70` 的 `resolve_shell` / `default_shell`），**不经过** Bash 工具。

后果（按代码推演，未在 Windows 实测）：模型被告知用 Unix 语法，实际执行在 `cmd` 中，`/dev/null`、`$(...)`、单引号、`&&` 之外的分隔符等会产生不同行为或直接失败。该不一致为**既存问题**，非本次 `feat/rust-core-parity-batch` 引入。

### 2.8 Windows Shell 工具语义统一（WO-3 + WO-2）— ✅ 已实施（2026-10-26）

§2.2 / §2.3 所述缺陷已于 2026-10-26 批次修复，实现事实：

- `bash.rs` 的两处 `#[cfg(windows)]` 硬编码 `cmd /C` 分支已删除，改为经 `mirri_os::shell::git_bash_for_bash_tool` 解析 Git Bash 并以 `-c` 执行；解析失败时返回明确的可操作错误（指引安装 Git for Windows 或设置 `MIRRICODE_SHELL_PATH`），不再静默回落 cmd。
- 解析链下沉 `rust/crates/mirri-os/src/shell.rs`：参数化纯函数 `resolve_git_bash_from`（`MIRRICODE_SHELL_PATH` 非空校验 → 已知位置 → PATH，四变体结果）+ 成功结果缓存；WSL 的 `System32\bash.exe` 通过规范化（小写化）后的 `SystemRoot` 前缀比较排除。
- 新增 Windows-only PowerShell 工具 `rust/crates/mirri-core/src/tools/powershell.rs`：诚实退出码 + UTF-8 输出，复用 Bash 工具的前台执行骨架；**仅前台**。
- 已知限制：PowerShell 工具不支持 `run_in_background`、超时不转后台；profile 白名单未包含 PowerShell；`MIRRICODE_SHELL_PATH` 指向 pwsh 时 Bash 工具会按非 bash 家族明确报错。
- 真机判据（Windows 上）见 `docs/specs/windows-shell-tools.md` §4，登记在 ROADMAP 待复验。

## 3. 待验证（依赖运行时，需实测）

### 3.1 桌面端在 Windows 上能否拉起引擎 — ✅ 已验证（2026-10-25）

实测确认：Git Bash + CLI 启动路径复现 `HOME is not set`（见 §2.1），桌面端从开始菜单启动的 `process.env` 场景按同一根因处理（修复已覆盖所有入口）。

### 3.2 归档压缩的 `temp + rename` 在 Windows 的可用性

`rust/crates/mirri-persistence/src/compress.rs` 的压缩与恢复均走 `temp 文件 → fsync → rename → 删源`（`:61, 109-122, 138-172`）。全文无 Windows 特判、无 sharing-violation 重试。

Windows 上两处语义差异需实测：

- 目标文件被另一进程打开时 `rename` 会失败（`FILE_SHARE_DELETE` 存在但非所有句柄都带）；
- 目录被当作工作目录占用时不能改名/删除。

Rust std 打开文件默认带 `FILE_SHARE_DELETE`，理论上可行，但「压缩中同时读取/恢复/删除同一会话」的组合未验证。

### 3.3 技能热重载在 Windows 的行为

`ReadDirectoryChangesW` 为路径递归，与 macOS FSEvents 同类语义（嵌套新建的文件会上报）。本批新增的「目录事件也触发」在 Windows 上预期只是多一两次被空 diff 门吸收的重扫。

未验证项：`notify` 的 Windows 后端对「新建目录后立即写入」是否稳定上报，以及 `--recursive` 下的重命名事件形状。

## 4. 已验证无平台风险的部分（不必重复排查）

- **目录级跨进程锁**：`rust/crates/mirri-persistence/src/dir_lock.rs:1-25` 基于 Rust 1.89 的 `File::try_lock`，flock / `LockFileEx` 双实现。
- **事件投递分级、todo 持久化、System Prompt 面板、归档管理页**：前端 + 协议层，无平台分支。
- **`Glob`**：`ignore`/`globset` 进程内运行，不依赖外部二进制。

## 5. 与既存台账的关系

`docs/specs/rust-core-design/ROADMAP.md:91` 已记录「ripgrep 分发兜底未对齐」并给出 P1/P2 分级，其中 P1（rust 侧读缓存兜底）已随 `grep-glob-capability` 交付。本文件 §2.2 是该 P1 交付物的**平台侧缺陷**（Windows 后缀缺失），不是新问题。
