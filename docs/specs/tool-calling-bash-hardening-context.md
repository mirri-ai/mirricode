# 工具调用 · Bash 工具硬化调研（Context）

> 快照日期：2026-11-03 · 类型：context（现状盘点 + 外部机制对照 + 借鉴裁决对照）
> 本文只盘点**当前状态**，不含已裁决决策（设计裁决见 [tools/bash-design.md](tools/bash-design.md)；Bash 既有契约见 [tool-calling-builtin-tools.md](tool-calling-builtin-tools.md) §2.1）。
> 代码事实以 `rust/crates/` 当前工作区为准，全部 file:line 当场核实。

## 1. 现状盘点（mirricode rust core）

### 1.1 参数与描述

| 参数 | 现状 | 位置 |
|---|---|---|
| `command` | 必填 string | `rust/crates/mirri-core/src/tools/bash.rs:189` |
| `timeout` | 前台默认 60s / 钳制上限 300s；后台默认 600s / 超过 86400s 拒绝 | `bash.rs:62-74`、`bash.rs:355-371` |
| `cwd` | 缺省用会话工作目录 | `bash.rs:245` |
| `run_in_background` | 后台任务立即返回 task_id | `bash.rs:276-306` |
| `disable_timeout` | **前台后台都生效**（`EffectiveTimeout::Disabled` 优先分支） | `bash.rs:355-357` |
| `exempt_lifetime` | 豁免 4h 生命周期天花板；前台注册时即 stamp | `bash.rs:808-810`、`bash.rs:269-272` |
| `description` | 后台任务标签，缺省用命令前 60 字符预览 | `bash.rs:381-388` |

### 1.2 执行机制

- Unix `sh -c`（`bash.rs:818`）；Windows Git Bash `-c`（`bash.rs:821-832`）。工具名是 `Bash`，但 Unix 实际执行 POSIX sh（Linux 上通常是 dash）。
- 环境变量：父进程环境**全量继承**（`rust/crates/mirri-os/src/process.rs:283-286` 无 `env_clear`），叠加 `noninteractive_env()` 四项覆盖（`NO_COLOR=1`、`TERM=dumb`、`GIT_TERMINAL_PROMPT=0`、`SHELL` 透传，`bash.rs:395-408`）。无敏感变量剔除。
- stdin 创建后立即关闭（EOF），交互命令不挂死（`bash.rs:840`）。
- 进程组隔离（Unix `process_group(0)`）+ 两阶段 kill（SIGTERM → 5s → SIGKILL，`mirri-os/src/process.rs:100-116`）。

### 1.3 超时与后台

- 前台超时**不杀进程**：`AtomicBool` 首赢守卫 → `hand_over_to_background` 转后台并重布防 600s（`bash.rs:871-1004`、`bash.rs:232-242`）——已由 tool-calling ADR-4 裁决，规格一致。
- 生命周期天花板：默认 4h（`config/mod.rs:413`），`exempt_lifetime` 豁免；sweeper 在 `rust/crates/mirri-embedded/src/embedded_server/orphan_governance.rs:160-209`。

### 1.4 输出处理

- 前台：两条流分别泵入独立 `String` 累积（`bash.rs:859-860`、`bash.rs:990-993`）——**过程无界**；结束后 `combine_captured_output` 把 stdout 全量在前、stderr 全量在后拼接（`bash.rs:640-648`）。超过 50k 字符由 `ToolResultTruncator` **事后**截断落盘（`rust/crates/mirri-core/src/tool_result_truncation.rs:24`、调用点 `turn_flow/hooks.rs:2041-2047`）。
- 流式 progress 逐块发送且区分 Stdout/Stderr（`bash.rs:995-1001`），但最终 ToolResult 丢失到达顺序。
- 后台：内存缓冲上限 1 MiB 丢头保尾（`task.rs:51`、`task.rs:1289-1333`）+ 全量 `output.log` 落盘；但 `run_bash_background` 的 progress 为 `None`——后台任务无实时增量输出（`bash.rs:1321-1349`）。

### 1.5 任务查询与日志可观察性

- TaskOutput 非阻塞（`block` 默认 false，`rust/crates/mirri-core/src/tools/task_output.rs:63-66`）且全模式免审批（`DEFAULT_APPROVE_TOOLS` 含之，`policies.rs:67-88`）。
- 断点：转后台返回文案与完成通知明示 "do NOT wait, poll, or call TaskOutput"（`bash.rs:1373-1401` 一带）——模型被文案禁止查询，纯等重布防 600s。
- 断点：快照只给尾部 2000 字符预览（`TASK_OUTPUT_PREVIEW_CHARS`，`task.rs:54`）；全量靠 `output.log`（`{session_dir}/tasks/{id}/output.log`）+ Read 分页，该路径在沙箱外，manual 模式被 `WorkspaceBoundaryPolicy`（链位 18）拦。持久层窗口读取 `read_output(task_id, offset, max_bytes)` 已支持任意偏移（`task_persist.rs:149-165`）。
- 断点：快照无运行时长/静默时长信号；`append_output` 不记录输出时间戳。
- 断点：前台取消时已累积输出直接丢弃，不落任务行（`bash.rs:958-973`）。
- 心跳与静默正交：心跳是进程存活信号，长静默命令必须照常跳心跳（`bash.rs:1048-1062` 设计注释明示）——静默检测不可复用心跳/stale。



- 规则 DSL 支持参数化模式 `Bash(<arg pattern>)`（`rust/crates/mirri-core/src/permission/rule_match.rs:103-141`，`tool_matches_rule` 闭包形参已预留），但**引擎内所有调用点都传 `None`**（`policies.rs:296/556/620/648/682`）且无任何内建工具实现 `matches_rule`——参数化规则全链路静默不匹配。
- 会话级批准记忆存的是刻面串，Bash 刻面现为裸 `"Bash"`（`bash.rs:258`），批准后 `SessionApprovalHistoryPolicy` 按工具名匹配可生效（`turn_flow/hooks.rs:1917`、`permission/mod.rs:159-184`）——即"本会话均批准"对 Bash 等于**放行任意命令**，粒度只有整工具。
- 用户级 allow/ask 规则（`UserConfiguredAllowPolicy`/`UserConfiguredAskPolicy`，`policies.rs:634-690`）同样只按工具名匹配；写 `Bash(git status)` 形态不生效。

### 1.6 规格与实现的偏差（builtin-tools §2.1 为准）

| # | 偏差 | 规格说法 | 实现行为 |
|---|---|---|---|
| D-1 | 两流合并 | 「按到达顺序交错合并（同一缓冲流式追加）」 | 分开累积、stdout 全前 stderr 全后拼接 |
| D-2 | `disable_timeout` 范围 | 「Only applies when run_in_background is true」 | 前台也生效（`Disabled` 分支优先） |
| D-3 | `exempt_lifetime` | 参数表无此参数 | 已实现且有需求裁决（US14.4），规格未登记 |
| D-4 | 后台 `description` | 「可选（后台必填，运行时校验）」+ 缺失报错文案 | 可选、缺省取命令前 60 字符预览（`bash.rs:287-289`、`bash.rs:381-388`），无校验报错 |

### 1.7 执行目录与受保护目录现状

- `cwd` 参数无任何校验：`resolve_execution` 直接 `input.cwd.or(self.default_cwd)`（`bash.rs:245`），任意绝对路径放行；workspace 边界策略不覆盖 Bash 的 cwd（BOUNDARY_TOOLS 只查 `args.path`，`policies.rs:890`）。
- 命令内 `cd` 完全绕开审查：无扫描、无提取、无拦截；模型频繁 `cd <绝对路径> &&` 前缀（用户观察），rust 实现的工具描述**没有**反 cd 引导（`bash.rs:167-181`；那句引导只存在于 builtin-tools 规格文本，模型看不到）。
- 桌面宿主未启用 App Sandbox（`apps/mirri-desktop/build/entitlements.mac.plist` 仅 hardened runtime 四项），Bash 子进程继承宿主身份（`process.rs:284-286` 无 env_clear）——触碰 macOS TCC 受保护目录（桌面/文稿/下载等）时引擎静默放行、OS 层弹系统授权。

## 2. 外部机制对照（opencode / deepseek-harness）

> 调研仓库：`../opencode`（HEAD `67139d6827`）、`../deepseek-harness`。完整三方对比报告（工作底稿）在 `plan/2026-11-03-bash-tool-design-comparison.md`（gitignored，不入库）。本表只保留与设计裁决相关的机制事实。

| 机制 | opencode | deepseek-harness |
|---|---|---|
| 命令级权限 | 自研 bash/zsh/posix 扫描器（2565 行 `packages/core/src/shell/scan.ts`）拆解复合命令，"always allow" 存 ARITY 前缀通配（`git *`、`npm run *`）；无法安全分析的语法（命令替换、heredoc 等）整体拒绝执行 | 无命令黑名单；沙箱（bwrap/Landlock/Seatbelt）+ `sandbox_permissions`/`justification` 升级审批 |
| 前台输出内存 | 边执行边写文件 + 实时字节数，模型只见尾部（`packages/core/src/shell.ts:333-345`） | 内存尾部保留 64KB/流 + 超出写 spill 文件（`packages/subprocess/subprocess-local/src/output.ts:160-183`） |
| 凭证卫生 | workspace 场景不继承宿主 env（`shell-retention.test.ts:104-145`） | `SENSITIVE_ENV_PATTERN = /KEY\|PASSWORD\|SECRET\|TOKEN/i` 剔除敏感名 + 受管变量最后合并（`packages/subprocess/subprocess/src/index.ts:47`） |
| 两流处理 | merge 单流保序 | 分开捕获，渲染时 `[stderr]` 分区标注 |
| 后台进度 | 后台输出流式写文件，HTTP 游标分页可读 | job 环增量读取 |
| 前台超时 | 超时即杀 | 默认转后台 job（promoteOnTimeout） |
| 生命周期 | 无（后台 job SQLite 持久化可跨重启恢复） | 每 owner 10 并发上限 |
| 持久 shell | 无（shell 工具一次性；PTY 面向用户终端） | 可选 `tool-bash-persistent`（PTY 会话跨调用保持 cwd/env） |

### 2.1 「agent 自助取日志」专题对照（2026-11-03 二轮调研）

| 维度 | opencode | deepseek-harness | mirricode（现状） |
|---|---|---|---|
| 日志落点 | `<data>/shell/*.out`（沙箱外） | 宿主 tmpdir spill 文件（沙箱外） | `{session_dir}/tasks/{id}/output.log`（沙箱外） |
| agent 读得到吗 | **被挡**——`external_directory` 默认 ask | **读得到**——权限哲学「只围写不围读」（`fs-sandbox` 只 override 五个写方法，读零围栏，`packages/fs/fs-sandbox/src/index.ts:65-136`） | **manual 模式被挡**（WorkspaceBoundaryPolicy 对 Read 的 outside 路径 ask） |
| 引擎目录免审批史 | 曾注入 `shell/*/*`、`tool-output/*` allow（2026-08），**2026-10-06 主动移除**（commit `36a38058fb`） | 无白名单；可读靠「读不围栏」的结构性事实 | 无 |
| 模型侧分页读取 | 无（服务端 REST 有 shellID 字节游标分页，只给 TUI/App 用） | `job_output` 无 offset（消费型游标，读过不再给；缺口靠模型自己 read spill 文件，`render.ts` 带 `[full output: <path>]`） | TaskOutput 快照尾部 2000 字符；持久层窗口读取已有但未暴露为参数 |
| 取消后输出 | **中断即删文件**（`shell.ts:167-180`，同病且更糟） | **保留**（kill 不动环、spill 不删） | **丢弃**（`bash.rs:958-973`） |
| 运行中静默预警 | 无 | 无 | 无 |

### 2.2 「cd/cwd 执行目录」专题对照（2026-11-03 三轮调研）

| 维度 | opencode | deepseek-harness | mirricode（现状） |
|---|---|---|---|
| workdir 参数审查 | 受 external_directory 审批，与 cd 目标平权（`plugin/shell.ts:114-142`：`authorizeExternal([target, ...directories])`） | **不审查**（`workdir: '/'` 可跑，`tools.spec.ts:1058` 有测试佐证）——作者立场：cwd 在不在 workspace 内不是有意义攻击面 | 无任何校验 |
| 命令内 cd 处理 | 扫描器提取目标（CWD 集合：cd/chdir/popd/pushd/push-location/set-location，穿透 `builtin cd`/`command cd`，`parse.ts:12,249-267`）；静态可解的 cd 段从命令权限资源中剔除（`parse.ts:205-210`）；含变量目标退回为命令资源；无法静态分析的命令整体拒绝 | **零处理**——沙箱按「写权限根」做边界，`cd /x && write` 与 `write /x` 判据相同，cd 天然不构成逃逸 | 零处理 |
| 硬性禁 cd | 无 | 无 | — |
| 反 cd 引导 | 仅 workdir 参数描述一句（"avoid changing directories in the command"） | 仅工具描述一句 "pass `workdir` instead of using `cd`"（正式下发文案，snapshot 可佐证） | 规格有、rust 实现的描述没有 |
| 边界哲学 | 引导 + 审批覆盖（但默认 agent 是 catch-all allow，机制「可选启用」） | 边界下沉到写权限沙箱（bwrap/Landlock/Seatbelt 只读全盘 + 白名单写根） | 无边界 |
| TCC 类系统弹窗问题 | 未见专门设计 | 未见专门设计（沙箱模式下只读全盘天然规避写入型 TCC，读取型 TCC 类别不在其词表） | 无处理（桌面宿主未开 App Sandbox） |

## 3. 借鉴 / 不借鉴裁决对照

| 外部机制 | 裁决 | 理由 |
|---|---|---|
| 尾部保留 + 超限落盘 | **借鉴** | → B-ADR-2 |
| 命令前缀通配授权 | **简化借鉴** | → B-ADR-1 |
| 敏感环境名剔除 | **借鉴但收紧范围** | → B-ADR-3 |
| stderr 分区标注 | **不借鉴** | → B-ADR-4（采纳规格 D-1 交错） |
| OS 级沙箱 | **不借鉴（本域不做）** | 量级独立（三平台后端 + fail-closed + 升级审批），应独立立项；Bash 域先修权限粒度 |
| 持久 shell（PTY） | **不借鉴** | 现产品无交互式调试场景诉求；跨调用状态保持与现有「fresh shell」描述契约（builtin-tools §2.1 安全段）冲突 |
| 后台 job 跨重启恢复 | **不借鉴（本域不做）** | 与生命周期天花板（4h + sweeper）语义冲突，需先裁决「任务持久化」方向 |
| 「只围写不围读」权限哲学（deepseek） | **不借鉴（另立项）** | → B-ADR-6：影响全部边界工具，属 approval 域层级的裁决；本设计以「任务 id 分页内置」绕开该依赖 |
| 引擎目录免审批白名单（opencode 的老路线） | **不借鉴** | → B-ADR-6：opencode 实践后主动撤销的路线（`36a38058fb`），且与「拒绝后不得绕路」合同打架 |
| 取消后保留输出（deepseek） | **借鉴** | → US-BH9：取消保全对齐保留语义 |
| 分页读取暴露给模型（opencode REST 的既有形态） | **借鉴** | → B-ADR-6：机制成熟度已被 opencode 服务端验证，我们接到模型可见工具上 |
| cd 目标提取 + workdir 平权审批（opencode） | **借鉴** | → B-ADR-8：分段器扩展提取 cd 段目标目录，与 cwd 参数合并送审；静态可解 cd 段从匹配串剔除 |
| 硬禁 cd | **不借鉴（主动否决）** | → B-ADR-8：三家无一先例；合法用法误伤面大 |
| 写权限根沙箱（deepseek 的结构性答案） | **不借鉴（另立项）** | → B-ADR-8：需 OS 级沙箱，本设计非目标；其「cwd 不是攻击面」立场不适用于我们（无沙箱时 cwd 是唯一可审的结构化入口） |
| 受保护目录字面模式检测 + 事前 Ask | **自研（无业界先例可抄）** | → B-ADR-9：定位是事前知情与体验一致性，不是强制边界；诚实边界写入描述 |
