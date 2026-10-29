# Windows Shell 工具语义统一 — 需求与设计（requirements + architecture）

> 建立于 2026-10-26 · 修订 v2（吸收 design review：B1 WSL 劫持、B2 对称性、S1–S6）·
> 对应台账：[windows-support-ROADMAP.md](windows-support-ROADMAP.md) WO-3（本批一并处置 WO-2）
> 本文是需求（WO3-R 系列）与设计决策（ADR）的合并文档：规模有限，拆两文徒增维护成本；如后续膨胀再拆分。
> 现状取证（file:line）见 [windows-support-context.md](windows-support-context.md) §2.3 与本文 §1 的抽象引用。

## 1. 背景与问题

产品在 Windows 上向模型提供的 shell 执行能力存在三方不一致：系统提示词与用户文档声称 Bash 工具走 Git Bash（Unix 语法），代码实际硬编码 `cmd /C`。后果：模型按提示词写 Unix 语法（`/dev/null`、`$(...)`、单引号），在 cmd 下失败或行为不同；用户看到的报错与文档承诺不符。`MIRRICODE_SHELL_PATH` 的文档语义（覆盖 Git Bash 路径）实际只作用于交互终端池，不经过 Bash 工具——变量语义分裂。

用户裁决（2026-10-26）：**有 Git Bash → Bash 工具走 Git Bash；无 → 提供独立 PowerShell 工具**。Bash 工具在无 Git Bash 时**明确报错，不静默降级**。

## 2. 需求定义（WO3-R 系列）

- **WO3-R1** Windows 上 Bash 工具通过 Git Bash 执行命令，模型可继续按 Unix shell 语法书写。
- **WO3-R2** Bash 工具的 Git Bash 解析链（优先级从高到低）：
  1. `MIRRICODE_SHELL_PATH`（须指向 bash 家族可执行文件，见 ADR-WO3-4）；
  2. 已知安装位置探测（PATH 上各 `git.exe` 推导其安装根下的 `bin\bash.exe`，含 Scoop 等包管理器 shim；`LOCALAPPDATA` 下常见安装前缀）；
  3. PATH 上的 `bash.exe`，**排除 `C:\Windows\System32\bash.exe`（WSL 入口）**（见 ADR-WO3-6）。
- **WO3-R3** Windows 上无法解析 Git Bash 时，Bash 工具返回明确的、带行动指引的错误（安装 Git for Windows / 设置 `MIRRICODE_SHELL_PATH` / 改用 PowerShell 工具），不执行、不降级到其他 shell。
- **WO3-R4** Windows 上注册独立 `PowerShell` 工具，通过 PowerShell 执行命令；无 Git Bash 的环境因此仍有可用的原生 shell 工具。
- **WO3-R5** `PowerShell` 工具优先 `pwsh.exe`（PS 7+），回退 `powershell.exe`（系统内置 5.1）。两者皆无法解析时，`PowerShell` 工具与 Bash 缺失语义对称：**注册照常、执行期报错**（报错指引：安装 PowerShell 7 或检查 PATH），不静默剔除。
- **WO3-R6** 系统提示词在 Windows 上说明两个工具的语法边界（Bash=Unix 语法，PowerShell=PS 语法）；非 Windows 平台不注册、不提及 `PowerShell` 工具。
- **WO3-R7** 用户文档三处（`tools.md`、`env-vars.md`、`getting-started.md`）与实际行为一致；`MIRRICODE_SHELL_PATH` 的文档语义更新为对终端池与 Bash 工具统一生效的「shell 覆盖变量」（须指向 bash 家族才对 Bash 工具生效，指向 pwsh/powershell 则对 PowerShell 工具生效）。
- **WO3-R8** PowerShell 工具的退出码语义对模型诚实：原生命令以 `$LASTEXITCODE` 如实传播；cmdlet 抛出终止性异常（`throw`、`-ErrorAction Stop`）以 PowerShell 自身非零退出码传播；cmdlet 的**非终止性**错误（PS 5.1 默认形态，只写 error stream）以 `exit 1` 传播（见 ADR-WO3-3 包装）。工具描述向模型声明「stderr 是 cmdlet 失败的主要信号」。
- **WO3-R9**（随批，WO-2）PATH 探测函数在 Windows 上同时探测 `name` 与 `name.exe`；rg 缓存路径按平台拼后缀。Grep 能力在「已装产品但 PATH 无 rg」的 Windows 环境下可用。

非目标：Bash 工具跨平台行为变更（Unix 路径不变）；交互终端池 shell 解析；WSL 集成（本轮显式排除而非隐式踩中）；cygwin/msys2 非 Git 发行版支持。

## 3. 设计决策

### ADR-WO3-1：双工具而非单工具动态降级

**决策**：Bash（Windows 分支走 Git Bash，缺失即报错）+ PowerShell（Windows-only 注册）两个独立工具，提示词静态声明各自语义。

**备选**：(a) 单 Bash 工具无 Git Bash 时静默回落 PowerShell——被否：工具名与实际 shell 不符即重蹈 cmd 覆辙，且模型写入的命令语法在降级前后语义不同，失败无法归因。(b) 维持 cmd——被否：cmd 对模型最不友好，微软自身在弃用。(c) 单工具 + 动态提示词——被否：语义保证弱于静态双工具。

**后果**：无 Git Bash 且不装 PowerShell 7 的机器上 Bash 工具不可用（WO3-R3 报错指引）。PS 5.1 系统恒有，故「完全无 shell 可用」不存在。

### ADR-WO3-2：解析逻辑落在 `mirri-os::shell`，成功结果进程内缓存

**决策**：Git Bash 与 PowerShell 的解析链实现为 `mirri-os/src/shell.rs` 的新增纯函数（参数化 env 与探测谓词，模式对齐既有 `resolve_shell`），Bash/PowerShell 工具消费。

**缓存语义**：仅缓存**成功**解析结果；解析失败每条命令重新探测（轻量：PATH 与已知前缀扫描），用户装上 Git Bash 或设置 `MIRRICODE_SHELL_PATH` 后无需重启即生效。`MIRRICODE_SHELL_PATH` 是第一优先级——中途设置它可覆盖已缓存的 PATH 探测结果；实现上缓存键含该变量值，变量变化即视为未命中。

**备选**：逻辑写进 `bash.rs`——被否：`mirri-os` 是执行环境层，且避免第三个散落的 shell 解析实现。

### ADR-WO3-3：PowerShell 工具为独立 builtin，共享执行骨架，退出码走 `$LASTEXITCODE`

**决策**：新 `rust/crates/mirri-core/src/tools/powershell.rs`，工具名 `PowerShell`；`register_builtin_tools` 仅 `cfg!(windows)` 注册。**审批/权限语义与 Bash 工具完全对齐**（同一 `background_gate`、task 行、许可规则，仅工具名不同）——不新增权限分支。

**调用形状**：`<shell> -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command <wrapped>`，其中 `<wrapped>` 为用户命令外包一层退出码与编码包装：

- 退出码：命令包装为 `& { <command> } ; if ($LASTEXITCODE -ne $null) { exit $LASTEXITCODE } elseif (-not $?) { exit 1 } else { exit 0 }`——原生命令经 `$LASTEXITCODE` 如实传播；终止性异常（`throw`、`-ErrorAction Stop`）使 PowerShell 进程自身非零退出、包装语句不执行；cmdlet 非终止性错误（不设 `$LASTEXITCODE`、只写 error stream）经 `elseif (-not $?)` 分支以 1 传播。用户命令自带 `exit N` 时直接以 N 终止进程，包装被跳过，语义恰好正确。
- 编码：进程启动时经 `-Command` 预置 `[Console]::OutputEncoding = [Text.Encoding]::UTF8` 与 `$OutputEncoding = [Text.Encoding]::UTF8`（stdout/stderr 解码与输入编码两侧），再执行用户命令。已知限制：原生命令按本地码页（如 GBK）输出的字节仍可能乱码，属强制 UTF-8 的固有代价，风险表登记。

**执行骨架复用**：从 `bash.rs` 抽取「spawn + 双泵 + 超时/取消 race + task 行/fingerprint」为共用模块，`bash.rs` 与 `powershell.rs` 各自实现工具层（参数 schema、提示词、错误文案）。抽取边界 = `run_shell_command` 及后台启动路径的 spawn 准备段（argv 构造前后分界），不整文件重构；现有 Bash 测试的 Unix 分支不得改动（`cfg(windows)` 分支的既有测试仅存在于 `bash-windows-kill.test.ts` 等 TS 侧，rust 侧无 Windows 行为测试需保持）。后台 task 行文案用工具自身名（"PowerShell failed to execute: ..."）。

**备选**：BashTool 加平台参数复用同一注册名——被否：破坏 WO3-R6 静态语义边界。复制三份执行路径——被否：漂移温床。

### ADR-WO3-4：`MIRRICODE_SHELL_PATH` 语义统一为「shell 覆盖变量」，家族不符或路径不存在均硬报错

**决策**：`MIRRICODE_SHELL_PATH` 对三类消费者统一规则：值须指向**存在的**可执行文件（不存在 → 硬报错「路径不存在」，显式配置的失败必须可见，不穿透到下一优先级），按文件名家族分发——

- 文件名含 `bash`：对终端池、Bash 工具生效。
- 文件名含 `pwsh`/`powershell`：对终端池、PowerShell 工具生效；Bash 工具执行期报错（"MIRRICODE_SHELL_PATH 指向 PowerShell，不是 bash；如需在 Bash 工具用 Unix 语法，请将其指向 bash.exe 或清除该设置"）。
- 其他（既非 bash 也非 PS 家族）：对终端池生效（现状不变）；Bash 工具与 PowerShell 工具执行期均报错并说明该变量未被对应工具采纳的原因。

**备选**（review S1）：非 bash 指向静默忽略 + warn——被否：显式配置被静默不采纳违反信号真实性，用户以为已生效。

### ADR-WO3-5：工具缺失在执行期报错，不在注册期剔除

Bash 与 PowerShell 的缺失语义对称（见 WO3-R3/R5）：模型能看见错误并按指引换工具；注册期剔除会让 permission/catalog 面出现平台分裂。代价：缺 Git Bash 机器上每次 Bash 调用烧一次工具调用，可接受。

**目录面**（工具 catalog）：PowerShell 的 catalog 行无条件存在于静态目录（与 profile 编辑器工具选择器一致），仅注册受 `cfg!(windows)` 限制——非 Windows 上它是目录里一个「不可注册」的行，profile 编辑器按现有「builtin 未注册」路径显示，不新增平台分支。

### ADR-WO3-6：显式排除 WSL 的 `bash.exe`

**决策**：PATH 上的 `bash.exe` 探测排除 WSL 入口，排除判据 = **路径规范化（含大小写不敏感、分隔符归一）后等于 `%SystemRoot%\System32\bash.exe`**（`%SystemRoot%` 实时解析，不写死 `C:\Windows`）。命中排除判据的候选**跳过并继续扫描本优先级剩余 PATH 条目**（Scoop shim 排在 System32 之后时仍可命中真 bash）；链路耗尽仍无 → 按未命中报错。产品仅发布 64 位二进制，无 SysWOW64 重定向问题。探测谓词参数化，非 Windows 主机可测。

**备选**：(a) 允许 WSL bash 作为 Git Bash 替代——被否：Git Bash 与 WSL bash 的路径语义、工具链、性能全不同，提示词无法同时承诺两者。(b) 探测 `bash.exe` 时校验其父目录含 `git.exe`——被否：Scoop 等包管理器布局不满足该假设，误杀真 Git Bash。

## 4. 成功判据

- 单元层（跨平台可测，macOS 开发盒可完成）：解析链优先级/空值/`MIRRICODE_SHELL_PATH` 家族分发/System32 排除全覆盖；PowerShell 包装命令（退出码、编码预置、`-ExecutionPolicy Bypass`）的 argv 形状测试；`which_exists_on_path` 的 `.exe` 探测、rg 缓存后缀回归。
- Windows 真机（有 Git Bash）：Bash 工具执行 `echo hi > /dev/null && echo ok` 返回 ok。
- Windows 真机（无 Git Bash，`MIRRICODE_SHELL_PATH` 指向不存在路径）：Bash 工具因「变量路径不存在」报错（不穿透到下一优先级）；清除变量后返回安装指引错误，不执行 cmd。PowerShell 工具 `Write-Output ok` 返回 ok、`$LASTEXITCODE` 传播、cmdlet 非终止性错误退 1、中文输出（cmdlet 与原生命令各一项）不乱码/乱码影响面被记录。
- Windows 真机（装 WSL 且 System32 有 bash.exe、另有 Git Bash）：解析命中真 Git Bash 而非 System32。
- 非 Windows：BashTool 行为与现状一致（现有测试全绿）；PowerShell 工具不注册。
- 真机判据是**本批完成的前置**：随 Windows CI smoke job（ROADMAP WO-6 选项 A+B）落地验证，未验证前 WO-3 在台账保持「已实施待验证」。

## 5. 风险与缓解

| 风险 | 缓解 |
|---|---|
| PowerShell 5.1 真实行为只能 Windows 验证 | 参数化单测锁调用形状；真机判据绑定 Windows CI 前置（§4） |
| Git Bash 路径转换（`C:\x` ↔ `/c/x`）边界行为 | cwd 传 Windows 原生路径（MSYS 自动转换）；命令体不转换 |
| MSYS 参数改写：`/` 开头的参数被 mangle（如 `cmd /foo` 被 Git Bash 改成 Windows 路径） | 提示词声明 Unix 语法；真机验证项覆盖；已知缓解是 MSYS 相关环境变量（`MSYS_NO_PATHCONV`），如需要由用户在 shell 配置侧设置，不在工具层注入 |
| 双 shell 提示词增加 Windows 提示词长度 | `WINDOWS_NOTES` 改写为两行，长度相当 |
| 现有依赖 cmd 行为的用户脚本 | minor changeset 发布说明；`MIRRICODE_SHELL_PATH` 指向 bash 家族可固定行为 |
| 抽取 bash.rs 执行骨架引入回归 | 抽取边界限定为 spawn 准备段 + 执行 race 核心；Unix 分支现有测试零改动全绿作为门禁；抽取后 `cargo test -p mirri-core` 全量 |
| 原生命令按本地码页（GBK 等）输出被强制按 UTF-8 解码 | 已知限制，登记不做通治；真机判据含原生命令中文输出一项以量化影响面 |

## 6. 发布

minor bump（新工具 = 新能力）；实施随 Windows 域台账推进，落地后 WO-2/WO-3 关闭，ROADMAP 同步更新（真机判据完成前置见 §4）。
