# 执行环境域 — 架构文档

> 状态：v1 基线 · 2026-09-12 ·
> 本文记录执行环境域的技术形状：TS 实现机制（作为 Rust 对齐基线）+ 本域裁决落定的契约形状；现状代码事实（file:line 盘点）见 [execution-environment-context.md](execution-environment-context.md)，不在本文维护。
> 配套：需求（含验收项与提案）见 [execution-environment-requirements.md](execution-environment-requirements.md)；Rust 引擎侧目标架构见 [rust-core-design/](rust-core-design/)（含跨平台与多工作区设计），本文不重复其内容，仅声明域内消费关系。
> 术语：工作区（W1）的唯一规范定义见 [glossary.md](glossary.md)。

## 1. 双代实现并存格局（动机与分层）

本域在 TS 侧存在**两代并行的实现栈**，均健在：

| 栈 | 抽象形态 | 消费方 |
|---|---|---|
| **统一执行环境库（kaos）** | 一个自包含的接口对象，集文件/路径/进程于一身，经 AsyncLocalStorage 隐式传递 | v1 引擎工具层（v1 Bash/Read/Write/Edit/Glob/Grep 工具）、SDK 的会话恢复路径、ACP 适配器（编辑器桥接） |
| **主机服务层（v2 os 域）** | 按能力拆分的四个 DI 服务（文件系统/进程/终端/主机环境）+ 一个文件监视服务，接口与 node-local 后端分离 | v2 引擎全部工具与界面（Bash/Read/Write/Glob/Grep、工作区文件树、终端、git 状态） |

**动机（抽象问题陈述）**：agent 的执行能力天然有"在哪台机器上执行"这个变化维度；把该维度收敛到一层抽象后面，上层（工具、界面）就能在本地、远程、宿主桥接之间无感切换。两代实现是这个同一问题的两次解法：库形态胜在**可注入性**（远程/桥接实现可作为值传递），服务形态胜在**可组合性**（按能力独立替换后端，如未来非 Node 后端）。Rust 内核应取两者之长：单一抽象 + 可替换后端。

两代实现共享一批**纯函数原语**（文本解码错误处理、流式缓冲读、glob 模式编译），这些原语在两代中是逐行同源的分叉副本（见 §8）。

## 2. 执行环境库的接口形状（kaos 栈）

统一接口把执行环境建模为"一个有名字、有主机事实、有工作目录、有环境变量层的执行体"：

```
interface Kaos {
  name: string            // 环境名（"local" / "ssh" / 桥接名），用于日志与辨识
  osEnv: Environment      // 主机探测快照（osKind/osArch/osVersion/shellName/shellPath）
  // 路径面（同步）：pathClass() / normpath() / gethome() / getcwd()
  // 目录面（异步）：chdir() / withCwd() / withEnv() / stat() / iterdir() / glob()
  // 文件面（异步）：readBytes/readText/readLines/writeBytes/writeText/mkdir
  // 进程面（异步）：exec(args) / execWithEnv(args, env) → KaosProcess
}
```

设计裁决：
- **不可变环境派生**：`withCwd` / `withEnv` 不修改原实例而是返回带新绑定的新实例；多会话并发下"切目录"天然无锁。`chdir` 则是实例内可变（对齐 Python os 习惯，但作用域限于该实例，不碰进程全局 cwd）。
- **环境变量层叠**：`withEnv` 叠加的是"层"（layer）而非快照——层记录在创建时，**在 spawn 时读取**，调用方持有一个稳定 record 并在执行间修改它即可影响后续执行。
- **spawn 时的 env 合并顺序**：宿主进程环境 → 调用方单次覆盖 → 各 withEnv 层（层按叠加顺序胜出）。
- **隐式上下文**：`runWithKaos(env, fn)` 把环境绑定到 async 上下文树；`getCurrentKaos()` 无绑定时抛带修复指引的错误（防"静默落到全局默认"这类难查的行为）。

三个实现：
1. **LocalKaos**：node:fs/promises + node:child_process 直连本机；`create()` 时并发执行主机探测与登录壳 PATH 补全（两者均进程级 memoize）。
2. **SSHKaos**：文件面走 SFTP，进程面走远端 exec；构造时建立连接、解析真实主目录与初始 cwd（cwd 不存在或非目录即失败）。详见 §6。
3. **AcpKaos**（ACP 适配器内）：装饰器模式——`read*/write*` 反向 RPC 到宿主编辑器（如编辑器未保存缓冲视角），其余全量委派 inner LocalKaos。构造零 IO，能力未声明时根本不包装（工具层完全无感）。

## 3. 主机环境探测机制

探测是**纯函数 + 注入探测依赖**的形态（platform/arch/release/env/isFile/execFileText 全部注入），生产端用 Node 环境的默认实现打包，进程级 memoize——同一套探测可在任意宿主 OS 上确定性重放。

流程（POSIX）：`/bin/bash → /usr/bin/bash → /usr/local/bin/bash` 依次存在性探测，全miss则回退 `sh /bin/sh`（不失败）。

流程（Windows）：以 **Git Bash 为唯一规范壳**。探测序：用户覆盖变量（存在性校验）→ 从 PATH 上的 git.exe 位置推断（仅锚定 `cmd\`、`bin\` 布局）→ `git --exec-path` 输出推断（带 5s 超时）→ 固定安装位置清单（含用户本地程序目录）→ 全miss抛"未找到壳"错误（消息携带**全部已检查路径**与 Git for Windows 安装指引）。v1 引擎层另将该错误映射为公开错误码（携带同样的安装 action 文案；v2 共享该错误码枚举）。

**登录壳 PATH 补全**：POSIX 下执行 `$SHELL -l -c /usr/bin/env`（5s 超时），取输出中**最后一行** `PATH=`（profile 噪声在前）；合并规则 = 保留现有 PATH 原串（含空分量语义）、只追加缺失的**绝对路径**条目、无新增则原样返回不写回；`$SHELL` 空白时回退 OS 用户数据库的登录壳。`/usr/bin/env` 用绝对路径调用——规避 cwd 中植入的 `env` 二进制与 profile 函数遮蔽。

**就绪门槛（v2 服务层）**：探测服务暴露 `ready` promise，字段访问在未就绪时抛编程错误（Bug 指示类）；组装根在创建任何会话作用域前 await ready。

## 4. 进程执行机制

### 4.1 spawn 与失败面

- spawn 选项固定形态：三管道 stdio、`detached: !isWindows`（POSIX 下子进程成为进程组长）、`windowsHide: true`。
- **waitForSpawn 门**：spawn 后先等 `spawn`/`error` 事件裁决"真的起来了"，再返回句柄——杜绝向从未存在的进程写 stdin。spawn 失败（ENOENT/EACCES）的翻译分栈：v2 服务层翻译为域名化错误（`os.process.spawn_failed`，携带 command/args/cwd/errno）；kaos 栈原样抛 Node 原始错误（已知分叉，Rust 侧应统一为域名化）。
- env 覆盖语义：未传 = `undefined`（子进程**原样继承**宿主 env，包括空值变量）；传了 = 宿主 env 浅拷贝 + 覆盖项（宿主 env 不会被"少传了"而丢）。

### 4.2 终止语义（进程树，跨平台核心裁决）

- **POSIX**：`process.kill(-pid, signal)` 负 PID 组杀——bash 拉起的后台孙子一起结束。异常分支：`ESRCH`（组已回收）视为成功（幂等）；`EPERM` 回退 `child.kill()` 直杀（尽力而为）；其余抛域名化 kill 失败错误。
- **Windows**：`ChildProcess.kill()` 只杀壳父进程且实际总是强杀，故一律 `taskkill /T /F /PID` 强杀整棵树（控制台 node 树对温和 taskkill 免疫，Windows 无真正的可优雅终止信号）。
- pid ≤ 0（spawn 失败哨兵）时 kill 直接成功返回——**绝不**对负 PID 发组信号（会击杀无关进程组）。
- 远程（SSH）：信号按会话协议投递（剥 `SIG` 前缀），支持优雅终止请求。

### 4.3 输出流：BufferedReadable

stdout/stderr 包装为"保背压 + 终后可读"的双模流：消费方未读时对源 pause（背压），源结束后缓冲保留供事后读取（wait-then-read 模式），预取窗口 128KB 上限防无界吃内存。`dispose()` 幂等销毁三个流。stderr 可选合并进 stdout（mergeStderr，消费方句柄引用同一流对象）——注意：TS 当前实现中子进程真实的 stderr 管道**未被读取合并**（存在写满阻塞子进程的隐患，见 context），"内容级合并"是终态要求而非现状。

### 4.4 进程运行器分层（v2 消费链）

```
工具/文件搜索（消费者）
  └─ ISessionProcessRunner.exec(args, {cwd?, env?})     // 会话作用域契约
       └─ 默认 cwd：无 handler 时 = 会话 cwd；有 handler 时 = 工作区根
            └─ IHostProcessService.spawn()               // App 作用域、真正起进程
```

工作区级运行器在会话创建时作为作用域种子注入，遮蔽默认注册——同一进程内多工作区各自的根目录默认值互不串扰。文件搜索（rg）与 Bash 工具共用这条链，从不直接调 node:child_process。

## 5. 终端仿真机制（v2 链路）

```
会话终端服务（Session 作用域：帧缓冲、序号、生命周期）
  └─ IHostTerminalService.spawn()      // App 作用域：node-pty 工厂
```

- **PTY 工厂**：node-pty 懒加载（import 时不触发原生模块构建/解析），spawn 参数 `xterm-256color` + 继承宿主 env；工厂持有全部存活 PTY 的集合，App dispose 时全量 kill（单个失败吞掉继续）。
- **帧协议**：每终端维护 `nextSeq` 单调递增的输出帧缓冲（环形，默认 2000 帧，超限丢最老）；attach 携带 `sinceSeq` 回放其后全部帧并返回回放数；**退出帧的 seq 固定为最大安全整数**——保证带游标重放时退出事件总能补发。exit_code 为 null 专表"服务端显式关闭"，与自然退出码区分。
- **cwd 约束**：创建契约（zod）在 wire 层就拒绝绝对路径（`/`、`\`、盘符形态），服务层再经工作区允许目录断言（执行档位）校验相对路径。
- 默认壳 `$SHELL || '/bin/sh'`（注意：与 v1 终端服务的 `win32 → powershell.exe` 回退不同，见 context 差异矩阵）；默认尺寸 80×24。

## 6. 远程执行环境机制（SSHKaos）

- **认证**：密钥（内容或路径）与密码构造认证队列，逐个尝试；host key 校验当前**无条件放行**（对齐原 asyncssh known_hosts=None 行为——Rust 侧应视为待改进项而非基线）。
- **文件面 = SFTP**：全部操作经 SFTP RPC，错误统一映射（不存在/权限拒绝/连接丢失/通用），目录冲突语义与本地对齐（"已存在但非目录"硬冲突、递归创建逐级校验、竞态创建收敛）。
- **进程面 = 远端 exec**：命令构造为 `cd '<cwd>' && K='v' ... <cmd> <args...>`——**环境变量以壳内联赋值注入**而非 exec 的 env 通道（后者被 sshd 的 AcceptEnv 白名单静默丢弃，是知名坑）；每个 token 经 POSIX 壳引号转义，变量名过 POSIX 合法名校验（防注入面）。
- **能力缺口**：远程环境探测未接线（osEnv 抛未实现）、PID 不可得（-1 哨兵）、SFTP 无 inode/device 导致 stat 字段缺省（0）、大小写不敏感 glob 明确拒绝。另注意：远程按行读取剥行终止符（Python splitlines 语义），本地各实现保留终止符——同接口两环境行形状不一致（Rust 须裁决统一，见 requirements US3.5）。
- stat 的 mode 位从 SFTP 属性重建（布尔类型辅助方法 → 文件类型位）。

## 7. 文件系统原语与约束机制（v2）

### 7.1 主机文件系统服务

直接落 node:fs/promises，接口面刻意窄（readText/writeText/appendText/readBytes/writeBytes/readLines/createExclusive/stat/lstat/readdir/mkdir/remove/realpath）。要点：
- **createExclusive**：`wx` 独占打开 + 写入 + **fsync 落盘**；EEXIST 返回 false 不抛（原子"创建或知道失败"）。
- **readLines**：UTF-8 走 64KB 分块流式切行；BOM 容忍规则按"行首偏移是否为 0"区分（仅首行保留 BOM 语义）；非 UTF-8 编码整读后切。
- **错误边界**：全部操作经纯函数翻译器（errno → 域名码 `os.fs.*`，幂等），错误附带 path/op/errno/syscall 明细。域名表 8 码（not_found/is_directory/not_directory/already_exists/permission_denied/not_empty/unavailable/unknown），后两者标记可重试。
- `remove` = rm recursive + force（不存在不报错）。

### 7.2 工作区文件服务（界面/文件树层）

- **路径约束两段式**：① 词法判定（拒绝空/绝对/`..` 段，解析后须落在主根 ∪ 附加目录集内）② **符号链接防逃逸**——对候选解析"最长已存在前缀的 realpath"（尾部不存在路径仍可判，向上回退拼接，防无限循环），再与各根的 realpath 集合比较；根 realpath 结果按目录集缓存。逃逸错误统一码（`FS_PATH_ESCAPES` 族）带 reason 细分（empty/absolute/dotdot_segment/resolved_outside/symlink_outside）。
- 读窗口 10MB 上限、4096 字节二进制嗅探（NUL 即二进制，否则不可打印占比 >30%）、etag（mtime/size/inode 三元组 Base36）、mime/语言按扩展名表。
- 搜索：rg（JSON 流式、begin/context/match/end 四类记录累积，限额三档触发即 SIGKILL）→ 无 rg 时纯 Node 降级（telemetry 记录降级原因）→ 文件名模糊搜索为纯 Node 递归 walk（深度上限、跳 .git、子序列打分）。

### 7.3 文件监视

- 底层统一 chokidar（非 fs.watch）：`ignoreInitial`、不跟随符号链接、非递归时深度 0、默认忽略 `.git` 与 `*.sock`；内部测试开关可切 polling 模式（规避特定 macOS 满负荷丢事件）。
- **单 watcher 分发模型（v2）**：每个工作区 handler 只挂一个递归 watcher（订阅者集合非空才启动、清空即停），gitignore 匹配器懒加载缓存；原始事件按订阅者的 watched 子树分发；**引用计数模型（v1）**：按（连接 × 会话）对路径去重共享底层 watch。Rust 侧可参考单 watcher 的资源效率与引用计数的多连接隔离两者。
- 防抖窗口与突发截断：200ms 合并（v2 可用环境变量调节）；窗口内超 500 条 → `truncated + count + 空明细`（驱动客户端全量重同步）。帧带会话级单调 seq，为易失事件（不进日志）。

### 7.4 附加目录集（放宽机制）

- 数据形态：`文件目录 ∪ 临时目录` 去重并集；文件目录持久在工作区本地配置（外部编辑 → 监视 + 200ms 防抖重载），临时目录进程内存（同工作区全部会话共享）。
- 全部变更经**单条尾队列串行化**（读写不交错）；生效方式是**播种**——附加目录集随会话创建种子进会话信息，被路径约束判定、文件监视的工作区判定一致消费。
- 对外暴露变更事件（界面可提示刷新）。

## 8. 共享纯函数原语（双栈同源分叉）

以下原语在库栈与引擎栈各有一份逐行同源副本，Rust 侧应单实现：

| 原语 | 语义要点 |
|---|---|
| 文本解码错误处理 | Python `errors=` 三态：strict（抛）/ replace（U+FFFD）/ ignore（**丢无效字节但保留合法 U+FFFD**——手写解码器，非 TextDecoder 的全删）；非 UTF 族编码走无损 Buffer.toString（errors 无意义） |
| 流式缓冲读 | §4.3 |
| glob 模式编译 | `*`→`[^/]*`、`?`→`[^/]`、`[!...]` 取反、`^` 字面量、转义处理；dotfile 默认匹配（pathlib 语义） |
| glob 遍历 | `**` 的零段/多段双情形展开（子目录递归保留完整 patternParts，杜绝深度≥1 双计）；目录循环检测以 (dev,ino) 为键、**inode=0 的文件系统（部分 Windows）跳过检测**（正常遍历优先于误杀） |
| 行读取 | 64KB 分块、跨块行拼接、行首偏移参与 BOM 判定 |

**已发生的分叉漂移**（Rust 收敛依据，详见 context 差异矩阵）：壳覆盖变量名（`MIRRICODE_SHELL_PATH` vs `MIRRI_SHELL_PATH`）、mingw 前缀集合（两枚 vs 五枚）、未找到壳的错误类型（专用错误类 vs 普通 Error）、探测返回是否含 pathClass/homeDir。

## 9. 搜索二进制（rg）供给

两套定位器并存（工具层 / 工作区搜索层），共享"探测→缓存→执行"骨架但能力不同：

- **工具层定位器**（4 级）：系统 PATH（仅存在性检查，不执行版本探测）→ vendor 钩子（当前恒空）→ 用户二进制目录缓存 → **自动下载**（固定版本、6 平台固定 SHA-256、tar/zip 解包、staged 临时目录 + rename 原子安装、校验失败明确报"渠道内容可能已变"）。
- **工作区搜索层定位器**（2 级）：`rg --version` 探测（经会话进程运行器，在 handler cwd 下执行）→ 缓存二进制探测。**无下载能力**。
- rg 执行（两套同构）：20s 超时、SIGTERM 5s 宽限后 SIGKILL 两段杀、stdout/stderr 各 10MB 截断、EAGAIN 时单线程重试一次。
- 降级文案与遥测：无 rg 时的降级经 telemetry 事件记录；下载也失败时用户可见文案附安装/手动放置指引。

## 10. 与相邻域的接口关系

| 相邻域 | 边界 |
|---|---|
| 工具调用域 | 本域提供进程运行器/文件原语/路径守卫的**机制**；工具的参数契约、结果回传、逐工具限制（Read 行数、Write 模式、Glob 匹配数）归工具调用域的内建工具规格。Bash 的超时转后台/任务面板归后台任务域 |
| 工作区管理域 | 本域消费"工作区根目录"与附加目录集；工作区的注册/信任/生命周期归工作区管理域。信任门对项目本地配置（含附加目录的持久文件）的开关语义归工作区管理域 |
| 会话管理域 | 终端归属会话；会话恢复/续聊的编排归会话管理域与持久化域，本域保证终端帧缓冲的回放契约 |
| 对话渲染域 | 终端输出帧/文件变更帧是易失事件（不进日志），其投影与断线重连的会话级游标机制归对话渲染域；本域只保证帧有序号与缓冲语义 |
| MCP 域 | MCP 服务器子进程的拉起属 MCP 域（可复用本域进程原语） |

## 11. 架构裁决记录（ADR）

（暂无。首次基线建立，后续设计变更以 ADR 追加。）
