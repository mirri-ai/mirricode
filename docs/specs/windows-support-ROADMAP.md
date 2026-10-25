# Windows 平台支持域 — 开放项与处置（ROADMAP）

> 状态：活文档（随执行推进更新） · 建立于 2026-10-25 ·
> 本文是该域**执行状态与开放项的唯一源**（`docs/specs/AGENTS.md`：设计文档不持有进度，开放项都在这里维护）。
> 现状盘点（代码事实、行号、取证边界）：[windows-support-context.md](windows-support-context.md)。
> 上游背景：产品以桌面端为首发形态，Windows 为主要目标平台之一（`WO` 系列来自 2026-10-25 一次针对 Windows 可用性的代码审查）。

## 0. 结论摘要

Windows 目前只验证「编译得过、打得成包」，没有任何自动化验证覆盖「跑起来对不对」（事实见 [windows-support-context.md](windows-support-context.md) §1）。审查发现 **3 项代码可确认缺陷**、**3 项待实测风险**。

优先级判断：**WO-1 是阻断级**——若确认，Windows 桌面端装完即无法启动引擎；其余为功能降级或多平台潜在故障。

## 1. 开放项台帐

### WO-1 数据目录解析无 `USERPROFILE` 兜底 — 阻断级

| 字段 | 内容 |
|---|---|
| 类型 | 代码可确认缺陷（影响面待实测） |
| 位置 | `rust/crates/mirri-core/src/config/home.rs:22-29`；调用点 `rust/crates/mirri-cli/src/main.rs:154` |
| 现象 | 只认 `MIRRICODE_HOME` / `HOME`。Windows 默认无 `HOME` 时 `mirri_home_dir()?` 使进程直接退出，错误 `HOME is not set` |
| 影响面 | 桌面端取决于实测结果（见 [WO-1-V]）；CLI 从 Git Bash 启动不受影响（Git Bash 导出 `HOME`） |
| 前置动作 | **先跑 [WO-1-V] 验证**，再决定改哪一侧 |
| 处置选项 | **A（推荐）** 给 `mirri_home_dir()` 补 `USERPROFILE` → `dirs::home_dir()` 兜底，与 `agents_md.rs:40`、`search.rs:304` 的既有写法统一；改动集中在一处，CLI 与桌面端同时受益<br>**B** 桌面端 spawn 时显式注入 `MIRRICODE_HOME`；覆盖面小、不修 CLI |
| 备注 | 选 A 时需同步 `config/tests.rs:1556-1598` 补 `USERPROFILE` 用例；注意 `HOME` 优先于 `USERPROFILE` 的顺序不能反转 |

### WO-1-V 验证 Windows 桌面端能否拉起引擎

| 字段 | 内容 |
|---|---|
| 类型 | 待实测 |
| 目的 | 确认「数据目录解析无 `USERPROFILE` 兜底」是否真的落在桌面端路径上（代码事实见 [windows-support-context.md](windows-support-context.md) §2.1） |
| 步骤 | Windows 真机安装桌面端 → 启动 → 读 `%USERPROFILE%\.mirri-code\server\server-desktop.log`<br>或直接跑 `mirri.exe web --no-open --port 58627` 看是否输出 `HOME is not set` |
| 判据 | 出现 `HOME is not set` ⇒ 确认阻断，WO-1 立即转实施；日志正常启动 ⇒ WO-1 降级为「CLI 裸环境可用性」，按 P2 处理 |
| 阻塞 | 无（需要一台 Windows 环境） |

### WO-2 rg 缓存兜底缺 `.exe` 后缀 — 功能降级

| 字段 | 内容 |
|---|---|
| 类型 | 代码可确认缺陷 |
| 位置 | `rust/crates/mirri-os/src/search.rs:305`（`join("bin").join("rg")`）、`:291-297`（`which_exists_on_path`） |
| 现象 | Windows 安装器缓存的是 `bin/rg.exe`，代码拼 `bin/rg`，`is_executable_file` 恒 false，回退逻辑失效 |
| 影响 | `grep-glob-capability` changeset 承诺的「PATH 无 rg 时回退缓存」在 Windows 不成立；用户装过产品但 PATH 无 rg 时 Grep 不可用 |
| 处置 | 缓存路径按平台拼后缀（`rg` / `rg.exe`）；`which_exists_on_path` 在 Windows 上同时探测 `name` 与 `name.exe`。顺带补 Windows 分支的单测——现有测试 `:1320-1326` 只在 Unix 语义下有牙 |
| 规模 | 小（约 20 行 + 测试） |

### WO-3 Bash 工具平台语义与提示词/文档不一致 — 影响 Agent 可用性

| 字段 | 内容 |
|---|---|
| 类型 | 既存缺陷（非本批引入） |
| 位置 | 代码 `rust/crates/mirri-core/src/tools/bash.rs:495-498`、`:745-748`（`cmd /C`）；提示词 `rust/crates/mirri-core/src/scope/system_prompt.rs:57`；文档 `docs/en/reference/tools.md:47`、`docs/en/configuration/env-vars.md:127` |
| 现象 | 三处（提示词 + 两份文档 + `MIRRICODE_SHELL_PATH` 的文档语义）声称 Windows 走 Git Bash，代码实际 `cmd /C` |
| 影响 | 模型按 Git Bash 语法写命令（`/dev/null`、`$(...)`、单引号），实际在 cmd 下执行 → 命令失败或行为不同 |
| 处置 | **需用户裁决方向**：<br>**A** 代码对齐文档：Bash 工具在 Windows 上解析 Git Bash（`MIRRICODE_SHELL_PATH` → PATH 上的 `bash.exe`），找不到时回落到 `cmd /C` 并在工具结果里说明<br>**B** 文档对齐代码：改 `WINDOWS_NOTES` 与两份文档，明确 Windows 用 `cmd`，并给出 cmd 语法指引 |
| 缺口 | 选 A 需处理 Git Bash 路径转 POSIX（`C:\x` → `/c/x`）、`NUL` → `/dev/null` 重写，工作量显著大于 B |
| 建议 | 倾向 A（用户体验与现有文档承诺一致），但需先确认 Git for Windows 是否为硬性前置（`docs/en/guides/getting-started.md:39` 已把它列为首次启动前置） |

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

1. **WO-1-V**（需 Windows 环境，一次验证即可定性）
2. **WO-1 / WO-2**（若验证确认 WO-1 为阻断；两者都是小改动，可同一批交付）
3. **WO-3**（需用户裁决 A/B 后再动）
4. **WO-4、WO-5**（随 Windows 环境可用时验证）
5. **WO-6**（结构性投入，建议在 1-4 收敛后立项）

## 3. 不在本域范围

- 通用 `HOME` 语义本身（POSIX 约定）——仅登记「缺 `USERPROFILE` 兜底」这一实现缺口。
- 平台无关功能的逐项复核——已确认无风险的部分见 [windows-support-context.md](windows-support-context.md) §4。
- macOS / Linux 平台差异（含 FSEvents vs inotify 的事件语义）——已由 `debugging-file-watchers` skill 沉淀为通用排查方法，不在本台账重复。

## 4. 质量门禁

- 每项处置完成后，受影响 crate 的 `cargo test` + `cargo clippy --all-targets -- -D warnings` 必过。
- 触及 `mirri-core` src 生产代码时，`node rust/scripts/check-zero-io.mjs` 保持 0 命中。
- 用户可感知的变更需 changeset（`gen-changesets` skill），major 需用户确认。
- WO-3 若选 A（改变 Bash 工具在 Windows 的行为），属行为语义变更，**需用户确认 bump 等级**。
