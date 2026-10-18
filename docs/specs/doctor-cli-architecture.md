# Doctor CLI（Rust 内核版）技术设计

> **文档类型：设计/Spec 类**（按 `docs/specs/AGENTS.md` 治理约定）——只写终态，不写执行状态。 ·
> 需求：[doctor-cli-requirements.md](doctor-cli-requirements.md) · 现状证据：[doctor-cli-context.md](doctor-cli-context.md) ·
> 设计变更以 ADR 追加记录，历史不重写。

## 1. 总体结构

```
mirri-cli（宿主 crate，允许直接 IO）
└── Commands::Doctor { target, path }          # clap 定义（lib.rs）
    └── doctor::run_doctor(target, path)        # 命令实现（src/doctor.rs，新文件）
        ├── locate_config_path()                # MIRRICODE_HOME 解析（复用 commands.rs 的 mirri_data_dir）
        └── mirri_core::config::doctor::validate_config_str(&text, &path)
            # 纯校验核心，零 IO，返回 CheckReport
```

分层原则：

- **校验核心放 `mirri-core`**（`src/config/doctor.rs` 新模块）：纯函数 `&str → CheckReport`，零 IO、不读环境、不展开 `${VAR}`（对应需求 US-1.6/约束 3），单元测试高密度覆盖。mirri-core 的 zero-IO 守卫天然满足（纯字符串输入）。
- **IO 与格式化放 `mirri-cli`**（`src/doctor.rs` 新文件）：定位文件、读文本、渲染报告、退出码。宿主 crate 允许 `std::fs`（`config-show` 先例）。

## 2. 数据模型（mirri-core，新公开类型）

```rust
// rust/crates/mirri-core/src/config/doctor.rs

/// 单个检查项的三态之一 + 可选消息。
pub enum CheckStatus {
    Ok,
    Skip,   // 默认路径不存在（由 CLI 层产生；核心层不产生 Skip）
    Error,  // 硬错误：退出码 1
    Warn,   // 软警告：不改退出码
}

/// 一条问题定位：TOML 路径（段.条[索引].键）+ 人读消息。
pub struct CheckIssue {
    pub status: CheckStatus,   // Error | Warn（核心层只产这两种）
    pub path: Vec<String>,     // 如 ["hooks", "[0]", "pattern"]、["providers", "myprov", "bse_url"]
    pub message: String,
}

/// 一次 config.toml 体检的完整报告。
pub struct CheckReport {
    pub file_path: String,     // 展示用
    pub issues: Vec<CheckIssue>, // 空且语法/结构校验通过 = 健康
    pub parse_ok: bool,        // TOML 语法与结构化解析是否通过（false 时 issues 里必有 Error）
}

/// 入口：校验一段 config.toml 文本。零 IO、零环境读取。
pub fn validate_config_str(text: &str, file_path: &str) -> CheckReport;
```

设计要点：

- `path` 用 `Vec<String>` 而非格式化字符串——CLI 层负责渲染（`hooks[0].pattern` 风格，对齐 TS doctor 的 `formatIssuePath`）。
- **阶段管线**（顺序短路）：
  1. **语法解析**：`toml::from_str::<toml::Value>` 失败 → 单条 Error（含 toml 错误的行/列信息，US-1.3），返回。
  2. **结构化加载**：`ServerConfig::from_toml` 失败（serde 类型错误 / 互斥键 / task 边界硬校验）→ 单条 Error（消息含 serde 原文），返回。注意：**调用 `from_toml` 而非 `load`**——不读文件、不展开 env。
  3. **未知键检测**（US-3）：对 raw `toml::Value` 走键清单比对（见 §3），逐条 Error。
  4. **软诊断**（US-4）：调用既有 `ServerConfig::diagnostics_warnings()`，逐条 Warn。
  5. **未知实验开关**（US-6）：`[experimental]` 键比对 flag 注册表，逐条 Warn。

## 3. 未知键检测的键清单（单一事实源）

需求 US-3.2 要求与写回逻辑共用清单，现状三个常量已是 `pub(crate)`（`config/wire.rs`）：

- `KNOWN_CONFIG_TOP_LEVEL_KEYS`（28 个顶层键）
- `KNOWN_PROVIDER_ENTRY_KEYS` / `KNOWN_MODEL_ENTRY_KEYS`（`[providers.*]` / `[models.*]` 条目内键）

doctor 模块同 crate 内直接引用，**不复制**。条目级键清单需小幅补全（这是唯一新增清单内容，补进同一文件，写回侧自动受益）：

```rust
// config/wire.rs 追加（与现有清单同风格）
pub(crate) const KNOWN_HOOK_ENTRY_KEYS: &[&str] = &[
    "event", "command", "pattern", "matcher",
    "timeout_ms", "timeout", "fail_closed", "failClosed",
    "env", "cwd",
];
```

段级嵌套结构已知键覆盖矩阵（顶层段 → 内部键来源）：

| 顶层段 | 内部键校验来源 |
|---|---|
| `hooks`（数组） | `KNOWN_HOOK_ENTRY_KEYS`（新） |
| `providers` / `models`（map） | 既有两个 ENTRY_KEYS |
| `permission`（含 `rules` 数组） | 新 `KNOWN_PERMISSION_RULE_KEYS`（`decision`/`scope`/`pattern`/`reason` — `PermissionRule` 实际字段） |
| `thinking` / `loop_control` / `task` / `subagent` / `model_catalog` | 新 `KNOWN_SECTION_KEYS: &[(&str, &[&str])]` 静态表，逐段列出 serde 字段名（含 camelCase 别名） |
| `image` / `background` / `services` | 自由透传段（`toml::Value`），不查内部键 |
| 其余标量顶层键 | 无内部结构 |

**规则**：一个键在清单内（任一拼写）→ 合法；不在任何清单 → `Unrecognized key "<k>" in <段>` Error。**宽松方向不对称**：不存在的段（未知顶层键）直接 Error；存在段的未知内部键逐条 Error。

## 4. 实验开关检测（US-6）

- 事实源：`mirri_core::FlagRegistry`。校验核心不能依赖组合根注册（注册表内容随宿主装配而变），因此 `validate_config_str` 增加一个参数：

```rust
pub fn validate_config_str(
    text: &str,
    file_path: &str,
    known_flags: &[String],   // CLI 层从 new_engine_flag_registry 构造的注册表快照
) -> CheckReport;
```

- CLI 侧构造：复用 `mirri-embedded` 的 `new_engine_flag_registry`（需将其从 `embedded_server/mod.rs` 的私有 fn 提为 `pub` 或提供快照 helper）拿注册表，读其已注册 flag id 列表传入。
- `[experimental]` 是 `HashMap<String, bool>`；遍历键，不在 `known_flags` → Warn：`experimental flag "<name>" is not registered in this engine and has no effect`。
- 纯函数不读进程 env——env 开关（`MIRRICODE_EXPERIMENTAL_*`）与体检无关（体检的是文件）。

## 5. CLI 层（mirri-cli）

### 5.1 clap 定义（`lib.rs` 的 `Commands`）

```rust
/// Validate config.toml against the rust engine's config schema
Doctor {
    /// Validate this file as config.toml instead of the default path
    #[arg(long)]
    config: Option<String>,
},
```

设计决策（ADR-D1）：**不用 `doctor config <path>` 子命令结构**（TS doctor 形态），而用 `--config <path>` 可选参数。理由：(a) TS doctor 的子命令结构是为将来挂 `doctor tui` 服务的，Rust 版明确不做 tui（非目标 1），单文件目标无需子命令层；(b) 留出 `--json`（SDR 追加时的机器输出）等平面扩展位。

### 5.2 行为矩阵（`src/doctor.rs`）

| 输入 | 状态 | 输出流 | 退出码 |
|---|---|---|---|
| 默认路径，文件不存在 | SKIP | stdout | 0 |
| 显式路径，文件不存在 | ERROR | stderr | 1 |
| 语法错误 | ERROR（1 条，含行/列） | stderr | 1 |
| 结构化失败（serde/硬校验） | ERROR（1 条） | stderr | 1 |
| 未知键（任意层级） | ERROR（逐条） | stderr | 1 |
| 软诊断命中 | WARN（逐条） | stdout | 0 |
| 全通过 | OK | stdout | 0 |

### 5.3 输出格式（对齐 TS doctor 的排版习惯，英文）

```
Mirri doctor

SKIP config.toml  /Users/x/.mirri-code/config.toml
     File does not exist; built-in defaults will apply.

Mirri doctor found 2 issues.

ERROR config.toml  /Users/x/.mirri-code/config.toml
  Invalid configuration in /Users/x/.mirri-code/config.toml.
  Validation issues:
    hooks[0].timeout_ms: unknown key — expected one of ...   ← 未知键
    providers.myprov.bse_url: unknown key in [providers.myprov]
WARN  providers.<default_provider>: default_provider "x" does not match ...  ← 软诊断
```

- 成功：`Mirri doctor` + 状态行 + `All checked config files are valid.`
- `path` 渲染：`Vec<String>` join，数字段渲染为 `[n]`，其余以 `.` 连接（对齐 TS `formatIssuePath` 语义）。

### 5.4 IO 边界

- `mirri_data_dir()`（`commands.rs` 既有）→ 拼默认路径；`std::fs::read_to_string` 读文件（宿主 crate 合法）。
- 相对路径按 cwd 解析（`std::path::absolute` 或 `PathBuf::join` 语义与 TS `resolveInputPath` 对齐：绝对路径直用，相对路径接 cwd）。

## 6. 与既有面的关系

- **RPC `config.diagnostics`**：继续存在（serve/embedded 面）；doctor 的 Warn 项与其共用 `diagnostics_warnings()`，单一事实源（US-4.3）。
- **`config-show`**：不变（打印原文，无校验）。
- **TS doctor**：不动（非目标 3）。迁移期两个 doctor 并存，判定口径各自锚定各自内核——这正是本设计的目的（消除 TS 口径对 Rust 配置的误报）。
- **写回 round-trip**：新增 ENTRY_KEYS 常量补进 `wire.rs` 后，`to_toml_string` 的透传语义不变（这些清单只用于"未知键是否保留"判定，doctor 是新的只读消费方）。

## 7. 测试设计

- **mirri-core `config/doctor.rs` 内嵌 `#[cfg(test)]`**（对齐 config 模块测试惯例放 `config/tests.rs`——见 ADR-D2）：
  - 语法错误 → Error 含位置（US-1.3）
  - 合法空文件 → 空 issues（US-1.7）
  - snake_case hooks / camelCase hooks 两种拼写 → 均无未知键 Error（US-3.4）
  - 未知顶层键 / 未知 provider 条目键 / 未知 hooks 键 / 未知 permission rule 键 → 逐条 Error（US-3.1/3.3）
  - 悬空 default_provider / model alias → Warn 不影响 parse_ok（US-4.1）
  - 互斥 loop_control 键 → Error（US-1.4）
  - 未知 experimental flag → Warn（US-6.1）
  - 校验函数不改输入文本（纯函数，US-1.5 的核心层等价物）
- **mirri-cli `tests/cli_tests.rs`**：`doctor` 参数解析（默认无 path / `--config <p>`）。
- **mirri-cli `src/doctor.rs` 内嵌测试**（tempfile + 真实文件）：默认路径缺失 → SKIP/退出 0；显式路径缺失 → ERROR/退出 1；合法文件 → 0；含未知键文件 → 1。
- 命名一律 Given-When-Then（`should ... when ...`）。

## ADR 记录

### ADR-D1 `--config <path>` 平面参数而非子命令树

**Context**：TS doctor 用 `doctor config|tui <path>` 子命令。Rust 版目标是单文件（config.toml），tui 非目标。
**Decision**：`mirri doctor [--config <path>]`。
**Rejected**：
- 复刻 TS 子命令树——为不存在的 `doctor tui` 预留结构，YAGNI；且 clap 子命令 + optional arg 的组合对"无参数=默认检查"的表达更绕。
- 多文件批量 `doctor <p1> <p2>`——无需求方。
**Impact**：US-2 的路径入口；将来 JSON 输出走同级 flag。

### ADR-D2 校验核心测试放 `config/tests.rs` 而非独立 tests/ 集成文件

**Context**：mirri-core 的 config 模块测试惯例是内嵌 `#[cfg(test)] mod tests`（`config/tests.rs`，3400+ 行）。
**Decision**：doctor 核心的单元测试追加进 `config/tests.rs`；只有 CLI 层行为（文件 IO、退出码）在 mirri-cli 侧测。
**Rejected**：`tests/doctor.rs` 独立集成测试——访问不了 `pub(crate)` 清单（若需要），且打破模块内聚惯例。
**Impact**：任务切分时测试落点明确。

### ADR-D3 未知键 = 硬错误（Error）而非警告

**Context**：Rust 内核对未知键静默忽略（round-trip 还保真写回）。可选口径：(a) Warn（忠实于"引擎不炸"）；(b) Error（对齐 TS doctor 价值主张——拼写错误必须暴露）。
**Decision**：Error（US-3.1 定为硬错误）。
**Rationale**：doctor 的存在意义就是补上"静默忽略"的盲区（需求 §0 动机）；若降级为 Warn，`pattern` 拼错成 `patern` 这类问题永远无人发现。引擎行为不变（仍是忽略），只有体检口径变严——这正是"体检口径锚定用户意图而非引擎宽容度"的体现。
**Rejected**：按 serde 实际行为降级为 Warn——让 doctor 退化为"引擎复读机"，丢失 US-3 的全部价值。
**Impact**：用户存量配置若有未知键（如 `micro_compaction` 这类已移除的 flag 键在 `[experimental]` 外的拼写错误），doctor 首跑可能亮红——这是期望行为（发现问题正是目的）。
