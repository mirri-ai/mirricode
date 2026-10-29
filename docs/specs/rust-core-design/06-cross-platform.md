# Rust Core 跨平台设计文档

> **文档类型：设计类**（按 `docs/specs/AGENTS.md` 治理约定）——只因设计变更修改。结构快照（2026-09-04）：OS 抽象 crate 实名为 `mirri-os`，规划模块与现状的差距标注见 §1.4，差距的当前状态以 [ROADMAP.md](ROADMAP.md) 为准。
> （曾规划为 platform 名，同名 crate 从未存在；模块现状见 §1.4）。
> §11.1 的「业务 crate 一律禁 `#[cfg]`」绝对铁律按分级规则修订。

> 状态: Draft
> 日期: 2026-08-29
> 作者: Architecture Team
> 前置文档: [00-overview.md](00-overview.md) · [05-crate-structure.md](05-crate-structure.md)
> 范围: Linux / macOS / Windows 三平台行为一致性设计

## 1. 设计目标

Mirri Code 的 Rust core 必须在 Linux、macOS、Windows 上以**一致的语义**运行。跨平台不是"能在三端编译通过"，而是"同一份业务逻辑在三端产生相同的行为可观测结果"。

### 1.1 核心原则

| # | 原则 | 含义 |
|---|------|------|
| P1 | **业务逻辑零 `#[cfg]`** | `mirri-core` 零条件编译（trait-only）；所有平台差异收敛到 `mirri-os` 或经 trait 抽象；其余业务 crate 尽量避免，确需平台分支时收敛到单一模块并注释说明（分级规则见 §11.1） |
| P2 | **线格式统一 POSIX 风格** | 所有跨进程/跨平台传输的数据（路径、文本、命令）使用单一 wire format，平台差异只在 OS 边界转换 |
| P3 | **平台差异收敛到 `mirri-os`** | 唯一的平台抽象 crate；`#[cfg]` 只在其中出现 |
| P4 | **依赖注入** | 引擎通过 `Box<dyn Trait>` 持有平台能力，测试注入 fake 实现，不触碰真实 OS |
| P5 | **差异必须文档化** | 无法消除的行为差异（见 §13）明确登记，禁止"悄悄不同" |

### 1.2 一致性矩阵

| 层面 | 必须一致（语义等价） | 允许差异（内部实现） |
|------|---------------------|---------------------|
| 协议线格式 | JSON-RPC 2.0 帧、路径 wire 格式（POSIX）、文本一律 LF | — |
| 文件系统语义 | 读文件返回的内容、路径校验结果、watch 事件的**最终合并结果** | 底层后端：inotify / FSEvents / ReadDirectoryChangesW |
| 进程语义 | `wait()` 退出码、kill 后的终态、stdio 流行为 | 信号投递机制（SIGTERM vs TerminateProcess） |
| 环境变量 | key 查找大小写不敏感（Windows）/ 敏感（Unix）的**可观测结果** | 内部存储结构 |
| 路径 | wire 上的字符串表示完全一致 | OS 原生 `PathBuf` 表示 |

### 1.3 非目标

- 不追求三平台字节级一致（如配置文件在磁盘上的权限位表示不同是允许的）
- 不抽象 UI 层（TUI/NAPI 各自的平台 API 不在本设计范围）
- 不为 Windows 复刻 POSIX 语义（如 `SIGTERM` 优雅退出），而是定义**引擎侧统一的终止契约**

### 1.4 模块规划

`mirri-os` 在现有六模块（`fs` / `glob` / `process` / `pty` / `search` / `path_access`）之上，本文档设计以下新增模块（各模块的当前现实状态见 [context.md](context.md) §4）：

| 新增模块 | 文件 | 对应章节 |
|----------|------|---------|
| 路径编解码 | `src/path_codec.rs` | §2 |
| Shell 抽象 | `src/shell.rs` | §3 |
| 文件监听 | `src/fs_watcher.rs`（新增依赖 `notify`） | §4 |
| PTY 抽象 | `src/pty.rs`（现有模块收敛为 `PtyProvider` trait） | §5 |
| IPC 传输 | `src/ipc.rs` | §6 |
| 文本行尾 | `src/line_ending.rs` | §8 |
| 文件权限 | `src/permission.rs` | §9 |
| 环境变量 | `src/env.rs` | §10 |
| 平台分发器 | `src/platform/mod.rs` | §11 |

## 2. PathCodec Trait — 路径编解码

### 2.1 动机

Windows 使用 `\` 与盘符（`C:\`），Unix 使用 `/` 与根（`/`）。业务逻辑（session 持久化、tool 参数、MCP 协议、日志）中路径以字符串形式跨层传递，若不统一编码，同一路径在三端会产生三种字符串表示，导致协议无法互通、持久化数据不可迁移。

**决策：wire 格式固定为 POSIX 风格**（与 00-overview §5 "路径编码" 决策一致），Windows 路径在 OS 边界转换。

### 2.2 Wire 格式定义

- 分隔符恒为 `/`，**绝不允许** `\`
- 绝对路径以 `/` 开头；相对路径以非 `/` 开头，原样保留
- Windows 盘符编码为 `/盘符/...`，盘符大写：
  - `C:\Users\foo` → `/C/Users/foo`
  - `D:\work\a.txt` → `/D/work/a.txt`
  - `C:\`（根目录）→ `/C`
- 相对路径：`Users\foo`（Windows 原生）→ `Users/foo`
- UNC 路径 `\\server\share\foo`：**v1 明确拒绝**，转换时返回错误（保留前缀 `/UNC/` 作为未来扩展，不占线格式）

| 平台 | OS 原生 | wire |
|------|---------|------|
| Linux | `/home/alice/proj/src` | `/home/alice/proj/src`（不变） |
| macOS | `/Users/alice/proj/src` | `/Users/alice/proj/src`（不变） |
| Windows | `C:\Users\alice\proj\src` | `/C/Users/alice/proj/src` |
| Windows | `C:\` | `/C` |
| Windows | `src\main.rs`（相对） | `src/main.rs` |

### 2.3 Trait 定义

```rust
// mirri-os/src/path_codec.rs
use std::path::{Path, PathBuf};

/// 路径编解码器：wire 格式（POSIX 风格）与 OS 原生格式互转。
///
/// 单向性保证：`from_wire` 只负责"解释" wire 字符串，不保证与
/// `to_wire` 互为逆运算（见 2.4 规范化）；但 wire → OS-native →
/// wire 应保持恒等（幂等）。
pub trait PathCodec: Send + Sync {
    /// wire → OS 原生路径。
    /// 约定：Unix 上即 `PathBuf::from(path)`；Windows 上执行盘符解码。
    fn from_wire(&self, path: &str) -> PathBuf;

    /// OS 原生 → wire 字符串。
    /// 约定：Unix 上原样输出（要求输入为 `/` 分隔）；Windows 上执行盘符编码。
    fn to_wire(&self, path: &Path) -> String;
}
```

### 2.4 规范化（Canonicalization）

`to_wire` 之前必须规范化，保证 wire 表示确定性：

1. 分隔符统一为 `/`（Windows 上 `\` → `/`）
2. 盘符大写（`c:` → `C:`）
3. 移除 `.` 与 `..` 段（词法级，不触碰文件系统）
4. 已存在的路径使用 `std::fs::canonicalize` 解析真实路径（含 symlink）；不存在的路径退化为词法规范化
5. 尾部 `/` 仅保留给根路径 `/` 与盘符根 `/C`

Windows 保留名（`CON` / `NUL` / `COM1` 等）为已知限制，见 §13。

### 2.5 Workspace 边界校验

**业务规则：引擎接触的任何绝对路径都必须位于 workspace 边界之内**（`fs.read` / `fs.write` / tool 参数 / session 恢复均适用）。校验与路径编解码分离为独立组件：

```rust
// mirri-os/src/path_codec.rs（续）
pub struct WorkspaceBoundary {
    root: PathBuf,          // 已 canonicalize 的 workspace 根
    codec: Box<dyn PathCodec>,
    case_insensitive: bool, // 平台判定：Windows/macOS 默认 true，Linux false
}

impl WorkspaceBoundary {
    /// 校验 path 位于 root 之内，返回规范化后的绝对路径。
    /// 失败返回 Err(PathAccessDenied) 或 Err(SymlinkEscape)（见 symlink 规则）。
    pub fn validate(&self, path: &Path) -> anyhow::Result<PathBuf>;

    /// 校验 wire 格式路径（内部先 from_wire 再 validate）。
    pub fn validate_wire(&self, path: &str) -> anyhow::Result<PathBuf>;
}
```

校验步骤（`validate`）：

1. `path` 转绝对路径（相对路径以 workspace root 为基准拼接）
2. 词法层：逐段移除 `.` / `..`，前缀检查 `starts_with(root)`
3. **大小写敏感性由平台决定**：`case_insensitive` 为真时（Windows、macOS 默认 APFS）比较前将两侧统一小写，避免 `C:\Work` 与 `c:\work` 的误判；Linux 上严格区分
4. Symlink 层校验（见 2.6）

### 2.6 Symlink 处理

`std::fs::metadata` 会跟随 symlink，`symlink_metadata` 不会——校验必须使用后者，防止 **symlink 逃逸**（`workspace/link -> /etc`）。

```rust
/// 递归校验 symlink 链：路径的每一级分量都必须留在 workspace 内。
///
/// 规则：
/// 1. 用 symlink_metadata 逐级检查（不跟随）每个分量是否为 symlink；
/// 2. 命中 symlink 时，解析其目标（canonicalize），递归校验目标；
/// 3. 目标逃逸出 root → Err(SymlinkEscape)；
/// 4. 递归深度上限 40 层（防环），超出 → Err(SymlinkLoop)。
fn resolve_within_workspace(path: &Path, root: &Path) -> anyhow::Result<PathBuf>;
```

Windows 上 symlink 创建需要开发者模式/管理员权限，引擎策略见 §13.2。

### 2.7 测试要点

- roundtrip：`to_wire(from_wire(w)) == w`（幂等）
- 三平台转换对照表（§2.2）逐条断言
- 逃逸用例：workspace 内 symlink 指向 `/etc`、指向 workspace 外相对路径、symlink 环
- 大小写：Windows 上 `C:\Work` 与 `/c/work` 判定为同一路径

## 3. ShellProvider Trait — Shell 抽象

### 3.1 动机

Shell tool（及任何"执行用户 shell 命令"的能力）在三端依赖不同的解释器：Unix 为 bash/zsh/sh，Windows 为 PowerShell/cmd。引擎不能硬编码 `bash -c`（Windows 上不存在），也不能硬编码 `cmd /C`（Unix 上不存在）。

### 3.2 数据结构

```rust
// mirri-os/src/shell.rs
use std::collections::HashMap;
use std::path::PathBuf;

/// 一个可执行的 shell 解释器描述。
pub struct ShellSpec {
    pub program: String,          // 如 "/bin/bash" / "pwsh"
    pub args: Vec<String>,        // 如 ["-c"]
    pub env: HashMap<String, String>, // 需要注入的附加环境（如 SHELL 归一化）
}

/// 解析结果：可直接交给进程层 spawn 的命令。
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

pub trait ShellProvider: Send + Sync {
    /// 当前平台的默认 shell。
    fn default_shell(&self) -> ShellSpec;

    /// 将一段 shell 命令字符串解析为可执行命令。
    /// Unix:  (bash|sh, ["-c", input])
    /// Windows: (pwsh|powershell, ["-Command", input]) 或 cmd 回退
    fn resolve_command(&self, input: &str) -> CommandSpec;

    /// 在 PATH 中查找可执行文件（按平台分隔符拆分）。
    fn resolve_on_path(&self, program: &str) -> Option<PathBuf>;
}
```

### 3.3 平台实现

| 平台 | 默认 shell | 执行方式 | 检测优先级 |
|------|-----------|---------|-----------|
| Linux | bash | `bash -c "<input>"` | `$SHELL` → 存在性探测（`/bin/bash` → `/bin/sh`） |
| macOS | zsh（`$SHELL` 常为 zsh）→ 回退 bash | `zsh -c` / `bash -c` / `sh -c` | 同 Linux |
| Windows | PowerShell | `pwsh -Command` → `powershell.exe -Command` → `cmd /C` | `pwsh` on PATH → `$PSModulePath` 探测 → `cmd` |

具体规则：

- **Unix**：读取 `$SHELL`，取其 basename 判断是否为已知 shell（`bash` / `zsh` / `sh` / `fish` 等）；若指向不存在文件则依次回退 `/bin/bash` → `/bin/sh`。`bash -c` 与 `sh -c` 差异仅在解释器选择，`resolve_command` 返回的 `CommandSpec` 统一为 `(shell, ["-c", input])`
- **Windows**：优先 `pwsh`（PowerShell Core，跨平台行为与 Unix 端 PowerShell 一致），PATH 探测失败回退 `powershell.exe`；`$PSModulePath` 存在视为 PowerShell 可用信号。`cmd /C` 仅作最后回退（引号转义规则不同，见 3.5）

### 3.4 PATH 解析

```rust
/// 按平台分隔符拆分 PATH：Unix 为 ':'，Windows 为 ';'。
/// Windows 上依序尝试 PATHEXT 扩展名（.EXE/.BAT/.CMD/.COM）。
fn resolve_on_path(&self, program: &str) -> Option<PathBuf> {
    // 内部实现：
    //   env PATH -> split(SEPARATOR)   // cfg 判定只在 mirri-os 内
    //   for dir: dir.join(program) 存在且(Unix: 有执行位 | Windows: 扩展名命中 PATHEXT)
    //   Windows 额外：当前目录优先（与 cmd 行为一致）
}
```

### 3.5 已知差异与对策

| 差异 | 对策 |
|------|------|
| 引号/转义规则：POSIX 单双引号语义 ≠ PowerShell 反引号 ≠ cmd `^` | 引擎不解析命令内容，整串透传给解释器；结构化 argv 路径（非 shell 字符串）由调用方显式传 `env`/`args`，不经 shell |
| 内建命令差异（`ls` 不存在于 Windows） | Windows 端文档注明：shell 命令语义以 PowerShell 为准，测试仅断言退出码与输出内容 |
| `$SHELL` 可能指向交互式 shell 或不存在路径 | 存在性探测 + 白名单 basename，失败即回退 |

### 3.6 测试要点

- 三端 `resolve_command("echo hi")` 返回的 `CommandSpec` 语义等价（执行结果一致）
- `resolve_on_path("node")` / `resolve_on_path("npm.cmd")` 平台差异断言
- `$SHELL` 指向不存在文件时回退到 `/bin/sh`
- Windows 大小写：`Path` 与 `PATH` 同时存在于环境中时按 case-insensitive 查找

## 4. FsWatcher Trait — 文件系统监听

### 4.1 动机

配置热加载（config hot-reload）、workspace 变更事件等需要监听文件系统。三端底层机制完全不同：Linux inotify、macOS FSEvents、Windows ReadDirectoryChangesW。

**决策：以 `notify` crate 为统一后端**（已跨平台封装上述三机制），`mirri-os` 在其上做薄封装，定义引擎侧 trait——这样业务层只依赖 `FsWatcher`，且测试可注入 fake。

### 4.2 Trait 定义

```rust
// mirri-os/src/fs_watcher.rs
use std::path::PathBuf;

/// 由 notify::EventKind 归类而来，仅保留业务需要的四类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind { Create, Modify, Delete, Rename }

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub kind: FsEventKind,
    pub path: PathBuf,
}

/// RAII 句柄：Drop 时停止监听并回收线程/资源。
pub struct WatchHandle {
    // 内部持有 notify 的 RecommendedWatcher 与 debounce 任务
}

pub trait FsWatcher: Send + Sync {
    /// 监听 path（文件或目录，目录为递归监听）。
    /// callback 在事件线程被调用，必须快速返回（只做入队）。
    fn watch(
        &self,
        path: &Path,
        callback: Box<dyn Fn(FsEvent) + Send + Sync>,
    ) -> anyhow::Result<WatchHandle>;
}

/// 平台映射（notify 内部完成，mirri-os 仅透传）：
///   Linux   → inotify
///   macOS   → FSEvents
///   Windows → ReadDirectoryChangesW
```

### 4.3 Debouncing — 100ms 合并窗口

底层 watcher 会为一次保存产生多个事件（Create + Modify + Modify + Close），且三端事件粒度不同：

| 平台 | 一次 `echo x >> f` 的原始事件 |
|------|------------------------------|
| Linux inotify | Modify, Modify（追加分片） |
| macOS FSEvents | 合并后单个 Modify（但延迟可达 ~1s 批量回调） |
| Windows ReadDirectoryChangesW | Modify 或 Rename 的多种排列 |

引擎侧统一 debounce 规则：

- **每路径 100ms 合并窗口**：窗口内同一路径的事件合并为最后状态（Modify 被 Modify 覆盖；Create 后被 Modify 仍报 Create；Delete 覆盖一切）
- 窗口结束时触发一次 `FsEvent`（tokio interval 驱动 flush）
- 监听目录的**子树事件统一报目录路径**（业务层按需递归定位），避免三端父子事件顺序差异泄漏

### 4.4 与 Config Watcher 的集成

```
config.json 变更
   │
   ▼
FsWatcher::watch(config_path, callback)   ← 引擎启动时注册
   │  debounce 100ms
   ▼
ConfigWatcher::handle(FsEvent)            ← 仅消费 Modify/Create/Delete
   │
   ▼
配置重载 + reload 通知（事件流 emit config.changed）
```

**`config.changed` 事件覆盖所有配置变更来源**：RPC 触发、文件监听器触发、程序化触发。事件 payload 包含变更的 section keys 列表。

`ConfigWatcher` 只依赖 `FsWatcher` trait：单测注入 `FakeWatcher` 手动触发事件，验证重载逻辑；集成测试才注册真实监听。

### 4.5 平台差异与限制

| 差异 | 引擎策略 |
|------|---------|
| inotify 按 inode 监听，文件被替换（rename 覆盖）后旧 watch 失效 | 监听**目录**而非单文件；`ConfigWatcher` 收到事件后校验路径 basename |
| FSEvents 事件批量延迟 | 引擎不依赖"实时"；debounce 窗口本身容忍延迟 |
| Windows 卷挂载/网络盘可能不产生事件 | 文档化：监听目录仅限本地文件系统 |
| 目录被删除后 watcher 自动失效 | `WatchHandle` 收到 Error 事件 → 通知业务层重新 watch（至多 3 次退避） |

### 4.6 测试要点

- debounce：同路径 5 个快速 Modify 合并为 1 个事件
- Create→Modify 合并后仍报 Create
- FakeWatcher 驱动 ConfigWatcher 重载
- 真实集成测试（三端 CI）：写文件 → 100ms 内收到 Modify

## 5. PtyProvider Trait — PTY 抽象

### 5.1 动机

terminal 类 tool 与 `mirri-core::terminal_pool`（长驻 shell 会话的创建、输入写入、输出捕获、resize 同步）需要伪终端（pseudo-terminal）能力。三端底层机制完全不同：Unix 为 `openpty()` + `fork()`（或 `tokio::process::Command` 配合 pty 从端），Windows 为 ConPTY（`CreatePseudoConsole` API，自 Windows 10 1809 起可用）。引擎侧统一为 `PtyProvider` trait，`mirri-core::terminal_pool` 只依赖该 trait，业务层不再直接触碰任何 PTY 系统调用。

`mirri-os` 现有 `pty` 模块已具备底层能力，本节将其收敛为 `PtyProvider` trait 暴露给业务层。

### 5.2 Trait 定义

```rust
// mirri-os/src/pty.rs
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitStatus;

/// 不透明句柄：持有 PTY 的全部资源（主/从端 fd、子进程句柄、读取线程等），
/// Drop 时统一回收。
pub struct PtyHandle { /* opaque */ }

pub trait PtyProvider: Send + Sync {
    /// 在 cwd 下创建 PTY 会话，初始尺寸 cols × rows，注入附加环境 env。
    fn create(
        &self,
        cwd: &Path,
        cols: u32,
        rows: u32,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<PtyHandle>;

    /// 向 PTY 主端写入数据（等价于子进程 stdin）。
    fn write(&self, handle: &PtyHandle, data: &[u8]) -> anyhow::Result<()>;

    /// 读取子进程输出。v1 为同步阻塞读；后续可扩展为 async stream。
    fn read(&self, handle: &PtyHandle) -> anyhow::Result<Vec<u8>>;

    /// 调整终端尺寸，子进程收到窗口变化通知（Unix: SIGWINCH）。
    fn resize(&self, handle: &PtyHandle, cols: u32, rows: u32) -> anyhow::Result<()>;

    /// 终止 PTY 对应的子进程（kill 语义见 §7.4）。
    fn kill(&self, handle: &PtyHandle) -> anyhow::Result<()>;

    /// 等待子进程退出并返回退出码。
    fn wait(&self, handle: &PtyHandle) -> anyhow::Result<ExitStatus>;
}
```

### 5.3 平台实现

| 平台 | 实现方式 |
|------|---------|
| Unix（Linux/macOS） | `openpty()` + `fork()`；或 `tokio::process::Command` 配合 stdin/stdout/stderr 重定向到 pty 从端 |
| Windows | ConPTY：`CreatePseudoConsole`（Windows 10 1809+），子进程输出经 VT 序列管道读取 |

### 5.4 使用方：terminal_pool

`mirri-core::terminal_pool` 是 PTY 的主要消费方：长驻 shell 会话的创建、输入写入、输出捕获、resize 同步、会话终止全部经 `PtyProvider` 完成；测试注入 fake provider 即可验证会话逻辑，不触碰真实终端。默认实现由 `mirri_os::default_pty_provider()` 提供（§11.3，规划）。

### 5.5 测试要点

- 三端：`create` 后子进程可见 `cwd` 与注入的 `env`
- 写入 `echo hello\r\n` 后 `read` 能读到回显与输出
- `resize` 后子进程 `stty size` 反映新尺寸（Unix）
- `kill` 后 `wait` 返回非零/信号退出码（§7.4 契约）
- Windows 10 1809 以下版本：`create` 返回明确错误（ConPTY 不可用），不静默降级

## 6. IpcTransport Trait — IPC 传输抽象

### 6.1 动机

引擎的多进程场景（Web server ↔ 独立 TUI/helper 进程、`mirri-test-harness` 跨进程一致性测试）需要进程间字节流通道。Unix 端标准做法是 Unix domain socket，Windows 端是 named pipe。

### 6.2 Trait 定义

```rust
// mirri-os/src/ipc.rs
use tokio::io::{AsyncRead, AsyncWrite};

pub trait IpcTransport: Send + Sync {
    /// 在给定地址创建监听端。Unix: socket 文件；Windows: named pipe。
    /// 地址已存在时策略：Unix 先 unlink 陈旧 socket 文件再 bind（见 13.5）。
    fn bind(&self, address: &str) -> anyhow::Result<Box<dyn IpcListener>>;

    /// 连接已监听地址，返回字节流。
    fn connect(&self, address: &str) -> anyhow::Result<Box<dyn IpcStream>>;
}

pub trait IpcListener: Send {
    /// 接受一个连接。
    fn accept(&mut self) -> anyhow::Result<Box<dyn IpcStream>>;
}

pub trait IpcStream: AsyncRead + AsyncWrite + Send + Unpin {}
```

### 6.3 平台实现

| 平台 | 类型 | 地址形态 |
|------|------|---------|
| Unix（Linux/macOS） | `tokio::net::UnixListener` / `UnixStream` | `/tmp/mirri-<name>.sock`（`$XDG_RUNTIME_DIR` 存在时优先） |
| Windows | `tokio::net::windows::named_pipe::NamedPipeServer` / `NamedPipeClient` | `\\.\pipe\mirri-<name>` |

自动检测方式——**`#[cfg]` 只出现在 `mirri-os` 内部**（§11）：

```rust
// mirri-os/src/ipc.rs（内部）
#[cfg(unix)]
type PlatformListener = tokio::net::UnixListener;
#[cfg(windows)]
type PlatformListener = tokio::net::windows::named_pipe::NamedPipeServer;
```

### 6.4 地址解析（`ipc_path` 配置）

地址由引擎配置 `ipc_path` 控制，缺省值按平台推导：

```
默认值推导规则（resolve_ipc_address(name)）：
  1. 配置显式指定 ipc_path → 直接使用（Windows 上补全 \\.\pipe\ 前缀）
  2. Unix：
     a. $XDG_RUNTIME_DIR 存在 → $XDG_RUNTIME_DIR/mirri-<name>.sock
     b. 否则 → std::env::temp_dir()/mirri-<name>.sock
  3. Windows → \\.\pipe\mirri-<name>   （named pipe 不走文件系统，无 temp dir 概念）
```

约束（必须在校验层防御，否则运行期报错）：

- Unix socket 路径长度 ≤ 约 108 字节（`sun_path` 平台上限）
- Windows pipe 名 ≤ 256 字符，且不允许 `/`、`\`、`.` 之外的字符出现在 `<name>` 中

### 6.5 安全

- **Unix**：socket 文件创建后 `chmod 0700`（仅属主可连）
- **Windows**：`ServerOptions::new().first_pipe_instance(true)` 创建独占首个实例，防止其他用户抢先绑定同名 pipe 的 DACL 劫持；完整 pipe ACL 超出 v1 范围（已知差异 §13.5）
- 两端地址名白名单：`[a-zA-Z0-9_-]`，拒绝路径穿越字符

### 6.6 测试要点

- 三端 `bind` → `connect` → 双向 echo 往返
- 陈旧 socket 文件（模拟崩溃残留）存在时 bind 成功且旧文件被替换
- 地址名注入（`../`、空串、超长）被拒绝
- 断连后 accept 返回错误而非 panic

## 7. Process Spawning — 进程抽象

### 7.1 现状

`mirri-os/src/process.rs` 已提供 `OsProcess` trait、`ExecOptions`、`ChildProcess`、`LocalProcess`（基于 `tokio::process`），并已实现**分级的终止语义**：

```
Current kill() 契约（mirri-os 内已有实现）：
  Unix:    killpg(pgid, SIGTERM) → 5s 宽限 → 仍未退出则 killpg(pgid, SIGKILL)
  Windows: child.kill()（= TerminateProcess，硬终止，无宽限）
```

本文档补充**线格式规范 `ProcessSpec`** 与 shell tool 的完整调用链，不重造现有轮子。

### 7.2 ProcessSpec（可序列化的进程规范）

```rust
// mirri-os/src/process.rs（新增）
use serde::{Deserialize, Serialize};

/// 线格式进程规范：可跨进程传递（MCP/反向 RPC 参数）、可持久化（replay）、
/// 可序列化为日志。与既有 ExecOptions 的关系见 7.3。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,   // 附加环境（合并进父进程环境）
    pub cwd: Option<PathBuf>,
    pub stdin: StdioMode,
    pub stdout: StdioMode,
    pub stderr: StdioMode,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StdioMode { Null, Inherit, Pipe }
```

### 7.3 与现有 API 的映射

```rust
impl TryFrom<ProcessSpec> for ExecOptions { /* cwd/env/timeout_ms 直映 */ }
```

Stdio 三态映射：`Null → Stdio::null()`、`Inherit → Stdio::inherit()`、`Pipe → Stdio::piped()`（piped 时 `ChildProcess` 暴露对应流）。`std::process::Command` 本身跨平台，无需抽象——差异只在**信号与终止语义**，已由 `ChildProcess::kill` 收敛。

### 7.4 信号处理契约（跨平台统一语义）

| 操作 | Unix | Windows | 引擎侧统一语义 |
|------|------|---------|---------------|
| 优雅终止 | `killpg(SIGTERM)` | 不支持信号；`TerminateProcess` 等价于硬杀 | `ChildProcess::kill()` — "请求终止，可能不优雅" |
| 强制终止 | `killpg(SIGKILL)` | `TerminateProcess`（唯一手段） | `ChildProcess::kill()` 内部 5s 升级，Windows 无条件硬杀 |
| 进程树 | 进程组（pgid）天然成组 | 无组概念；`CreateJobObject` 可绑定作业对象（未来项） | 契约：kill 语义为"终止该进程及其后代"；Windows v1 仅杀直接子进程，登记已知差异 §13.3 |
| 退出码 | 被信号杀死 → `-1`（现有实现） | 可正常返回 exit code | 引擎统一：信号终止视为"异常退出"，`wait()` 返回 `-1` |

### 7.5 Shell tool 完整调用链

```
用户输入: "ls -la | grep src"
   │
   ▼
ShellProvider::resolve_command(input)
   │  → CommandSpec { program: "/bin/bash", args: ["-c", "ls -la | grep src"] }
   ▼
ProcessSpec { program, args, env, cwd: workspace, stdin/stdout/stderr: Pipe, timeout_ms }
   │
   ▼
OsProcess::exec(spec) → ChildProcess
   │
   ▼
stdout/stderr 流式转发为 tool.output 事件；kill 语义见 §7.4
```

### 7.6 测试要点

- kill 契约：派生 `sleep 60` 子进程，kill 后在 5s 内退出（Unix）；Windows 立即退出
- 退出码：正常退出 / 信号退出（`-1`）断言
- ProcessSpec ↔ ExecOptions 往返映射
- shell 注入：`resolve_command` 输出中 program 恒为检测到的 shell，不信任输入中任何可执行字段

## 8. Line Endings — 行尾规范化

### 8.1 动机

Unix 惯例 LF（`\n`），Windows 惯例 CRLF（`\r\n`）。若不处理，同一份文本在三端读写会产生字节差异：session 文件跨平台迁移后 diff 爆炸、写入配置在 Windows 上出现混合行尾。

**决策：wire 格式（协议、持久化、消息体）恒为 LF**；文件 I/O 按"检测—规范化—保留"策略处理（见 8.4）。

### 8.2 类型与检测

```rust
// mirri-os/src/line_ending.rs

/// 检测结果。Mixed 表示文本中同时存在多种行尾（迁移/拼接产物）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding { Lf, Crlf, Cr, Mixed }

impl LineEnding {
    /// 扫描字节流判定行尾。
    /// 统计 \r\n 对数、独立 \n 数、独立 \r 数：
    ///   仅 \r\n → Crlf；仅 \n → Lf；仅 \r（老 Mac）→ Cr；
    ///   混有 ≥2 种 → Mixed；完全没有行尾 → 按 Lf 处理（中性）。
    pub fn detect(content: &[u8]) -> LineEnding {
        // 按 \r 与 \n 的邻接关系计数，O(n) 单遍扫描
    }

    /// 归一化为 LF：\r\n → \n，\r → \n。
    pub fn normalize(content: &str) -> String;

    /// 从 &str 无损转换到目标行尾（写文件用）。
    pub fn to_bytes(&self, content: &str, target: LineEnding) -> Vec<u8>;
}
```

**二进制保护**：前 8KB 内含 `\0` 即判定为二进制，跳过检测与归一化（全程原样透传）。

### 8.3 读写策略

| 场景 | 策略 |
|------|------|
| 引擎自有产物（session `.jsonl`、events） | **写 LF**（wire 恒 LF，跨机器 diff 稳定） |
| 用户可见的模板配置（`.mirri/config.*`） | 新建时写 **OS 原生行尾**（Windows CRLF / Unix LF） |
| 读取用户文件（`fs.read` / tool 读文件） | 内存中归一化为 LF 后返回给上层（内容不变，只统一行尾） |
| 覆盖写用户文件 | 若文件已存在：detect 原行尾，**保留原行尾写回**；新建文件：OS 原生 |

`to_bytes` 保证"原文内容 + 目标行尾"是唯一变换，绝不引入其他改动。

### 8.4 git 类比与配置

- 与 `core.autocrlf` 同一问题域，但引擎**不依赖 git 配置**，行为自管
- 预留配置项 `line_ending.wire = "lf"`（不可变常量，v1 不做覆盖）

### 8.5 测试要点

- `detect` 四态 + 空内容 + 纯 `\r`（老 Mac）
- `normalize` 混合行尾归一再转回保留（roundtrip）
- 二进制文件（PNG 头）不被触碰
- 三端集成：Windows 上写配置断言 CRLF，session 断言 LF

## 9. File Permissions — 文件权限抽象

### 9.1 动机

配置文件与 token 文件包含敏感信息（API key），必须限制访问。Unix 用 rwx 位（`chmod` / `chown`），Windows 用 ACL（`icacls`）——两者语义完全不同且不可移植。

**决策：v1 只抽象"最小必要语义"**：私密文件 = 属主可读写 + 他人不可访问。

### 9.2 类型定义

```rust
// mirri-os/src/permission.rs
use std::path::Path;

/// 平台无关的权限意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Unix 风格 rwx 位（0o000..0o777）
    Mode(u32),
    /// Windows 简化模型：只读属性（防止误改，非安全边界）
    ReadOnly(bool),
}

impl Permission {
    /// 应用到文件。Unix 用 PermissionsExt::from_mode；
    /// Windows 设置/清除 FILE_ATTRIBUTE_READONLY。
    pub fn apply(&self, path: &Path) -> std::io::Result<()>;

    /// 私密文件的标准权限：Unix 0o600 / Windows ReadOnly(true)。
    /// 用于 config 与 token 文件的落盘。
    pub fn private_file() -> Permission;

    /// 私密目录：Unix 0o700 / Windows 无操作（目录只读属性无隔离意义）。
    pub fn private_dir() -> Permission;
}
```

### 9.3 平台映射

| 意图 | Unix（`Mode`） | Windows（`ReadOnly`） |
|------|---------------|----------------------|
| 私密文件（config / token） | `0o600`（属主 rw） | `ReadOnly(true)` + **文档声明：仅防误改，非安全边界** |
| 私密目录（`~/.mirri`） | `0o700` | 无操作（Windows 目录隔离需 ACL，超出 v1） |
| 可执行脚本 | `0o755` | 无操作（可执行性由扩展名/关联决定） |

**重要限制（登记 §13.4）**：Windows 的 read-only attribute 是合作性标记，不阻止特权/同属主进程读写；真正的隔离需要 ACL（`FileSystemRights`，v1 列为 future 项，通过 `windows-sys` 实现）。

### 9.4 落盘顺序

写敏感文件必须 **"先写后收紧"**：

```
1. 以默认 umask 写入临时文件（write-temp）
2. Permission::private_file().apply(temp)   ← 收紧权限
3. 原子 rename 到目标路径
```

顺序不可颠倒：若先 rename 再 chmod，存在窗口期文件以宽松权限存在。此顺序同时规避部分文件锁问题（§13.4）。

### 9.5 测试要点

- Unix：`0o600` 落盘后 `mode & 0o077 == 0`；`private_dir` 后他人不可列目录
- Windows：read-only 属性读取/清除往返
- 权限收紧失败（如只读文件系统）→ 返回明确错误，不静默降级
- rename 与 chmod 顺序断言（落盘-收紧-rename 时序）

## 10. 环境变量

### 10.1 动机

Unix 环境变量大小写敏感（`Path` 与 `PATH` 是两个变量），Windows 大小写不敏感（`CreateProcess` 的环境块按 case-insensitive 语义合并，`Path` 与 `PATH` 同时存在时行为未定义）。引擎的 env 透传、`$SHELL` 检测、PATH 解析都必须处理该差异。

### 10.2 EnvMap 包装

```rust
// mirri-os/src/env.rs
use std::collections::HashMap;

/// 大小写语义按平台归一的环境变量容器。
///
/// 内部存储策略：
///   Unix:    按原样 key 存储（case-sensitive）
///   Windows: key 统一小写存储，value 原样保留（case-insensitive，
///            且天然消除 "Path 与 PATH 并存" 的歧义）
///
/// 对外暴露的语义（trait 化后与平台无关）：
///   get/contains/remove 在 Windows 上大小写不敏感，Unix 上敏感。
pub struct EnvMap { map: HashMap<String, String>, case_insensitive: bool }

impl EnvMap {
    pub fn from_env() -> EnvMap;              // 快照父进程环境（进程启动时一次）
    pub fn get(&self, key: &str) -> Option<&str>;
    pub fn set(&mut self, key: &str, value: String);   // 覆盖语义：同 key 覆盖
    pub fn remove(&mut self, key: &str);
    pub fn contains_key(&self, key: &str) -> bool;
    pub fn merge(&mut self, other: &EnvMap);  // 注入层用（shell/env 附加项）
    pub fn to_hashmap(&self) -> HashMap<String, String>; // spawn 前导出
}
```

Windows 归一化规则（仅在 `mirri-os` 内实现）：

```rust
// 内部：key → 归一化 key
#[cfg(windows)]
fn normalize_key(key: &str) -> String { key.to_ascii_lowercase() }
#[cfg(unix)]
fn normalize_key(key: &str) -> String { key.to_string() }
```

### 10.3 Wire 格式约定

- **wire 上保留用户传来原始大小写**（协议层不做改写，避免"协议里 PATH，引擎里 path"的困惑）
- **服务器端（引擎侧）归一化**：`ProcessSpec.env` 在 spawn 前经 `EnvMap` 导入，Windows 上自动小写化 key——即归一化只发生在执行边界，传输层中立
- 同名异大小写冲突（Windows 上 `Path` + `PATH` 同时到达）→ 后者覆盖前者，并 emit 一次 warning（`tracing::warn`），这是可观测的确定性行为

### 10.4 测试要点

- Windows：`set("Path", ...)` 后 `get("PATH")` 命中；`Path`/`PATH` 双写入收敛为单 key
- Unix：`Path` 与 `PATH` 各自独立
- `from_env()` 快照与子进程可见环境一致
- wire 往返：传入原始大小写 → spawn 环境行为等价

## 11. 平台检测与条件编译

### 11.1 分级规则

> 原「`#[cfg]` 只允许出现在 mirri-os 内、业务 crate 一律禁止」的绝对铁律与
> embedded/rpc 层现实冲突（`mirri-embedded`、`mirri-proto` rpc 层存在 Windows/Unix 分支），收敛为**三级规则**：

1. **`mirri-core` 零 `#[cfg]`（trait-only）**：平台差异通过 trait 表达，core 不感知任何目标平台
2. **平台分支封装进 `mirri-os`**：`#[cfg]` / `cfg!()` 只允许在其内部出现；业务代码经 trait 获取平台能力
3. **其余业务 crate 尽量避免**：确需平台分支时，**收敛到单一模块（如 rpc 层）**并加注释说明理由
   （`// 平台分支：<原因>`），由 code review 把关；新平台差异应优先下沉到 `mirri-os` 再暴露 trait

允许清单（mirri-core 内）：

| 允许 | 说明 |
|------|------|
| `std::path::MAIN_SEPARATOR` 等常量 | 仅用于展示/格式化，不用于行为分支 |
| `std::env::consts::OS` | 仅用于 telemetry/诊断日志 |
| 现有跨平台 std API（`PathBuf`、`std::fs`） | 其内部 cfg 是 std 的事，业务层正常使用 |

禁止清单（mirri-core 内）：`#[cfg(unix)]`、`#[cfg(windows)]`、`#[cfg(target_os = "...")]`、`#[cfg(windows)]` 包裹的任意代码、`if cfg!(...)` 行为分支、`std::os::unix::*` / `std::os::windows::*` 直接引用（必须经 `mirri-os` 再导出）。其余业务 crate 命中 `#[cfg]` 时需满足第 3 级的收敛 + 注释要求。

**CI 强制**：静态检查 job（`scripts/check-no-cfg.sh`，§12.2）——`mirri-core` 命中 `grep -rn "#\[cfg" src/`（及 `cfg!(`）**即失败**；`mirri-os` 豁免；其余业务 crate 命中时**需带说明注释**（WARN + review 兜底）。

### 11.2 mirri-os 内部分层（规划布局）

> 以下为修订后规划布局：`path_codec` / `shell` / `fs_watcher` / `ipc` / `line_ending` / `permission` / `env` / `platform/*` 尚未落地（待实施）；现状仅 `fs` / `glob` / `process` / `pty` / `search` / `path_access`。

```
mirri-os/src/
├── lib.rs               # trait 定义 + 默认工厂函数 re-export
├── platform/
│   ├── mod.rs           # cfg 分发器（唯一允许密集 cfg 的文件）——待实施
│   ├── unix.rs          # cfg(unix) 分支实现
│   └── windows.rs       # cfg(windows) 分支实现
├── path_codec.rs        # trait（平台无关）——待实施
├── shell.rs             # trait + 平台无关逻辑——待实施
├── fs_watcher.rs        # trait + notify 薄封装——待实施
├── pty.rs               # trait + openpty/ConPTY 实现
├── ipc.rs               # trait + cfg 类型别名（§6.3）——待实施
├── process.rs           # 既有（OsProcess 等）
├── line_ending.rs       # 纯算法，无 cfg——待实施
├── permission.rs        # trait + cfg 实现——待实施
└── env.rs               # EnvMap + cfg 归一化——待实施
```

### 11.3 默认工厂函数（规划）

> 以下工厂函数为规划形态（待实施）。

```rust
// mirri-os/src/platform/mod.rs（待实施）

/// 按目标平台返回具体实现（Box<dyn>，编译期确定，无运行期开销）。
#[cfg(unix)]
pub fn default_path_codec() -> Box<dyn PathCodec> { Box::new(PosixPathCodec) }
#[cfg(windows)]
pub fn default_path_codec() -> Box<dyn PathCodec> { Box::new(WindowsPathCodec) }

#[cfg(unix)]
pub fn default_shell_provider() -> Box<dyn ShellProvider> { Box::new(BashProvider::new()) }
#[cfg(windows)]
pub fn default_shell_provider() -> Box<dyn ShellProvider> { Box::new(PowerShellProvider::new()) }

pub fn default_fs_watcher() -> Box<dyn FsWatcher> { Box::new(NotifyWatcher::new()) }   // notify 跨平台，无 cfg

#[cfg(unix)]
pub fn default_ipc_transport() -> Box<dyn IpcTransport> { Box::new(UnixSocketTransport) }
#[cfg(windows)]
pub fn default_ipc_transport() -> Box<dyn IpcTransport> { Box::new(NamedPipeTransport) }

#[cfg(unix)]
pub fn default_pty_provider() -> Box<dyn PtyProvider> { Box::new(UnixPtyProvider::new()) }
#[cfg(windows)]
pub fn default_pty_provider() -> Box<dyn PtyProvider> { Box::new(ConPtyProvider::new()) }
```

与 TS 引擎的对应关系：`default_*` 工厂等价于 TS 端 `platform.ts` 中按 `process.platform` 分发的工厂；**差异是编译期而非运行期**——错误平台组合在编译阶段即被排除。

### 11.4 依赖注入装配点

引擎装配（`EngineBuilder::build()` 或 server bootstrap）是**唯一**调用 `default_*` 的地方：

```rust
pub struct EngineConfig {
    pub path_codec: Box<dyn PathCodec>,
    pub shell_provider: Box<dyn ShellProvider>,
    pub fs_watcher: Box<dyn FsWatcher>,
    pub ipc_transport: Box<dyn IpcTransport>,
    pub pty_provider: Box<dyn PtyProvider>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            path_codec: mirri_os::default_path_codec(),
            shell_provider: mirri_os::default_shell_provider(),
            fs_watcher: mirri_os::default_fs_watcher(),
            ipc_transport: mirri_os::default_ipc_transport(),
            pty_provider: mirri_os::default_pty_provider(),
        }
    }
}
```

测试路径：`EngineConfig { path_codec: fake, ..Default::default() }` 即完成平台替换，无需任何 cfg。

### 11.5 测试要点

- 编译期验证：业务 crate 中故意插入 `#[cfg(windows)]` 会被 CI 静态检查拦截
- 单测全部基于 fake 实现（不触碰真实 OS 行为差异）
- 平台真机行为由集成测试覆盖（§12 三端 CI 矩阵）

## 12. CI 矩阵

### 12.1 平台 × 架构矩阵

| 平台 | 架构 | GitHub runner | Rust target triple | NAPI 产物 |
|------|------|---------------|--------------------|-----------|
| Linux | x86_64 | `ubuntu-latest` | `x86_64-unknown-linux-gnu` | `mirri.linux-x64-gnu.node` |
| Linux | aarch64 | `ubuntu-24.04-arm`（原生 arm runner） | `aarch64-unknown-linux-gnu` | `mirri.linux-arm64-gnu.node` |
| macOS | x86_64 | `macos-13`（Intel） | `x86_64-apple-darwin` | `mirri.darwin-x64.node` |
| macOS | aarch64 | `macos-14`（Apple Silicon） | `aarch64-apple-darwin` | `mirri.darwin-arm64.node` |
| Windows | x86_64 | `windows-latest` | `x86_64-pc-windows-msvc` | `mirri.win32-x64-msvc.node` |

说明：

- **仅支持 `windows-msvc`** 工具链；`windows-gnu` 不作为发布目标
- Linux aarch64 使用原生 arm runner，不引入 cross/zig 交叉编译链（保持 CI 简单可复现）
- macOS x86_64 用 `macos-13`，aarch64 用 `macos-14`（GitHub 原生镜像对应关系）
- Rust 工具链统一由仓库根 `rust-toolchain.toml` 固定（stable + 组件）

### 12.2 每平台任务（matrix job）

```yaml
# .github/workflows/cross-platform.yml（示意）
name: cross-platform

on:
  push: { branches: [main] }
  pull_request:

jobs:
  test:
    strategy:
      fail-fast: false          # 单平台失败不取消其他平台
      matrix:
        include:
          - { os: ubuntu-latest,        target: x86_64-unknown-linux-gnu }
          - { os: ubuntu-24.04-arm,     target: aarch64-unknown-linux-gnu }
          - { os: macos-13,             target: x86_64-apple-darwin }
          - { os: macos-14,             target: aarch64-apple-darwin }
          - { os: windows-latest,       target: x86_64-pc-windows-msvc }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2          # 按 target 分缓存
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace --all-targets
      - run: cargo build --release --target ${{ matrix.target }}
      # 平台静态检查：分级规则（§11.1）——mirri-core 零 cfg，mirri-os 豁免，其余命中需带注释
      - run: ./scripts/check-no-cfg.sh
```

`scripts/check-no-cfg.sh` 核心逻辑（按 §11.1 分级规则）：

```bash
#!/usr/bin/env bash
# 分级检查：mirri-core 硬禁 #[cfg]；mirri-os 豁免；其余业务 crate 命中须携带说明注释
set -euo pipefail
for crate in rust/crates/*/; do
  name=$(basename "$crate")
  [ "$name" = "mirri-os" ] && continue
  hits=$(grep -rEn '#\[cfg|cfg!\(' "$crate/src" 2>/dev/null || true)
  [ -z "$hits" ] && continue
  if [ "$name" = "mirri-core" ]; then
    echo "ERROR: mirri-core must not use #[cfg] / cfg!()" >&2
    exit 1
  fi
  # 其余 crate：命中处必须带说明注释（// 平台分支：<原因>），由 code review 兜底（§11.1）
  echo "WARN: $name contains #[cfg] — each site must carry an explanatory comment (§11.1)"
done
```

### 12.3 NAPI binding 构建

NAPI binding（TUI 接入）在每个 target 上独立构建并发布：

```bash
# 每个 matrix target 内：node 依赖 + napi build
corepack enable
yarn install
napi build --release --target ${{ matrix.target }}   # @napi-rs/cli
# 产物按 12.1 命名表上传 artifacts / 发布 npm optionalDependencies
```

要点：

- `@napi-rs/cli` 的 `--target` 与 12.1 的 target triple 一一对应
- 产物走 npm `optionalDependencies` 平台分包（`@mirri/engine-<platform>-<arch>`），安装期由 Node 按 `process.platform` 选择
- 三端 NAPI 产物必须**同一 commit 齐套发布**（版本号一致），否则 TUI 跨平台升级错位

### 12.4 ACP binary 构建

```bash
cargo build --profile dist --target ${{ matrix.target }} -p mirri-cli
# 产物: target/<triple>/dist/mirri(.exe)
```

ACP host 随 `mirri` 二进制内的 `mirri acp` 子命令分发，不产出独立的
`mirri-acp` 发布产物；crate 内保留的薄 `mirri-acp` bin 仅供 conformance
测试 spawn。

### 12.5 分层测试策略

| 层 | 内容 | 跑在哪 |
|----|------|--------|
| 单元测试 | 纯算法（line_ending、path_codec 词法层、EnvMap） | 所有平台（matrix） |
| 平台集成测试 | 真实 watch / socket / kill / 权限落盘 | 所有平台（matrix），断言见各章"测试要点" |
| 平台专属断言 | `mirri-os` 内部按 cfg 编写的差异断言（如 Unix mode 位） | 对应平台 |
| 跨平台一致性 | `mirri-test-harness` 同一场景三端跑，比对事件序列与输出 | 三端结果上传后合并比对 |

## 13. 已知平台差异

以下差异**无法消除**，均为"文档化 + 引擎策略收敛"，属 P5 原则的登记清单。业务代码不感知这些差异（已被 trait 吸收），本表服务于运维排障与评审。

### 13.1 文件路径大小写敏感性

| 平台 | 行为 | 引擎策略 |
|------|------|---------|
| Linux | case-sensitive（`Foo` ≠ `foo`） | `WorkspaceBoundary` 严格比较 |
| macOS | 默认 APFS case-insensitive（可格式化为 sensitive） | 边界校验按 insensitive 比较；文档注明"用户可自选大小写敏感卷" |
| Windows | NTFS case-insensitive（保留大小写） | 校验/dedup 统一小写比较；wire 上保留原大小写 |

影响面：路径去重、workspace 边界校验、symlink 目标比较。**决策：比较语义跟随文件系统，而非固定一端**——由 `WorkspaceBoundary::case_insensitive` 在 mirri-os 内按平台初始化（§2.5）。

### 13.2 Symlink 行为

| 平台 | 差异 | 引擎策略 |
|------|------|---------|
| Unix | 创建 symlink 无需特权 | 正常创建，读取侧有完整校验（§2.6） |
| Windows | 创建 symlink 需开发者模式或管理员权限；目录可用 junction（无需特权） | 目录链接优先尝试 symlink，失败回退 junction；两者在读取侧同样递归校验；创建失败返回明确错误，不静默降级 |

### 13.3 进程信号投递

| 平台 | 差异 | 引擎策略 |
|------|------|---------|
| Unix | SIGTERM（可捕获、可优雅退出）→ 5s → SIGKILL；进程组整体投递 | `ChildProcess::kill()` 现有契约（§7.4） |
| Windows | 无信号概念；`TerminateProcess` 为硬杀，目标进程无清理机会；无进程组 | v1：直接硬杀；优雅关闭依赖引擎自身协议（IPC 消息）；Job Object 进程树终止列为 future（§7.4） |

暴露给业务层的统一语义："kill 可能不优雅"——依赖优雅退出的功能（如保存 session）必须走引擎级关闭协议，而非进程信号。

### 13.4 文件锁

| 平台 | 机制 | 引擎策略 |
|------|------|---------|
| Unix | `fcntl` POSIX advisory lock（按进程，进程退出自动释放；同进程内锁与 fd 相关） | 不依赖文件锁作为唯一防线：写路径全部 **write-temp + atomic rename**（§9.4），锁仅用于跨进程互斥的显式场景 |
| Windows | `LockFileEx`（按句柄，可选强制语义；进程崩溃由 OS 释放） | 同上；显式锁场景统一收敛为 mirri-os 的 `FileLock`（封装 fcntl/LockFileEx），业务层只见到"互斥或失败"两种结果 |

### 13.5 Socket / Pipe 清理

| 平台 | 差异 | 引擎策略 |
|------|------|---------|
| Unix | socket 是文件系统实体：进程崩溃后残留**陈旧文件**，阻塞重新 bind | `bind` 前探测并 unlink 陈旧文件（配合 pid 存活性检查：陈旧 socket 的属主进程存活则放弃并报错）；Drop 时主动 unlink |
| Windows | named pipe 无文件系统实体，进程退出后自动清理 | 无清理动作；安全由 `first_pipe_instance(true)` 独占保证（§6.5） |

### 13.6 其他登记项

| 差异 | 说明 | 引擎策略 |
|------|------|---------|
| Windows 保留文件名（CON/NUL/COM1） | 无法在文件系统创建 | 词法校验层拒绝并给出明确错误 |
| 可执行位 | Windows 无 exec bit | 可执行性判定仅发生在 Unix（`resolve_on_path` 检查 exec bit）；Windows 依赖 PATHEXT |
| 目录只读属性 | Windows 无隔离意义 | `Permission::private_dir` 在 Windows 为 no-op（§9.3） |
| 环境变量大小写 | Windows case-insensitive | EnvMap 归一化（§10） |
| 默认 shell 生态 | bash/zsh vs PowerShell | ShellProvider（§3），命令语义差异随 shell 固化 |

## 相关文档

| 文档 | 关联 |
|------|------|
| [00-overview.md](00-overview.md) | OS 抽象层在整体架构中的位置（§2 架构图） |
| [05-crate-structure.md](05-crate-structure.md) | `mirri-os` 在 crate 依赖图中的职责边界 |
| [01-protocol-unification.md](01-protocol-unification.md) | wire 格式（路径/文本）在协议层的使用 |
| [plans/02-phase2-web-migration.md](plans/02-phase2-web-migration.md) | Web server 的 IPC / 进程使用场景 |

## ADR

### ADR-001（2026-09-16）：发布构建 profile 与本地构建 profile 拆分

**背景与动机**：Rust workspace 原先将全程序优化参数（thin LTO、codegen-units=1）直接放在 `cargo` 的 `release` profile 上，导致开发者日常 `cargo build --release` 的尾部（大 crate 的单 codegen 单元 + 全程序 ThinLTO 链接）长时间串行执行，本地迭代体验差，而这些优化参数只对发布产物有意义。

**决策**：cargo profile 按用途拆分——`release` 仅保留日常本地构建所需的轻量参数（strip symbols）；新增 `dist` profile（继承 release）承载发布级参数（thin LTO、codegen-units=1、panic=abort），发布打包流水线（`rust/scripts/release-package.mjs` 与其 CI 调用方）统一改为 `--profile dist` 构建，产物位于 `target/dist/`。

**被否决的替代方案**：
- 维持单一 release profile（本地构建尾部持续慢，发布与日常耦合）；
- 日常迭代全部使用 debug profile（本地引擎二进制性能差距过大，无法真实验证运行时行为）。

**影响范围**：§12.4 的发布构建命令与产物路径；`rust/Cargo.toml` profile 定义、`rust/scripts/release-package.mjs`、CI `rust-native-build.yml` 的构建参数与产物路径。
