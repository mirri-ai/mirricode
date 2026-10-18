# 执行环境域 — 现状盘点（context）

> 快照日期：2026-09-12（`date` 实测）。本文为执行环境域的现状清单：代码事实（file:line）、覆盖差异矩阵（TS 实际支持 vs 需求文档已收录/未收录）、v1-source 能力标记。不含目标、不含裁决（见 requirements / architecture）。
> 行号以快照日代码为准，随代码演进漂移；差异矩阵是**文档 vs TS 现状**的对照，不是 TS vs Rust 的对照。

## 1. 代码地图（TS 侧双栈）

### 1.1 库栈（kaos：统一执行环境抽象）

| 能力 | 代码事实 |
|---|---|
| 统一接口 | `packages/kaos/src/kaos.ts:12-97`（路径面/目录面/文件面/进程面四段） |
| 进程句柄契约 | `packages/kaos/src/process.ts:10-27`（stdin/stdout/stderr/pid/exitCode/wait/kill/dispose） |
| 本地实现 | `packages/kaos/src/local.ts:188-770`；`create()` 并发探测主机+登录壳 PATH `:215-222`；实例级 cwd `:194-206`；`withCwd/withEnv` 不可变派生 `:224-230` |
| spawn 选项 | `packages/kaos/src/local.ts:57-69`（三管道、`detached:!isWindows`、`windowsHide`） |
| kill 语义 | `packages/kaos/src/local.ts:112-169`（POSIX `process.kill(-pid)`；ESRCH 幂等；EPERM 回退直杀；Windows `taskkill /T /F` `:128-141`；pid≤0 直接成功 `:118-120`） |
| spawn 就绪门 | `packages/kaos/src/local.ts:888-900`（waitForSpawn） |
| env 合并 | `packages/kaos/src/local.ts:759-769`（宿主 env → 调用覆盖 → withEnv 层） |
| glob 遍历 | `packages/kaos/src/local.ts:299-450`；循环检测 key `:52-55`（inode=0 跳过） |
| 行读/尾部读/扫描 | `packages/kaos/src/local.ts:478-648`（64KB 分块、BOM 偏移规则）；scanTextFile `:495-531`、readLineRange `:533-543`、readTailLines `:545-591` |
| mkdir 语义 | `packages/kaos/src/local.ts:672-725`（existOk 仅对目录生效；非目录冲突抛 KaosFileExistsError） |
| 错误类 | `packages/kaos/src/errors.ts:4-39`（KaosError/ValueError/FileExistsError/ShellNotFoundError） |
| AsyncLocalStorage 上下文 | `packages/kaos/src/current.ts:8-44`（runWithKaos/setCurrentKaos/getCurrentKaos 带修复指引错误） |
| **远程 SSH 实现** | `packages/kaos/src/ssh.ts:435-944`：认证队列 `:186-211`；host key 无条件放行 `:519`；`osEnv` 未接线（抛错）`:446-450`；exec 命令构造（`cd` 前缀 + env 内联注入 + POSIX 变量名校验）`:894-923`；信号剥 `SIG` 前缀 `:255-264`；SFTP 错误映射 `:169-184`；mkdir 递归逐级校验 `:795-848`；close `:936-944`。**无任何生产消费方**（仅测试 `packages/kaos/test/ssh*.test.ts`、`test/e2e/ssh-*.test.ts`） |
| 共享原语 | `packages/kaos/src/internal.ts:135-179`（decodeTextWithErrors 三态）；`:186-238`（globPatternToRegex）；`:245-296`（BufferedReadable，128KB） |

kaos 消费方：v1 引擎工具层（`packages/agent-core/src/tools/builtin/{shell/bash.ts:217-297,file/*}` 注入 Kaos 实例）、node-sdk 恢复路径（`packages/node-sdk/src/v2/resume-replay.ts:73` `LocalKaos.create()`）、ACP 适配器（`packages/acp-adapter/src/kaos-acp.ts:25-...` AcpKaos 装饰器，读/写反向 RPC 到宿主、其余委派 inner；`server.ts:553` 懒创建 LocalKaos）。

### 1.2 引擎栈（v2 os 域 + workspace 层）

| 能力 | 代码事实 |
|---|---|
| 文件系统契约 | `packages/agent-core-v2/src/os/interface/hostFileSystem.ts:30-52`；stat/entry 模型 `:14-28` |
| 文件系统错误域 | `packages/agent-core-v2/src/os/interface/hostFsErrors.ts:19-74`（8 码，unavailable/unknown 可重试）；errno 映射 `:110-129`；边界翻译器幂等 `:131-138` |
| 进程契约+错误 | `packages/agent-core-v2/src/os/interface/hostProcess.ts:17-48, 53-78`（spawn_failed/kill_failed） |
| 终端契约 | `packages/agent-core-v2/src/os/interface/terminal.ts`（wire zod `:28-48`；相对 cwd 强制 `:20-23, 69-75`；帧类型 `:50-67`；TerminalProcess `:93-99`） |
| 主机环境契约 | `packages/agent-core-v2/src/os/interface/hostEnvironment.ts:32-43`（含 `ready`） |
| 监视契约 | `packages/agent-core-v2/src/os/interface/hostFsWatch.ts:14-36` |
| node-local 后端（fs） | `packages/agent-core-v2/src/os/backends/node-local/hostFsService.ts`：createExclusive（wx+fsync，EEXIST→false）`:174-188`；readLines 分块 `:117-167`；remove=rm 递归强删 `:244-250`；App scope `:261-267` |
| node-local 后端（进程） | `packages/agent-core-v2/src/os/backends/node-local/hostProcessService.ts`：spawnOptions `:26-41`；waitForSpawn `:50-63`；HostProcess kill（POSIX 组杀/EPERM 回退/Windows taskkill）`:119-160`；mergeStderr `:95-97`；spawn 失败域名化 `:184-195` |
| node-local 后端（终端） | `packages/agent-core-v2/src/os/backends/node-local/hostTerminalService.ts:25-43`（node-pty 懒加载、xterm-256color、env=process.env）；dispose 全杀 `:45-54` |
| node-local 后端（主机环境） | `packages/agent-core-v2/src/os/backends/node-local/hostEnvironmentService.ts:31-36`（ready=探测+登录 PATH 并发）；未就绪访问抛 BugIndicatingError `:39-46` |
| node-local 后端（监视） | `packages/agent-core-v2/src/os/backends/node-local/hostFsWatchService.ts`：chokidar `:9`；单次 watch 独立 watcher `:73-75`；非递归 depth 0 `:42-51`；默认忽略 .git/*.sock `:24-25`；内部 polling 开关（MIRRICODE_INTERNAL_FS_WATCH_POLLING）`:37-50`；事件映射 `:84-101` |
| 进程运行器分层 | 契约 `packages/agent-core-v2/src/session/process/processRunner.ts:17-38`；会话默认实现 `session/process/processRunnerService.ts:24-58`；工作区实现（默认 cwd=handler 根，env=undefined 原样继承）`workspace/workspaceProcess/workspaceProcessRunnerService.ts:21-55`；会话创建时遮蔽注入 `workspace/sessionLifecycle/sessionLifecycleService.ts:276` |
| Bash 工具消费链 | `packages/agent-core-v2/src/agent/tools/os/bash/bashTool.ts`：shell 组装（`shellPath -c 'cd <cwd> && cmd'`、Windows 路径转 POSIX `:192-208, 487-500`；NUL→/dev/null 重写 `:502-506`）；非交互 env（NO_COLOR/TERM=dumb/GIT_TERMINAL_PROMPT/SHELL）`:200-206`；stdin 立即关闭 `:467-471`；空命令/中止校验 `:318-339` |
| 工作区文件服务 | `packages/agent-core-v2/src/workspace/workspaceFs/fsService.ts`：读 10MB 上限 `:96`；隐藏过滤+macOS 噪声 `:98-99, 920-922`；grep 30s 超时 `:93`；搜索硬上限 500 `:92`；walk 深度 64、跳 .git `:94, 672-708`；**路径约束两段式** resolveWithin `:772-805`（空/绝对/`..` 段拒绝 → 词法界内 → realpath 最长存在前缀 `:754-770` → 与 realpath 根集比较 `:738-752`）；gitignore 匹配器缓存 `:710-723`；rg 解析缓存 `:725-736`；rg JSON 累积器（限额即杀）`:524-598, 816-917` |
| wire 契约与默认值 | `packages/agent-core-v2/src/workspace/workspaceFs/fs.ts`：list（depth 1-10/limit 1-1000 默认 200/5 种排序）`:85-94`；read（offset/length≤10MB 默认 1MB/encoding auto）`:108-128`；grep（max_files 200、per_file 50、total 5000、context 2）`:197-217` |
| 会话终端服务 | `packages/agent-core-v2/src/session/terminal/terminalService.ts`：默认壳 `$SHELL || '/bin/sh'` `:251-253`；默认 80×24 `:30-31`；cwd 经工作区断言 `:78-83`；环形帧缓冲 2000 `:32, 230-234`；attach sinceSeq 回放 `:124-137`；exit 帧 seq=MAX_SAFE_INTEGER `:247-249`；close→exit_code null `:160-168`；自然退出广播 `:205-228`；not found `:182-191` |
| 文件监视（工作区层） | `packages/agent-core-v2/src/workspace/workspaceFs/fsWatchService.ts`：单 watcher 递归+启停 `:85-113`；防抖默认 200ms/窗口 500（**环境变量 MIRRICODE_FS_WATCH_DEBOUNCE_MS / MIRRICODE_FS_WATCH_MAX_CHANGES_PER_WINDOW 可调**）`:40-48, 204-263`；订阅路径词法约束 `:145-182` |
| 附加目录集 | `packages/agent-core-v2/src/workspace/workspaceDirs/workspaceDirsService.ts`：fileDirs(local.toml) ∪ ephemeral `:93-95`；addDir persist 默认 true `:124`；变更尾队列串行 `:205-212`；local.toml 监视 200ms 防抖重载 `:184-203`；播种会话 `sessionLifecycleService.ts:274`；消费方=path-access 与 fsWatch 的工作区判定 |
| rg 供给（工具层，带下载） | `packages/agent-core-v2/src/os/backends/node-local/tools/rgLocator.ts`：PATH→vendor→缓存→下载 `:83-159`；6 平台固定 SHA-256 `:24-40`；原子安装 `:181-255`；校验失败文案 `:257-271`；runRg 20s/5s 两段杀/10MB 截断 `os/backends/node-local/tools/runRg.ts:14-16` |
| rg 供给（搜索层，无下载） | `packages/agent-core-v2/src/workspace/workspaceFs/internal/rgLocator.ts:58-97`（仅 PATH 探测 + 缓存二进制）；runRg 同构 `internal/runRg.ts:14-17` |
| Glob/Grep 工具消费 | `packages/agent-core-v2/src/agent/tools/os/glob/globTool.ts`（rg --files --hidden --sortr=modified、100 匹配上限、20s 超时、敏感后置过滤）；`grep/grepTool.ts`（默认 files_with_matches、head_limit 250、mtime 并发 stat 32、20s 超时）——逐工具语义归工具调用域 |

### 1.3 v2 引擎栈的 _base/execEnv 分叉（与 kaos 同源）

`packages/agent-core-v2/src/_base/execEnv/`：decodeText.ts / bufferedReadable.ts / globPattern.ts 与 kaos/internal.ts 逐行同源；**environmentProbe.ts 与 kaos/src/environment.ts 存在行为差异**（见 §3 差异矩阵）；loginShellPath.ts 行为一致（kaos 版注释更详尽）。

## 2. v1-source 能力标记（含存活状态）

| 能力 | 代码事实 | 存活状态 |
|---|---|---|
| v1 Bash 工具走 kaos（env 合并含 process.env 展开） | `packages/agent-core/src/tools/builtin/shell/bash.ts:274-297` | `[v1][node-sdk]` SDK v1 路径存活；apps 已不消费 |
| Windows 无 Git Bash 的公开错误码（安装指引文案） | `packages/agent-core/src/errors/codes.ts:434-439`（`shell.git_bash_not_found`） | `[v1][node-sdk]` |
| ACP 桥接执行环境（编辑器未保存缓冲读写） | `packages/acp-adapter/src/kaos-acp.ts:1-18`（设计）、全文件；`server.ts:553` | `[v1][acp-adapter]` 存活 |
| SSH 远程执行环境 | §1.1 | `[v1]` 库层存在，**无产品入口、无宿主消费** |
| v1 会话文件服务（fsService/fsPathSafety/fsSearch/fsGit/fsWatcher） | `packages/agent-core/src/services/fs/*`（pathSafety 五类拒绝 `fsPathSafety.ts:43-68`；读 10MB/二进制 4096B/30% `fsService.ts:37-41, 163-226`；rg 探测+降级 `fsSearchService.ts:127-151`；chokidar+引用计数+100 路径/200ms/500 截断 `fsWatcherService.ts`） | `[v1]` **无宿主**：v1 HTTP 服务器（packages/server）已于 commit `1aa104b9`（2026-08-19，迁移 v2）删除；公开导出面仍在（`agent-core/src/index.ts:180`） |
| v1 终端服务（会话级） | `packages/agent-core/src/services/terminal/terminalService.ts`：默认壳 `$SHELL ?? (win32→powershell.exe, 否则 /bin/sh)` `:270`（**注释称须用 `\|\|` 但代码是 `??`——空 $SHELL 不会触发回退，注释与实现不一致**）；node-pty `:237-256`；2000 帧缓冲 `:31, 226-234` | `[v1]` 无宿主（同上）；测试仍在（`packages/agent-core/test/services/terminal-service.test.ts`） |
| v1 工作区目录浏览器（绝对路径、只列目录、git 检测） | `packages/agent-core/src/services/workspace/workspaceFsService.ts:31-105` | `[v1]` 无宿主；**注意与 v2 的 workspaceFsService（路径约束文件服务）同名不同物** |
| v1 coreProcess（进程内 RPC 适配器）| `packages/agent-core/src/services/coreProcess/coreProcessService.ts:79-141`（**不是子进程管理器**——in-process RPC 桥） | `[v1]` 无宿主（实现方 packages/server 已删）；仅 v1 services 图内部注入 |
| v1 environment 服务 | `packages/agent-core/src/services/environment/environment.ts:13-19`（仅 homeDir/configPath 契约） | `[v1]` 无生产实现（原实现在已删除的 packages/server） |

> v1 无宿主服务的处置是开放项（见 ROADMAP）。它们的用户可见契约在 requirements 中的归属：fs 原语/监视/终端行为已并入对应 Epic 的基线条目（US3.x、US5.x）；v1 工作区目录浏览器（绝对路径形态）的用户可见承接在工作区管理域的"添加工作区对话框"条目（该域 US1.1 目录树浏览），本域不重复记录；v1 coreProcess 与 environment 服务为引擎内部 RPC/配置骨架，无用户可见行为，仅在此留档。

## 3. 覆盖差异矩阵（文档 vs TS 现状）

### 3.1 已文档化（requirements 覆盖 ↔ 代码支撑）

| 需求条目 | TS 支撑 | 备注 |
|---|---|---|
| US1.1 本地环境/并发 cwd 隔离 | kaos LocalKaos 实例级 cwd；v2 工作区 runner 遮蔽注入 | 全覆盖 |
| US1.2 远程执行环境 | kaos SSHKaos | 全覆盖（osEnv 探测未接线，文档以〔设计增强〕要求补齐） |
| US1.3 宿主桥接 | AcpKaos | 全覆盖 |
| US1.4 上下文绑定 | kaos current.ts | 全覆盖 |
| US2.1-2.4 探测 | v2 environmentProbe + hostEnvironmentService；kaos environment.ts | 全覆盖 |
| US3.1-3.3, 3.6 文件原语 | hostFsService + kaos local.ts | 全覆盖 |
| US3.4 监视 | hostFsWatchService + workspaceFs/fsWatchService | 全覆盖 |
| US3.5 远程文件语义 | SSHKaos SFTP | 全覆盖 |
| US4.1-4.3 进程 | hostProcessService + kaos local | 全覆盖 |
| US4.4 远程进程 | SSHKaos exec/kill | 全覆盖 |
| US5.1-5.3 终端 | v2 session/terminal + hostTerminalService | 见 §3.3 缺口（输入/resize 通道） |
| US6.1-6.3 沙箱面 | fsService.resolveWithin + workspaceDirs + path-access | 全覆盖 |
| US7.1-7.2 rg | 两套 rgLocator | 见 §3.3（两套不一致） |
| US8 非功能 | 各超时常量 | 全覆盖 |

### 3.2 需求文档已声明划归相邻域的 TS 细节（非遗漏，requirements 边界段有对应声明）

- Bash 工具的非交互 env 注入（NO_COLOR/TERM=dumb 等）、Windows 路径→POSIX 转换、NUL 重定向重写——归工具调用域内建工具规格（`docs/specs/tool-calling-builtin-tools.md` §2.1）。
- Read/Write/Glob/Grep 工具的逐工具限制（行数/字节数/匹配数/敏感文件前置守卫）——归工具调用域（`tool-calling-builtin-tools.md` §0.5、§1.x）。本域只收录共享机制（路径约束、rg 供给）。
- 终端创建 REST 路由的 URL 形状与错误码映射——归对话渲染/kap-server 协议面。
- 文件树/下载的界面级行为（etag/mime/language_id 投影）——归 web 界面域；本域仅收录底层原语。

### 3.3 回归 / 缺口（TS 支持与文档预期不符或自身不一致）

1. **壳覆盖变量名分叉**：库栈 `MIRRICODE_SHELL_PATH`（`packages/kaos/src/environment.ts:94`）vs 引擎栈 `MIRRI_SHELL_PATH`（`packages/agent-core-v2/src/_base/execEnv/environmentProbe.ts:128`）；v1 错误码文案引用前者（`agent-core/src/errors/codes.ts:436`）。同一产品两个变量名，用户设置一个对另一条路径无效。requirements US2.2 已要求统一（设计增强）。
2. **探测集合分叉**：引擎栈 mingw 前缀含 ucrt64/clang64/clangarm64（`environmentProbe.ts:53-59`），库栈仅 mingw32/mingw64（`kaos/src/environment.ts:192`）——同一台 MSYS2 UCRT 机器，两条路径探测结果可能不同。
3. **未找到壳的错误类型分叉**：库栈专用 KaosShellNotFoundError（携带已检查路径），引擎栈普通 Error（`environmentProbe.ts:184-186`），引擎栈上层无法程序化区分。
4. **v2 终端输入/调整尺寸通道未接线**：服务层与 PTY 层完备，WS 控制帧 schema 已定义（`packages/protocol/src/ws-control.ts:318-426`），但连接层明确注释"terminal 留在 REST"（`packages/kap-server/src/transport/ws/v1/wsConnectionV1.ts:217-220`）而 REST 也只注册 list/create/get/close（`packages/kap-server/src/routes/terminals.ts:96-229`）——**write/resize/attach 无暴露通道**。requirements US5.1 的"写入即时送达/调整尺寸"在 v2 现网无消费面（v1 完整实现已随 server 删除）。→ 开放项。
5. **rg 供给两套不一致**：工具层可自动下载+校验，工作区搜索层（web 文件树 grep）无下载能力且降级文案不同（温和版 vs 安装指引版）。requirements US7.1 已按"单一供给"表述。
6. **v1 终端默认壳回退注释与实现不一致**：`terminalService.ts:267-270` 注释称必须用 `||`（空 $SHELL 会导致 spawn 失败），代码实际 `??`——空字符串 $SHELL 不触发回退。v1 无宿主，影响有限，作为基线记录。
7. **v2 终端默认壳无 Windows 回退**：`session/terminal/terminalService.ts:251-253` 的默认壳是 `$SHELL || '/bin/sh'`，无平台分支——Windows 上 $SHELL 通常未设，PTY spawn 会失败（v1 有 powershell.exe 回退但被上一条缺陷削弱）。requirements US5.1 已按终态要求跨平台回退。→ 开放项 O8。
8. **mergeStderr 未读管道隐患**：`os/backends/node-local/hostProcessService.ts:26-41, 95-97` 中 mergeStderr 两分支 stdio 完全相同，仅消费者侧句柄同流——子进程真实 stderr 管道从未被读取（可能因缓冲写满阻塞子进程），"内容级合并"不存在。requirements US4.3 已按句柄同流为基线、内容合并为设计增强表述。
9. **本地/远程按行读取的行终止符不一致**：本地三实现（kaos local `splitLinesKeepingTerminator`、v2 hostFsService `_readUtf8Lines`、AcpKaos readLines）保留行终止符；SSHKaos readLines 剥终止符（Python splitlines）。requirements US3.5 已记录并要求终态统一。
10. **rg 供给行为不对称**（§1.2 两套定位器的用户可见后果）：agent 工具层（Glob/Grep）下载失败**报错并附指引、无降级**；工作区搜索层无下载、有纯 Node 降级、v2 无用户可见文案（v1 有一次性告警）。超时/终止也不同（工具层 20s 两段杀；搜索层 30s 单段 SIGKILL）。requirements US7.1/7.2 已按分层表述。
11. **execEnv 双实现漂移面**：§1.3/差异 1-3 的根因。requirements P4 提案已收录。

### 3.4 需求文档超出 TS 现状、有意保留的内容（requirements 已标注为设计增强/提案，此处仅引用不裁决）

- US1.2〔设计增强〕远程主机事实探测缺失以明确错误暴露（终态须补齐或显式声明不可用）。
- US5.3〔设计增强〕终端 cwd 相对路径强制（TS wire 层已实现，实为基线；标注保留以强调该约束跨层一致性）。
- §9 P1-P5 五项提案（沙箱、远程产品化、rg 统一、探测合一、终端完整面）——全部标注〔提案〕，无实现。

## 4. 与相邻域文档的引用核对

- 工具调用域已在需求头部声明"执行原语（终端/文件/进程抽象）归 execution-environment 域"（`docs/specs/tool-calling-requirements.md` 范围边界段）；`tool-calling-builtin-tools.md` §0.5 路径守卫 + §2.1 Bash 与本文无重叠冲突（本文 §1.2/10 已反向引用）。
- 工作区管理域已声明"文件沙箱与执行环境（见执行环境域）"（`workspace-management-requirements.md` 范围边界段）；本域消费其"工作区根/信任门"概念，不重复定义。
- 对话渲染域的易失事件/游标概念覆盖终端帧与文件变更帧的**投影侧**；本域只保证帧产生侧（有序号、缓冲、回放）。
- Rust 侧：`docs/specs/rust-core-design/06-cross-platform.md`、`08-multi-workspace.md` 及 `plan/rust-v2-method-parity.md` 存在既有对齐跟踪，本域 ROADMAP 一句话引用不重复。

## 5. 本域 recon 中发现的可复用事实（供 Rust 团队直接取用）

- Windows 终止整树必须 `taskkill /T /F`（温和 taskkill 对控制台 node 树无效）；POSIX 必须负 PID 组杀 + ESRCH 幂等 + EPERM 回退——三平台差异的完整处置见 kaos/local.ts 与 hostProcessService.ts 双份一致实现。
- 登录壳 PATH 补全的防注入细节：`/usr/bin/env` 绝对路径调用（防 cwd 植入 + profile 函数遮蔽）、只导入绝对路径条目、unset PATH 不写回。
- Python errors= 语义的 ignore 模式必须手写解码器（保留合法 U+FFFD），TextDecoder 的 fatal:false 不满足。
- glob `**` 的零段/多段双情形展开、(dev,ino) 循环检测与 inode=0 文件系统的降级。
- 独占创建 + fsync 的组合（防"创建成功但断电丢内容"）。

## 6. env 命名规范与执行环境核验结论（B11 增量，2026-09-13）

> 依据 `plan/batches/B11-exec-env-verification.md`（缺口 D-02/D-03 的 Rust 等价核验/修复、D-14～D-18/D-22 命名规范归档）。本节为增量快照，不改写上文 2026-09-12 基线口径。

### 6.1 env 命名规范归档表（D-14～D-18/D-22）

**裁决基准**（总计划 DEC-1）：规范名 = Rust 现行 `MIRRI_*`，兼容别名 = `MIRRICODE_*`——先例是 mirri-core config 的 `env_var_with_alias` 模式（`rust/crates/mirri-core/src/config/env_layer.rs:479-485`；`MIRRI_MODEL_ID`/`MIRRI_API_KEY`/`MIRRI_BASE_URL`/`MIRRI_MODEL_PROVIDER`/`MIRRI_PERMISSION_MODE` 五组主名+别名，`:204-230`；空白值跳过语义 `:470`）。两代一致的 `MIRRICODE_HOME` 保留原名（TS v2 `packages/agent-core-v2/src/app/plugin/manager.ts:318` 与 Rust `rust/crates/mirri-core/src/config/home.rs:22` 均读此名）。本表是归档，不是全量改名令；Rust 无对应面的行如实记"无对应，待实装时采用规范名"。

| 用途 | 规范名 | TS v2 现行 | v1/库栈/文档站残留 | Rust 现行（B11 后） |
|---|---|---|---|---|
| 壳路径覆盖 | `MIRRI_SHELL_PATH` | 引擎栈 `MIRRI_SHELL_PATH`（`agent-core-v2/src/_base/execEnv/environmentProbe.ts:128`） | 库栈 `MIRRICODE_SHELL_PATH`（`packages/kaos/src/environment.ts:94`）；v1 错误文案（`agent-core/src/errors/codes.ts:436`） | **B11/T1 起采用**：`MIRRI_SHELL_PATH` 主名 + `MIRRICODE_SHELL_PATH` 别名（`rust/crates/mirri-os/src/shell.rs:57-69`） |
| mingw 探测集合 | —（非 env 名，行为分叉） | 引擎栈 mingw32/mingw64/ucrt64/clang64/clangarm64 全五类（`environmentProbe.ts:53-59`） | 库栈仅 mingw32/mingw64（`kaos/src/environment.ts:192`） | 无对应探测（Rust 终端不做 mingw 探测） |
| 遥测禁用 | `MIRRI_DISABLE_TELEMETRY` | kap-server（`packages/kap-server/src/services/telemetry.ts:27`） | CLI `MIRRICODE_DISABLE_TELEMETRY`（`packages/telemetry/src/bootstrap.ts:6, :25-33`） | 无（Rust 尚无遥测 sink，DEC-2；待实装时采用规范名） |
| 日志前缀 | `MIRRI_LOG_*` | `_base/log/logConfig.ts:52-58` | v1 `MIRRICODE_LOG_*`（`agent-core/src/logging/resolve-config.ts:22-37`） | 无专用日志 env（待实装时采用规范名） |
| Skill 目录占位符 | `${MIRRI_SKILL_DIR}` | `agent-core-v2/src/app/skillCatalog/registry.ts:176` | v1 + 文档站 `${MIRRICODE_SKILL_DIR}`（文档站已随 B11/T4 勘误） | n/a（占位符展开在 TS 侧） |
| 会话占位符 | `${MIRRI_SESSION_ID}` | `registry.ts:177`（未激活会话展开为空串） | 文档站未记载（B11/T4 已补记） | n/a（同上） |
| 插件根 env | `MIRRI_PLUGIN_ROOT` | `manager.ts:319, :764` | v1 TUI `MIRRICODE_PLUGIN_ROOT`（`apps/mirri-code/src/cli/sub/plugin-run-node.ts:6-16`，断链缺陷 D-01；文档站已随 B11/T4 勘误 + 两代差异注记） | 无对应 |
| home 目录 | `MIRRICODE_HOME`（两代一致，**保留原名**） | `manager.ts:318` | 一致 | `config/home.rs:22`、`agents_md.rs:37`、`mirri-embedded/src/providers/config_factory.rs:249` |
| 模型/权限 env | `MIRRI_MODEL_ID`/`MIRRI_API_KEY`/`MIRRI_BASE_URL`/`MIRRI_PERMISSION_MODE` 主名 | — | `MIRRICODE_*` 别名 | `env_layer.rs:204-230`（别名机制先例） |
| ACP 测试/注入 knob | `MIRRI_ACP_*`、`MIRRI_AGENT_ID`、`MIRRI_SESSION_ID`（agent env） | — | — | `rust/crates/mirri-acp/src/main.rs:52-106`、子代理注入 |

### 6.2 D-02 修复事实（B11/T1，2026-09-13）

- **修复落点**：`rust/crates/mirri-os/src/shell.rs`（新模块）——纯决策函数 `resolve_shell`（`:26-51`，注入全部 env 输入与 `can_find` 谓词，平台无关可测）+ 生产包装 `default_shell`（`:57-69`，读真实 env + `cfg!(windows)` + PATH 查找 `:85-93`）。
- **解析优先级**：显式参数 > `MIRRI_SHELL_PATH`（规范名）> `MIRRICODE_SHELL_PATH`（兼容别名）> `$SHELL` > 平台回退（Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe` 兜底；非 Windows: `/bin/sh`）。空白/纯空白值在所有层级跳过（对齐 `env_var_non_empty` 语义）。
- **接入点**：`rust/crates/mirri-core/src/terminal_pool.rs:327` `TerminalPool::detect_shell` 改为薄委派 `mirri_os::shell::default_shell`（`create_terminal` 必经此路径 `:348`，闭环）。不引入 `#[cfg(windows)]` 分支进 terminal_pool（macOS 无法编译验证 Windows-only 代码）。
- **测试**：纯函数 7 例（优先级链/空白跳过/Windows 回退顺序，`shell.rs` 内嵌 `#[cfg(test)]`）+ 集成 2 例（`rust/crates/mirri-core/tests/terminal_pool_provider_tests.rs:352, :376`，`ScriptedPtyProvider` 断言 `PtyConfig.program` 携带 env 覆盖；env 全局变异依赖 `--test-threads=1` 门禁）。
- **真机验证状态**：Windows 行为无法在本机（macOS）实测——静态核验 + 注入谓词测试桩覆盖，Windows 真机验证留待用户（无 `$SHELL`/无 env 时应解析为 pwsh/powershell/cmd 而非 `/bin/sh`；设 `MIRRICODE_SHELL_PATH` 旧名仍生效）。
- **与 US2.2 的已知出入**：requirements US2.2 验收要求覆盖变量"仅在路径真实存在时生效"（TS v2 引擎栈有该校验：`environmentProbe.ts:128-134` 对 override 做 `isFile` 才返回）；Rust 实现按 B11 计划设计裁决 §0.2 的优先级链**不做 env 覆盖值的存在性校验**——`MIRRI_SHELL_PATH` 指向不存在的二进制时直接选中、PTY spawn 报错（可诊断但不回落下一级）。出入如实留档，留待后续收敛（若收敛需在 `default_shell` 给覆盖值复用 `findable_on_path` 谓词并同步纯函数测试）。

### 6.3 D-03 核验结论（B11/T2，只读核验，Rust 无同类问题，不改代码）

TS v2 `os/backends/node-local/hostProcessService.ts:26-41, :95-97` 的 mergeStderr 隐患（子进程真实 stderr 管道从未被读取 → 缓冲写满可能阻塞子进程）在 Rust **无对应物**：

1. **进程创建**：`rust/crates/mirri-os/src/process.rs:225-227`（`LocalProcess::build_command`）stdin/stdout/stderr 三管道分别 `Stdio::piped()`，无 `Stdio::from(stdout_handle)` 类句柄同流——不存在"stderr 挂到 stdout 句柄"模式。
2. **主消费路径**：`rust/crates/mirri-core/src/tools/bash.rs:391-403`——`take_stdout`/`take_stderr` 取 owned 句柄后 `tokio::try_join!` **并发读双流**（注释明说顺序读有单流写满阻塞死锁风险），`wait` 最后。防回归测试已有：`bash.rs` `should_stream_stdout_and_stderr_updates_when_progress_handle_is_wired`。
3. **便利 API 弱序注记**：`rust/crates/mirri-os/src/process.rs:309-336`（`run_to_completion`）先读尽 stdout 再读 stderr 再 wait——两流都被读取、wait 最后，但顺序 await 是弱序：极端场景（子进程先写满 stderr OS 缓冲且 stdout 未到 EOF）理论上可阻塞。该函数为便利 API，当前无生产调用方（仓内仅测试消费），按 B11 计划**仅记录不修**；bash 主路径已并发读。

**处置**：D-03 不迁移进 Rust（无同类问题），本结论留档；TS 侧不修（DEC-1）。

### 6.4 D-35 Rust 侧核验结论（B11）

Rust 终端产品面完备：`terminal_pool.rs` 全 API（create/write/resize/kill/get_output_since/attach_sink_with_replay/detach_sink/last_seq/list_terminals/close_all）；REST `rust/crates/mirri-server/src/http.rs:71-77`；WS 五个连接本地控制帧 + 乱序安全 attach 协议（`rust/crates/mirri-server/src/ws_rpc.rs:813-830`）。web 入口接线（T3）不在本轨，随 web 队列另批实施。

### 6.5 D-35 web 侧接线结论（B11/T3，2026-09-13）

web 终端面板接线落地（缺口 = 入口接线 + i18n 化，非新面板，见 B11 计划 R-5）：既有三层（`apps/mirri-web/src/api/daemon/ws-rpc-api.ts` 终端 RPC 桥含 F-P3-4 seq 游标 gap recovery、`src/composables/useTerminal.ts`、`src/components/Terminal.vue` xterm 面板）挂载进共享右侧 detail 层——`DetailTarget` 增 `'terminal'`（`src/composables/useFilePreview.ts`），入口 = ChatHeader 终端按钮（桌面，session 门控）+ MobileTopBar 镜像（≤640px，ChatHeader 在移动端不渲染）；工具栏 kill/dismiss 语义分离（End process = 杀进程+收面板，X = 仅收面板），Escape 对聚焦 xterm 表面穿透（VS Code 集成终端规则），全部文案 i18n en/zh 双写。接线中修复一处既有隐患：`useTerminal` 原先二次 `connectEvents` 会**替换** v3 单连接事件 handler 集（劫持聊天事件流）且 unmount `close()` 会拆共享传输——现 `WsRpcApi.connectTerminalEvents()` 以 additive `terminalSinks` 挂载（`ws-rpc-api.ts`，identity 校验释放、不触碰 app handler、不关传输），回归测试双层钉住（`test/use-terminal.test.ts`、`test/ws-rpc-terminal.test.ts`）。A1 实跑验收（attach 回放/resize/exit/移动端/刷新回放）留 B12 统筹实跑。
