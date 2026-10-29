# Provider 凭据（provider-credential）现状盘点

> **快照日期：2026-10-28**（SDR-003 管理面主次重定；SDR-004 砍除多密钥并存——竞品机制表保留原样作调研记录，差距行已按单凭据口径改写）。本文是 provider 凭据域的代码事实清单与竞品调研记录，随现状变化更新；不含目标与裁决（设计文档单向引用本文）。
> 文档基线 = [provider-credential-requirements.md](provider-credential-requirements.md) + [provider-credential-architecture.md](provider-credential-architecture.md)。
> 调研对象：opencode 仓库本地副本（`../opencode`，commit `eeec76bb`，2026-10-16 检出）。

## 1. Rust core 侧现状（我们）

### 1.1 密钥的存放与解析

| 事实 | 位置 |
|---|---|
| provider 密钥存于 `config.toml` `[providers.<id>].api_key`（与 base_url/model 等非敏感配置同文件同节） | `rust/crates/mirri-core/src/config/mod.rs`（`ProviderEntry.api_key`） |
| 运行时密钥解析三级链：provider `api_key` 字段 → provider `env` 子表 → 按类型约定的环境变量 | `rust/crates/mirri-embedded/src/providers/config_factory.rs`（`resolve_provider_api_key` + `env_value_of` + `api_key_env_name`） |
| CLI 既有子命令：`serve` / `web` / `acp` / `migrate` / `session-list` / `config-show` / `print` / `doctor`——无任何凭据管理命令 | `rust/crates/mirri-cli/src/lib.rs`（`Commands` 枚举） |
| config 写协议：load-without-env → mutate → re-parse 校验 → 原子写（0600 临时文件 + rename）→ reload | `rust/crates/mirri-core/src/config/mod.rs`（`write_config_file_atomically`，委托 `mirri_persistence::write_file_atomically`） |
| 读取面脱敏：`config.get`/`getAll`/`inspect`/`model.list`/`provider.list`/`provider.get` 全部经 `redact_api_keys`（apiKey/api_key → `[REDACTED]`） | `rust/crates/mirri-embedded/src/embedded_server/config_handlers.rs:260`（被 model_handlers / provider_handlers 引用） |
| web 设置页编辑密钥：`setConfig({providers:{<id>:{apiKey}}})` 深合并写回 config.toml；明文永不回传服务端（编辑框留空，`apiKeyDisplay` 仅 env 引用形态显示） | `apps/mirri-web/src/components/settings/models/ProviderDetail.vue`（`submitEditKey` / `openEditKey`） |
| 401/403 → `provider.auth_error`（终态、不重试、无「去哪换密钥」状态投影；错误信息只含供应商报文） | `rust/crates/mirri-core/src/error.rs`（`ProviderAuthError`）+ `loop_engine/retry.rs`（`classify_surface_error`） |
| 私目录/原子写基建已有：`mkdir_private`（0700）、`write_file_atomically`（0600+rename） | `rust/crates/mirri-os/src/sync_fs.rs`、`rust/crates/mirri-persistence` |
| 敏感文件读保护（agent 工具面）：`credentials.json`/`.env`/私钥等读取触发 ask | `rust/crates/mirri-core/src/tools/path_security.rs:549`（`is_sensitive_file`）、`rust/crates/mirri-core/src/permission/policies.rs`（Policy 13 `SensitiveFileAccessAskPolicy`） |
| MCP 配置已有 `${env:VAR}` 引用展开（`mcp.config.preview` 为唯一解析出口，A5 裁决）——provider api_key 无对应机制 | `rust/crates/mirri-embedded/src/embedded_server/mcp_config_handlers.rs`（`dispatch_preview_mcp_config`） |
| ACP 认证面：`authenticate`/`logout` 方法已随 managed login 退役（方法不存在，method-not-found） | `rust/crates/mirri-acp/src/acp_handler.rs:3184`（回归测试钉住） |
| OAuth 存储类型面预留 `storage: 'file'|'keyring'`（klient providerConfigSchema `oauth` 字段），无实现 | `packages/klient/src/contract/global/providers.ts`（`oAuthRefSchema`） |

### 1.2 差距定性（现状 vs 需求）

| 能力 | 现状 | 差距 |
|---|---|---|
| 密钥集中保管与免重启换 key | 每 provider 单一 `api_key` 字段混存 config.toml，换 key = 改配置文件 | 全缺（Epic A，SDR-004 收窄为每供应商一条） |
| CLI 管理 | 无任何凭据命令 | 全缺（Epic B——辅助面） |
| Web 管理界面 | 设置页仅有单 key 编辑（US-6 写 config.toml）；无凭据状态/外部引用面 | 全缺（Epic G——第一管理面，SDR-003） |
| 回读核对（CLI） | 不可能——读取面全脱敏，明文只在 config.toml 文件里 | 全缺（B.5） |
| 外部引用 | 无 | 全缺（Epic C） |
| 失效呈现 | `provider.auth_error` 报文级错误，无状态、无指引 | 缺投影与指引（Epic D） |
| 配置文件可分享 | 密钥与配置同文件 | 缺（D-M1 的动机） |

## 2. 竞品调研：opencode 的凭据管理（借鉴来源）

### 2.1 架构事实

| 事实 | 位置 |
|---|---|
| 凭据归属「integration」（= provider/MCP 服务层），一条 `credential` 表（SQLite），字段：id / integration_id / label / value(JSON) / active / 时间戳 | `packages/schema/src/credential.ts`（`Value = OAuth \| Key \| External` tagged union）、`packages/core/src/credential/sql.ts` |
| `active` 部分唯一索引：`CREATE UNIQUE INDEX ... ON credential (connector_id) WHERE active = 1`——数据库层保证每 integration 至多一条激活 | `packages/core/src/database/migration/20260611035744_credential.ts` |
| 创建/激活/删除均为事务：先清同 integration 的 active，再置新条目；删除激活凭据后最新一条自动接任 | `packages/core/src/credential.ts`（`create`/`activate`/`remove`） |
| 标签缺省自动命名去重（`name`、`name 2`…） | `packages/core/src/integration.ts`（`createCredential`，约 :385 起） |
| 变更发全局事件 `credential.updated` / `credential.switched`；provider 插件订阅 switched → 重新加载（无需重启生效） | `packages/core/src/plugin/provider/openai.ts:323`、`gitlab.ts:286`、`mcp/index.ts:594` 等 10+ 处 |
| **External 类型**：只存 `methodID` + metadata（如 resource name），密钥本体永不入库；Azure CLI 首例——曾有把 CLI token 副本存进 oauth 凭据的旧设计，后被显式迁移为 external（`20261007190000_azure_cli_external_credential.ts` 清除 token 字段）；取用经插件内部 token 机制（带约 5 分钟内存缓存）现场调 `az account get-access-token` | `packages/schema/src/credential.ts`（`External`）、`packages/core/src/plugin/provider/azure.ts`（`token(scope)` 与缓存 map） |
| 运行时解析统一出口 `connection.resolve()`：external → 引用、oauth → 剩余寿命 <5min 自动刷新写回 | `packages/core/src/integration.ts:723-741` || 凭据失效是**状态**不是异常：`needs_auth` 挂在连接上（带 message + url 行动指引），投影进 list/get | `packages/schema/src/connection.ts`（`Status`）、`opencode.ts`（`connectionStatus` 映射 SsoRequired/SignedOut） |
| 旧 `auth.json` 单文件 → 数据库的迁移：宽松解码（不认识的条目跳过不报错）、按 integration 幂等（已存在跳过） | `packages/core/src/database/migration/20260805200742_import_legacy_credentials.ts` |
| CLI：`auth login`（交互选择 integration → method：oauth auto/code、command、key、external）/ `list` / `switch` / `logout` / `export` / `import` | `packages/cli/src/commands/handlers/auth/*.ts` |
| `export` 明文导出：TTY 时 stderr 先警示（"output contains secrets"）；`import` 走 stdin 管道；幂等（按 id 跳过已存在） | `export.ts` / `import.ts` |
| **明文回读面**：REST `credential.list` 返回**含明文 secret**（服务端 Authorization 中间件兜底）——本项目明确不借鉴（见 SDR-002 / ADR-003） | `packages/server/src/handlers/credential.ts` |
| **存储明文不加密**：全仓无 keychain/加密实现（加密依赖单入口收拢 + 脱敏层） | 全仓检索 `encrypt/keyring/safeStorage` 零命中 |
| 日志脱敏双通道：模式匹配（sk-/AIza/ghp_/Bearer/私钥头）+ **动态扫描环境变量名匹配 KEY\|TOKEN\|SECRET 的值** | `packages/http-recorder/src/redaction/secrets.ts` |

### 2.2 借鉴裁决对照（详见 requirements SDR-001/002）

| opencode 机制 | 本域采纳 | 说明 |
|---|---|---|
| 独立凭据库 + active 激活 + 事件驱动重载 | ⚠️ 部分（SDR-004） | 采纳独立库与免重启生效；**多凭据 + 激活切换被砍除**（每供应商一条，用户裁决从未有此需求）；SQLite → 单文件 JSON（ADR-001） |
| 多 method 声明式注册（oauth/command/key/external/env） | ⚠️ 部分 | 本域只取 external（Epic C）；oauth 留槽（P-2）；command/env 不引入（我们无 command 型来源诉求，env 已有旁路） |
| external 引用（Azure CLI） | ✅ Epic C | 含「曾存 token 后改引用」的反例教训直接吸收为 C.1 |
| needs_auth 状态投影 | ✅ Epic D | 改名 `needs_reconfigure`（我们的失效主因是 key 被拒，非登录态） |
| auth.json 宽松迁移 | ✅ Epic E | 对应我们的 config.toml api_key 字段 |
| export TTY 警示 | ✅ B.5 | 用在我们的 show 回读上 |
| REST list 返回明文 | ❌ 不借鉴 | ADR-003：明文出口只留 CLI 本地 |
| 完全不加密 | ⚠️ 用户裁决 | 本批同 opencode 明文（SDR-002），但保留 P-1 钥匙串提案路径 |

## 3. 既有裁决/相邻域约束（本域设计不可违反）

| 约束 | 出处 |
|---|---|
| 「不动 provider 配置与凭证域」——spec 11 当时把本域划为正交邻域，未做裁决 | `docs/specs/rust-core-design/11-model-resolution.md` §1.3（本域建域即其补位，无冲突） |
| model-settings US-6「编辑供应商凭据」现状写 config.toml；本域目标态把它改写为写凭据库（D-M6） | `docs/specs/model-settings-page-requirements.md` US-6 |
| auth-oauth 域托管登录已全线退役（SDR-004）；oauth 值类型只作未来槽位 | `docs/specs/auth-oauth-requirements.md` §13 P-2 / 本域 D-M4 |
| TS core 处于退役线，新引擎能力一律 rust first | 根 `AGENTS.md`「Engine-target gate」 |
| 配置写入走原子写协议（env 不烘焙进文件） | `rust/crates/mirri-core/src/config/mod.rs` 写协议注释（§1.1 表） |
| MCP 域 `${env:}` 展开的「唯一解析出口」纪律（A5）是 MCP 域内裁决，不约束 provider api_key 面 | `docs/specs/mcp-architecture.md`（引用自 mcp_config_handlers 注释） |
