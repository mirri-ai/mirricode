# Windows 平台支持域 — 开放项与处置（ROADMAP）

> 状态：活文档（随执行推进更新） · 建立于 2026-10-25 ·
> 本文是该域**执行状态与开放项的唯一源**（`docs/specs/AGENTS.md`：设计文档不持有进度，开放项都在这里维护）。
> 现状盘点（代码事实、行号、取证边界）：[windows-support-context.md](windows-support-context.md)。
> 上游背景：产品以桌面端为首发形态，Windows 为主要目标平台之一（`WO` 系列来自 2026-10-25 一次针对 Windows 可用性的代码审查）。

## 0. 结论摘要

Windows 目前只验证「编译得过、打得成包」，没有任何自动化验证覆盖「跑起来对不对」（事实见 [windows-support-context.md](windows-support-context.md) §1）。审查发现 **4 项代码可确认缺陷**、**2 项待实测风险**。

优先级判断：**WO-1 已实测确认为阻断级**（2026-10-25 用户 Windows 实测：`HOME is not set`）；**WO-7 同批实测确认**（session 写入报 `os error 5`）。两者修复已落地，待 Windows 复验。

## 1. 开放项台帐

### WO-1 数据目录解析无 `USERPROFILE` 兜底 — 阻断级 ✅ 已修复（待 Windows 复验）

| 字段 | 内容 |
|---|---|
| 类型 | 代码可确认缺陷 · **2026-10-25 实测确认**（用户 Windows 真机复现 `HOME is not set`） |
| 位置 | 解析规则现位于 `rust/crates/mirri-os/src/home.rs`（2026-10-26 收敛后唯一源，见 §3.1）；engine 侧包装 `rust/crates/mirri-core/src/config/home.rs` |
| 处置 | 已按选项 A 实施并随后收敛：`HOME`(非空) → `USERPROFILE`(非空) 兜底，`HOME` 优先顺序不变；五个原分散解析点全部委托 `mirri_os::home` |
| 验证 | `rust/crates/mirri-os/src/home.rs` 10 个参数化用例（优先级、空值、双 HOME/USERPROFILE）；`mirri-core/src/config/tests.rs` 保留 EngineError 映射用例 |
| 状态 | **修复已合入**，待 Windows 真机复验引擎可启动 |

### WO-1-V 验证 Windows 桌面端能否拉起引擎 — ✅ 已完成（2026-10-25）

用户在 Windows 真机（Git Bash + CLI 启动 web）实测：`HOME is not set` 复现，WO-1 定性为阻断级。同场测试发现新缺陷 WO-7。

### WO-7 wire.jsonl 写入报 os error 5（ACCESS_DENIED）— 阻断级 ✅ 已修复（待 Windows 复验）

| 字段 | 内容 |
|---|---|
| 类型 | 代码可确认缺陷 · **2026-10-25 实测确认**（Windows：wire.jsonl 创建成功但 0KB，append 报 os error 5） |
| 位置 | `rust/crates/mirri-persistence/src/append_log.rs` `JsonlAppendLog::create`（修复前 `:162-186`） |
| 根因 | 旧实现先以 `append(true)` 打开句柄，再 `set_len(0)` 截断。Windows 上 append 句柄仅有 `FILE_APPEND_DATA` 权限，而 `SetEndOfFile` 要求 `FILE_WRITE_DATA` → 恒返回 `ERROR_ACCESS_DENIED (os error 5)`。与实测证据完全吻合：文件由 open 创建（成功、0KB），随后 truncate 失败使 create 中断，wire log 不可用 |
| 处置 | `create` 改为两步：先 `write(true).create(true).truncate(true)` 打开（截断由 open 的 TRUNCATE 标志完成，Windows 上合法），再以 `append(true)` 重开持有 O_APPEND 语义的句柄。Unix 行为不变 |
| 验证 | `append_log.rs` tests 新增 `should_truncate_a_preexisting_file_and_keep_appending_when_create_reopens_a_log`（锁「create 截断旧文件 + append 正常」回归语义；Windows 上的 os error 5 由真实平台行为本身守卫） |
| 状态 | **修复已合入**，待 Windows 真机复验发消息后 wire.jsonl 正常增长 |

### WO-2 rg 缓存兜底缺 `.exe` 后缀 — 功能降级 ✅ 已修复（待 Windows 复验）

| 字段 | 内容 |
|---|---|
| 类型 | 代码可确认缺陷 |
| 位置 | `rust/crates/mirri-os/src/search.rs:305`（`join("bin").join("rg")`）、`:291-297`（`which_exists_on_path`） |
| 现象 | Windows 安装器缓存的是 `bin/rg.exe`，代码拼 `bin/rg`，`is_executable_file` 恒 false，回退逻辑失效 |
| 影响 | `grep-glob-capability` changeset 承诺的「PATH 无 rg 时回退缓存」在 Windows 不成立；用户装过产品但 PATH 无 rg 时 Grep 不可用 |
| 处置 | 已修复：缓存路径按平台拼后缀（`rg` / `rg.exe`），`which_exists_on_path` 在 Windows 上同时探测 `name` 与 `name.exe`；测试在 `mirri-os/src/search.rs`（参数化覆盖后缀探测） |
| 状态 | **修复已合入**，待 Windows 真机复验 Grep 回退缓存可用 |

### WO-3 Bash 工具平台语义与提示词/文档不一致 — 影响 Agent 可用性 ✅ 已实施（待 Windows 真机判据，见 [windows-shell-tools.md](windows-shell-tools.md) §4）

| 字段 | 内容 |
|---|---|
| 类型 | 既存缺陷（非本批引入） |
| 位置 | 代码 `rust/crates/mirri-core/src/tools/bash.rs:495-498`、`:745-748`（`cmd /C`）；提示词 `rust/crates/mirri-core/src/scope/system_prompt.rs:57`；文档 `docs/en/reference/tools.md:47`、`docs/en/configuration/env-vars.md:127` |
| 现象 | 三处（提示词 + 两份文档 + `MIRRICODE_SHELL_PATH` 的文档语义）声称 Windows 走 Git Bash，代码实际 `cmd /C` |
| 影响 | 模型按 Git Bash 语法写命令（`/dev/null`、`$(...)`、单引号），实际在 cmd 下执行 → 命令失败或行为不同 |
| 处置 | 已按选项 A 实施并扩展为完整方案（spec `windows-shell-tools.md`，WO3-R1–R9）：Git Bash 解析链下沉 `mirri-os/src/shell.rs`（参数化纯函数 `resolve_git_bash_from` + 生产包装）；`bash.rs` 两处 `#[cfg(windows)]` 改为经 `mirri_os::shell::git_bash_for_bash_tool` 解析并以 `-c` 执行，解析不到时明确报错并附可操作指引；新增 Windows-only PowerShell 工具（`tools/powershell.rs`，诚实退出码 + UTF-8）；`WINDOWS_NOTES` 与三处用户文档同步更新 |
| 状态 | **实现已合入**，待 Windows 真机按 spec §4 判据复验 |
| 备注 | 后续项：<br>・PowerShell 工具后台（`run_in_background`）支持<br>・PowerShell 工具超时转后台（timeout-to-background）接管 |

### WO-4 归档压缩 rename 语义在 Windows 未验证 — 潜在故障

| 字段 | 内容 |
|---|---|
| 类型 | 待实测 |
| 位置 | `rust/crates/mirri-persistence/src/compress.rs`（`:61, 109-122, 138-172`） |
| 风险 | 目标被占用时 `rename` 失败；目录作 CWD 时不可改名。全文无 Windows 特判、无 sharing-violation 重试 |
| 验证 | Windows 上开启 `[retention]`，构造「压缩进行中同时读取 / 恢复 / 删除同一会话」；比对 `mirri-persistence` 相关用例在 Windows 上的结果 |
| 处置预案 | 若失败，在 rename 外包一层有界重试（sharing violation 是瞬态通常可重试），并把失败路径的清理语义（临时文件不残留）一并覆盖 |
| 阻塞 | 需 Windows 环境 |

### WO-5 技能热重载在 Windows 未验证 — 低风险

| 字段 | 内容 |
|---|---|
| 类型 | 待实测（低优先级） |
| 位置 | `rust/crates/mirri-embedded/src/skill_watcher.rs` |
| 预期 | `ReadDirectoryChangesW` 递归语义与 FSEvents 同类，本批「目录事件也触发」的修复在 Windows 上只是多一两次被空 diff 门吸收的重扫 |
| 验证 | Windows 上跑 `skill_watcher::tests` 与 `embedded_server` 的三个 hot-reload e2e 用例 |
| 备注 | 若要在 CI 上长期守住，见 WO-6 |

### WO-6 Windows 无任何自动化测试覆盖 — 结构性缺口

| 字段 | 内容 |
|---|---|
| 类型 | 基础设施 |
| 现状 | `ci.yml:91` `rust-test` 只在 ubuntu-latest；`desktop-build.yml` 的 Windows job 只打包不测试（见 [windows-support-context.md](windows-support-context.md) §1） |
| 选项 | **A（推荐，成本低）** 新增一个 `rust-test-windows` job，跑 `cargo test --workspace` 的**子集**（`mirri-os`、`mirri-persistence`、`mirri-core` 的平台敏感模块），避免全量 workspace 在 Windows 上的时长与抖动<br>**B** 加一个 Windows 桌面端 smoke job：装包 → 启动 → 断言 healthz 通（正好覆盖 WO-1）<br>**C** 只保留文档警示，不投入 CI |
| 建议 | A + B 组合。B 的判据最直接（能挡住 WO-1 这类「装完起不来」），A 才能长期守住平台语义 |
| 备注 | 引入 Windows job 前需确认 GitHub 上 `windows-2025-vs2026` runner 的可用配额与时长预算 |

## 2. 执行顺序建议

1. ~~**WO-1-V**~~（✅ 2026-10-25 完成：阻断确认）
2. ~~**WO-1 / WO-7**~~（✅ 修复已落地：同一批交付，待 Windows 真机复验）
3. ~~**WO-2**~~（✅ 2026-10-26 已修复，随 Windows Shell 工具批次交付，待 Windows 复验）
4. ~~**WO-3**~~（✅ 2026-10-26 已实施：Git Bash 解析 + PowerShell 工具，待 Windows 真机判据）
5. **WO-4、WO-5**（随 Windows 环境可用时验证）
6. **WO-6**（结构性投入，建议在 1-4 收敛后立项；WO-7 的教训进一步支持「Windows 必须有真测试」）

## 3. 不在本域范围

- 通用 `HOME` 语义本身（POSIX 约定）——仅登记「缺 `USERPROFILE` 兜底」这一实现缺口。
- 平台无关功能的逐项复核——已确认无风险的部分见 [windows-support-context.md](windows-support-context.md) §4。
- macOS / Linux 平台差异（含 FSEvents vs inotify 的事件语义）——已由 `debugging-file-watchers` skill 沉淀为通用排查方法，不在本台账重复。

## 3.1 平台差异化 home 解析的统一（2026-10-26 完成）

WO-1 的根因之一是「home 解析散落四处、各自实现」。2026-10-26 已完成收敛：全仓库 OS 差异化的 home 解析统一到 **`rust/crates/mirri-os/src/home.rs`** 唯一接口（`os_home_dir` / `mirri_home_dir` + 参数化 `resolve_*_from`），语义统一为「`MIRRICODE_HOME`(非空) → `HOME`(非空) → `USERPROFILE`(非空)」。四个原分散点全部改为委托：`mirri-core/src/config/home.rs`（仅保留 `EngineError` 映射）、`mirri-core/src/agents_md.rs` 的 `brand_home`（顺带修复空 `MIRRICODE_HOME` 不过滤的漂移）、`mirri-os/src/search.rs` 的 `cached_rg_path`、`mirri-embedded/.../env_handlers.rs` 的 `env_os_home`。`dirs` 依赖已从 `mirri-core` 移除（`mirri-os` 内部也已不再使用，见下）。今后任何「跟 OS 走的家目录差异」都改 `mirri-os::home`，不允许再新增 `dirs::home_dir()` 或裸读 `HOME`/`USERPROFILE` 的实现。

## 4. 质量门禁

- 每项处置完成后，受影响 crate 的 `cargo test` + `cargo clippy --all-targets -- -D warnings` 必过。
- 触及 `mirri-core` src 生产代码时，`node rust/scripts/check-zero-io.mjs` 保持 0 命中。
- 用户可感知的变更需 changeset（`gen-changesets` skill），major 需用户确认。
- WO-3 若选 A（改变 Bash 工具在 Windows 的行为），属行为语义变更，**需用户确认 bump 等级**。
