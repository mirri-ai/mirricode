# Doctor CLI 现状参考（Context）

> **文档类型：参考类**（按 `docs/specs/AGENTS.md` 治理约定）——现状与现状痛点的盘点，随侦察/现状变化更新。 ·
> 结构快照：2026-10-11 ·
> 配套设计文档：[doctor-cli-requirements.md](doctor-cli-requirements.md) · [doctor-cli-architecture.md](doctor-cli-architecture.md) ·
> 本文不描述目标、不含裁决——终态与裁决在设计文档；执行状态在 ROADMAP / 计划文件。

## 1. TS 侧 doctor 现状（config 校验的唯一现有实现）

- **命令入口**：`apps/mirri-code/src/cli/sub/doctor.ts`。注册于 CLI 命令树：`mirri doctor`（默认检查 `config.toml` + `tui.toml`）、`mirri doctor config [path]`、`mirri doctor tui [path]`。退出码：0 = 全部通过；1 = 至少一个 ERROR。显式传入的路径不存在 → ERROR；默认路径不存在 → SKIP（提示"built-in defaults will apply"）。
- **校验路径**：`config.toml` 校验经 `createMirriConfigRpc().validateConfigToml({text, filePath})`（`packages/node-sdk/src/config-rpc.ts`），进程内 RPC 调用 TS 内核 `parseConfigString`（`packages/agent-core/src/config/toml.ts`）——即 smol-toml 语法解析 + zod schema（`packages/agent-core/src/config/schema.ts`，多数子对象 `.strict()`）双重校验。zod 校验失败时错误带 `validationIssues`（path + message 列表），doctor 格式化为 `Invalid configuration in <path>. Validation issues: <path>: <message>` 逐行输出。
- **tui.toml 校验**：`parseTuiConfig`（`apps/mirri-code/src/tui/config.ts`）——smol-toml + `TuiConfigFileSchema`（zod，**非 strict**：未知键不报错）。键为 snake_case（`disable_paste_burst`、`notification_condition`、`auto_install`）。
- **容错设计**：TS 内核加载 config.toml 有 salvage 机制（`salvageConfigData`）——校验失败时逐 section 丢弃无效段并继续启动（带 warning）。doctor 是**同一 schema 的严格视角**：不 salvage，直接报错。因此存在"启动能跑但 doctor 报错"的合法状态（被 salvage 丢弃的段正是 doctor 要暴露的）。
- **测试**：`apps/mirri-code/test/cli/doctor.test.ts`，Given-When-Then 命名，依赖注入（DoctorDeps：cwd/stdout/stderr/exit/fileExists/readTextFile/validateConfigToml 全部可 mock）。

## 2. Rust 侧 config 解析现状（校验能力盘点）

- **解析器**：`ServerConfig::from_toml`（`rust/crates/mirri-core/src/config/mod.rs:582`）——`toml::from_str` 双重解析（一次 `toml::Value` 保 raw，一次结构体）。serde **非 deny_unknown_fields**：未知键被静默忽略（round-trip 时从 `raw` 合并回写，见 `raw_toml.rs`）。字段别名：`HookDefInput`（`hook_engine/types.rs:100`）接受 `pattern`/`timeout_ms`/`fail_closed`（原生 snake_case）与 `matcher`/`timeout`（秒，自动 ×1000）/`failClosed`（TS 兼容）；各 typed section（ProviderEntry、ModelAlias、LoopControlConfig 等）均有 camelCase serde alias。
- **硬校验**：`from_toml` 内联三类——loop_control 互斥键（`retention_tail_ratio` vs `retention_tail_tokens`）、`[task]`/`[background]` 边界（如 `maxRunningTasks ≥ 1`）。
- **软诊断**：`ServerConfig::diagnostics_warnings()`（`config/mod.rs:835`）——advisory 检查：`default_provider` 指向不存在的 `[providers]` 条目、`[models.*]` alias 的 provider 不存在、遗留 `[model]` section 警告、`disabled_agents` 弃用通知。**已有 RPC 面 `config.diagnostics`**（`mirri-proto/src/rpc/registry.rs:503`，emb-only）暴露这些 warning。
- **已知键清单**：`KNOWN_CONFIG_TOP_LEVEL_KEYS`（`config/wire.rs:466`，28 个顶层键）、`KNOWN_PROVIDER_ENTRY_KEYS`、`KNOWN_MODEL_ENTRY_KEYS`（同文件 1021/1036）——目前用于 round-trip 写回时区分"已知字段被清除"vs"未知键透传"，**未用于校验报错**。
- **flag 注册表**：`mirri_core::FlagRegistry`（`flags.rs:133`）解析 env → config `[experimental]` → 注册默认。目前 Rust 内核只注册一个引擎 flag（`hook-command-rewrite`，`embedded_server/mod.rs:150-170` 的 `new_engine_flag_registry`）。TS 侧另有 `tool-select`、`persistence_minidb_readmodel` 等注册项（各域自行 `registerFlagDefinition`），Rust 侧无对应注册——`[experimental]` 是自由 `HashMap<String, bool>`，未知 flag 名不报错。
- **tui.toml**：Rust 侧**完全不涉及**（grep 零匹配）——tui.toml 是 TS TUI 客户端的文件。

## 3. Rust CLI 骨架现状（命令挂载点）

- **入口**：`rust/crates/mirri-cli/src/lib.rs`（clap `Commands` enum）+ `src/main.rs`（dispatch）+ `src/commands.rs`（非 serve 子命令实现，含 `mirri_data_dir()`——`MIRRICODE_HOME` 优先，回退 `$HOME/.mirri-code`）。
- **现有子命令**：`serve`、`web`、`migrate`、`session-list`、`config-show`、`auth login|status`、`print`。
- **测试模式**：`rust/crates/mirri-cli/tests/cli_tests.rs`——`Cli::try_parse_from` 纯解析测试（不起服务、无副作用），Given-When-Then 命名。
- **config 读取**：`ServerConfig::load(path, fs)`（`config/mod.rs:551`）经 `SyncFs` trait 注入（zero-IO 契约），含 `${VAR}` 展开与 env 覆盖；`load_without_env` 是写回安全版。
- **config-show 先例**：`run_config_show` 直接 `std::fs::read_to_string` 打印原文（mirri-cli 是宿主 crate，允许直接 IO——zero-IO 守卫只约束 mirri-core）。

## 4. 差距清单（TS doctor vs Rust 现状）

| 能力 | TS doctor | Rust `mirri` CLI |
|---|---|---|
| config.toml 语法校验 | ✅ smol-toml | ✅ toml crate（load 即校验） |
| config.toml schema 校验（未知键/类型错误逐项报错） | ✅ zod strict + validationIssues 路径化 | ❌ serde 静默忽略未知键；类型错误只有 serde 单条报错 |
| hooks 键名兼容性 | ❌ 只认 `matcher`/`timeout`/`failClosed`（camelCase），snake_case 写法报 Unrecognized keys | ✅ 两套都认 |
| tui.toml 校验 | ✅（TUI 客户端文件） | ❌（Rust 侧不读该文件） |
| advisory 诊断（悬空 provider 引用等） | ❌（schema 只查结构） | ✅ `diagnostics_warnings()`（但无 CLI 入口；仅 RPC `config.diagnostics`） |
| `[experimental]` 未知 flag 检测 | ❌ | ❌（自由 map） |

## 5. 双内核 schema 漂移实例（2026-10-11 实测取证）

用户实际 `config.toml` 的 `[[hooks]]` 用 snake_case 键（`pattern`/`timeout_ms`/`fail_closed`）：

- `mirri doctor config`（TS 内核）报 `ERROR hooks[0]: Unrecognized keys: "fail_closed", "pattern", "timeout_ms"`——TS zod HookDefSchema strict 只认 camelCase。
- 同一文件 Rust 内核正常解析执行（rtk hook 在 Rust 引擎下工作）。
- 该文件是"面向 Rust 写的"，而 doctor 跑在 TS 内核上，两边 schema 事实不一致。这构成本需求的直接动机：迁移期用户需要**按目标内核（Rust）的口径**校验配置，而现有 doctor 只能按 TS 口径。
