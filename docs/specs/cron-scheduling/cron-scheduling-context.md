# 定时任务（cron-scheduling）现状盘点

> **快照日期：2026-10-30**（建域首批盘点，代码基线 HEAD `6015a8ed`；2026-10-31 补记 SDR-009/010 的动机场景，代码基线未变；2026-11-03 补记 §5 的 JSON 存储现状快照与 SQLite 基建事实——ADR-020 的事实源）。本文是定时任务域的代码事实清单，随现状变化更新；不含目标与裁决（设计文档单向引用本文）。
> 文档基线 = [cron-scheduling-requirements.md](cron-scheduling-requirements.md) + [cron-scheduling-architecture.md](cron-scheduling-architecture.md)。
> 域范围裁决（SDR-001）：cron 子系统从后台任务与子代理域（subagent E11/E12）**拆出**独立成域；域边界与拆域理由见需求文档 §0。
> 阶段定位（SDR-009/010，用户裁决 2026-10-31）：阶段一（会话级）服务 **agent 自计时需求**——单次延时回访（PR 创建后等 CI 出结果，避免循环 sleep-poll 浪费算力）、周期巡查、未来指挥官-工人的协作 check；session scope 是该类需求的正确归属。阶段二（工作区级）服务**用户委托例行工作**的产品化需求。两类需求共享机制基建、定位独立。

## 1. 数据模型与核心引擎（mirri-core）

| 事实 | 位置 |
|---|---|
| `CronTask` 字段全集：id / cron / prompt / session_id / recurring / timezone(Option) / created_at / last_fired / next_fire_at——**无 origin、无 max_fires、无 fire 计数** | `rust/crates/mirri-core/src/cron.rs:85-112` |
| stale 阈值 7 天，从 `created_at` 起算不随 fire 重置：`now - task.created_at >= STALE_THRESHOLD`——长期循环任务每 7 天必死一次 | `rust/crates/mirri-core/src/cron.rs:74-77`、`:854-856` |
| stale 语义：只作用于 recurring；stale 触发**总是搭载一次投递**（最后一次带 stale 标记后删除），从不在无投递时静默清除 | `rust/crates/mirri-core/src/cron.rs:852-856` |
| 持久化 = 会话 wire 事件流：`CronAdd` / `CronDelete` / `CronCursor` 三种 wire op，`record_op` 写入 session 的 `wire.jsonl` | `rust/crates/mirri-core/src/cron.rs:318`、`:425`、`:523`、`:764`、`:1939` |
| `CronManager::add` 的唯一非测试调用方是 CronCreate 工具（模型创建路径）；RPC 面无 cron.add——`routes/task.rs` 只有 list / delete / trigger 三臂 | `rust/crates/mirri-core/src/tools/cron_create.rs`、`rust/crates/mirri-embedded/src/embedded_server/routes/task.rs:55-73` |
| 一次性任务首触发上限 350 天、每会话 ≤ 50 个活跃任务、prompt ≤ 8KiB、表达式 5 年内须有触发（拒绝条件族） | `rust/crates/mirri-core/src/cron.rs`（`add` 校验）；参数族亦见 subagent-context §6.2 |
| wire DTO 把 session_id 折进 `tags.sessionId`（`CRON_SESSION_TAG`）——任务归属会话是**存储结构内的必填字段** | `rust/crates/mirri-core/src/cron.rs:114-119` |
| journal 路由：cron 的 wire ops 落主 journal（`JournalTarget::Session` 一侧），与子代理分域路由表无交集 | subagent-requirements SDR-B4-02 归属表（`cron.*` 在主域列） |
| `recurring` 缺省解析两层不一致（SDR-003 背景）：工具层缺省**循环**（注释明示 "defaults to true when absent so a bare cron + prompt is treated as a recurring schedule"），恢复读取层缺省**一次性**（`.unwrap_or(false)`） | `rust/crates/mirri-core/src/tools/cron_create.rs:93-97`、`rust/crates/mirri-core/src/cron.rs:205-210` |

## 2. 调度驱动（mirri-embedded cron_driver）

| 事实 | 位置 |
|---|---|
| driver 是 **per-session tokio 循环**：session create / resume 时 `spawn_cron_driver` 起一个专属任务；server tick 默认 30s（`CRON_DRIVER_TICK_SECS`，可测试覆写 `with_cron_tick`） | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:209-230`、`rust/crates/mirri-embedded/src/embedded_server/mod.rs:1394`、`:1703`、`:1726` |
| **无 app-wide scheduler**：没有一个 server 级统一 tick 遍历所有会话的任务；任务只在拥有者会话的 driver 里活着 | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs`（全文件结构即证） |
| busy 语义：目标代理回合运行中，fire 被挂起（held），在首个 idle tick 注入；**session 关闭时 held fires 被丢弃——注释明示 "lost permanently by design"** | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:249-285` |
| resume 时补发：错过的触发按 coalesce 合并为一次注入（TS catch-up 语义，游标从 `max(last_fired, created_at)` 重算） | `rust/crates/mirri-core/src/cron.rs:557-623`、`:691-807` |
| 注入路径：cron 注入的 turn 与交互 turn 走**同一** run_engine_turn + InteractionKernel（cron_job origin）；注入目标是会话的 canonical 主代理（first-agent fallback） | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:445-456` |
| **`cron.trigger`（手动触发）直接走 `CronManager::trigger` 注入路径，无 busy gate**——与自然触发（busy 时 held）不一致，busy 会话中被手动触发会与运行中回合并发投递 | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:92-130`（dispatch_trigger_cron_task 直接触发注入）；对照 `:300-305`（自然触发的 busy 检查） |
| 手动触发 run-now 不动日程：零 wire op、游标不推进、不消耗 one-shot（subagent SDR-B6-01 已裁决） | `rust/crates/mirri-embedded/src/embedded_server/routes/task.rs:67-73`；subagent-requirements SDR-B6-01 |

## 3. 无人值守与审批链（核心缺陷链，issue #179）

| 事实 | 位置 |
|---|---|
| `PermissionPolicyContext` 无 turn origin / 无人值守声明字段——策略链看不到「这轮是谁发起的」 | `rust/crates/mirri-core/src/permission/mod.rs:228-244` |
| `PromptOrigin::is_real_user_input()` 把 `SkillActivation` / `PluginCommand` / `UserSteer` 全算作用户输入——cron 注入的 turn 是 `cron_job` origin，不在此列 | `rust/crates/mirri-core/src/origin.rs:112-118` |
| headless 生产恒 false：`Ask` 结果在 headless=false 时翻译为 `Blocked`（"approval required but no approval mechanism available"） | `rust/crates/mirri-core/src/permission/mod.rs:519-525`、`:286-291` |
| `deny_when_unattended` 目前只服务私有网络 fetch（SDR-007 d：无人值守时 ask 一律拒绝）；其余策略全部 false | `rust/crates/mirri-core/src/permission/policies.rs:461`（唯一 true 处） |
| 后果：无人值守（无人在场审批）时，cron 注入 turn 里任何需要审批的工具调用会**卡住直到回合超时**（turn 超时 300s 量级）——不是拒绝、不是改道，是挂死 | cron 注入与交互 turn 同路（§2 注入路径事实）+ headless 恒 false 推论 |

## 4. 生命周期承诺与文档现状

| 事实 | 位置 |
|---|---|
| 用户文档承诺 "closing the UI is fine"（关 UI 不影响任务，重开后继续触发）——**该承诺只在「进程活着、会话未关」时近似成立**：任务实际绑定进程内 driver，进程退出即无人触发 | `docs/en/guides/use-cases.md:124` |
| **`MIRRICODE_DISABLE_CRON` 幽灵开关**：6 处用户文档宣传该总开关（env-vars.md:134、tools.md:119、use-cases.md:126 及各自 zh 镜像），rust 代码**零实现**（全仓 rg 无该 env 消费点；rust 侧存在的 cron env 仅有 `MIRRI_CRON_*` 测试族） | `docs/en/configuration/env-vars.md:134`、`docs/en/reference/tools.md:119`、`docs/en/guides/use-cases.md:126`；代码反证：`rg MIRRICODE_DISABLE_CRON rust/` 零命中 |
| 用户文档承诺 recurring 7 天过期 + stale 终信号——与代码一致（从 created_at 起算） | `docs/en/guides/use-cases.md:124`、`rust/crates/mirri-core/src/cron.rs:854-856` |
| web CronPanel 只列**活跃会话**的任务（"scoped to the ACTIVE SESSION — cron tasks live on the session"），无会话打开时呈诚实空态；创建仍只能靠模型的 CronCreate 工具 | `apps/mirri-web/src/components/settings/CronPanel.vue:1-8` |

### 4.1 幽灵开关小节（D-M6 / ADR-006 / 需求 E.4 引用的现状证据）

`MIRRICODE_DISABLE_CRON` 的完整证据链：6 处用户文档宣传该总开关（env-vars.md:134、tools.md:119、use-cases.md:126 及各自 zh 镜像），rust 代码**零实现**（全仓 `rg MIRRICODE_DISABLE_CRON rust/` 无该 env 消费点；rust 侧存在的 cron env 仅有 `MIRRI_CRON_*` 测试族）。即：当前代码对「定时子系统总开关」零实现、文档却多处宣传——需求文档 E.4 将其兑现为真实行为，本小节为其现状依据。

### 4.2 创建表单（cron-task-creation-ux）批 7 遗留注意点（批 8 消化）

| 注意点 | 现状事实 | 批 8 动作 |
|---|---|---|
| **m1 过渡态裸表达式输入**：批 7 的 CronPanel 创建表单仍是裸表达式输入 + SchedulePicker 并存的过渡形态 | `apps/mirri-web/src/components/settings/CronPanel.vue`（Form batch 7 transitional 注释）；raw 文本不 trim 存储、提交时才 trim（B1 修复） | CronCreateForm（架构 §3）整体替代过渡表单，裸表达式入口消失 |
| **m3 时间选择 15 分钟粒度**：daily/weekday/weekly/monthly 的时间下拉只列 `:00/:15/:30/:45`，但需求 VU3 的示例时刻是 `:03`（每小时第 3 分）——任意分钟不可达 | `apps/mirri-web/src/components/settings/cron/SchedulePicker.vue` 的 `timeOptions`（hour × `['00','15','30','45']`） | 改为自由时间输入（`<input type="time">`，picker 内 field/interval 模式已用该形态）或分钟下拉扩到 0-59 全量；取实现小者 |

## 5. workspace 级任务的耦合清单（issue #181，阶段二侦察）

> **2026-11-03 增补（存储现状快照，ADR-020 的事实源；同日实施后更新）**：ADR-020 已落地——工作区任务现存储于引擎数据根的单一 SQLite 库 `<home>/cron-index.db`（`rust/crates/mirri-cron-index/`，`scheduled_tasks` 表，workspace_id 为普通列，runs 内嵌任务行 body）；`WorkspaceCronManager` 经 `open_with_index` 挂共享索引、全部变更经 `mutate_store` 单收口点写穿（事务化 `replace_workspace`）；JSON 形态（`<会话数据根>/<workspace_key>/cron/tasks.json`）降级为一次性导入源 + 只读存档（`meta` 表标志保证导入仅一次）；工作区删除级联改为 `delete_workspace` 行删除（`remove_files` 保留清理存档文件）；web 任务面板为全工作区聚合视图（cron-improvement A14，`groupTasksByWorkspace`）。既有 SQLite 基建：`rusqlite = "0.40"`（bundled）已为 workspace 依赖，会话索引 `rust/crates/mirri-session-index/src/lib.rs` 是既有 SQLite 消费方（`<引擎数据根>/session-index.db`，busy 超时 + 单写者约定）——`mirri-cron-index` 沿用同款约定。

CronManager 与 session 的 12 处耦合（全部已核验）：

| # | 耦合点 | 位置 |
|---|---|---|
| 1 | 数据模型 `session_id` 必填字段 | `rust/crates/mirri-core/src/cron.rs:93` |
| 2 | wire DTO 把归属折进 `tags.sessionId` | `rust/crates/mirri-core/src/cron.rs:114-119` |
| 3 | 任务归属 `SessionScope`（会话生命周期管理面） | subagent E11 US11.4 |
| 4 | CronCreate 工具构造绑定（工具注册于会话级 manager） | `rust/crates/mirri-core/src/tools/cron_create.rs:9` |
| 5 | journal 路由 `JournalTarget::Session`（写会话 wire.jsonl） | `rust/crates/mirri-core/src/cron.rs:318` |
| 6 | fork 不复制（三方域一致裁决，见 glossary A9） | glossary A9 fork 边界口径 |
| 7 | 存储随 session wire.jsonl（任务记录是会话事件流的一部分） | `rust/crates/mirri-core/src/cron.rs:1939` |
| 8 | driver per-session（任务只在拥有者会话的循环里活着） | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:209-230` |
| 9 | 注入目标 = canonical 主代理 | `rust/crates/mirri-embedded/src/embedded_server/cron_driver.rs:445-456` |
| 10 | turn_flows / event bus（事件按会话维度流转） | `cron_driver.rs` spawn 参数（`:215-222`） |
| 11 | RPC `sessionId` 必填（cron.list / delete / trigger 均以会话为参数） | `rust/crates/mirri-embedded/src/embedded_server/routes/task.rs:55-73` |
| 12 | web UI `activeSessionId`（面板只看活跃会话） | `apps/mirri-web/src/components/settings/CronPanel.vue:3-5` |

workspace 级前置件（**已就绪**）：

| 事实 | 位置 |
|---|---|
| `session.create` 已支持 `workspaceId`（路由 key on workspace_id） | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:211-259` |
| `mirri print` 已验证**无 UI 起跑 session 可行**：create → wait 模式 message.send → 流式收结果；但 manual 审批路径会卡到回合超时 | `rust/crates/mirri-cli/src/print.rs:148-162` |
| SDR-004 懒物化：session.create 只进内存，**首条用户消息才落盘进 index**——对「每次运行产生一个新 session」的 workspace 级任务是天然友好的存储形态 | 会话管理域 SDR-004 |

## 6. 可借鉴的既有骨架

| 事实 | 位置 |
|---|---|
| `orphan_governance.rs`：**server 级常驻循环**（60s sweep，`ORPHAN_LIFETIME_SWEEP_SECS`），tokio interval 驱动、无会话依赖——app-wide scheduler 的现成结构模板 | `rust/crates/mirri-embedded/src/embedded_server/orphan_governance.rs:20-49`、`:119-133` |
| `keep_alive_on_exit`（后台代理）：用户**显式配置存活**的产品姿态先例——关闭会话时任务「保留到后台」是用户选择而非隐式默认 | `rust/crates/mirri-core/tests/background_agent_tests.rs:588-662` |
| held-buffer purge（#177 顺带发现）：held fires 的清空时机存在独立缺陷，已裁定为**独立快修**，不在本域 spec 范围 | #177 快修批次 |

## 7. 痛点定性（现状 vs 用户诉求）

| 痛点 | 现状机理 | 影响 |
|---|---|---|
| 无人值守必挂死 | ask 在无审批面时 Blocked 至回合超时，无「预声明无人值守 → 预先拒绝」机制 | #179：agent 的定时回访（CI 结果确认等自计时场景）在无人值守时不可用 |
| 任务无法自限 | 无 max_fires / fire 计数，模型只能靠提示词自律 | #179：跑偏的循环任务持续烧钱 |
| 用户无法直接创建 | RPC 无 cron.add，创建路径只有模型的 CronCreate 工具 | #179：用户必须「请模型代为排定」 |
| 长期任务 7 天必死 | stale 从 created_at 起算，fire 不重置 | #181：看护类长期工作流无法存活 |
| 进程退出即失能 | driver per-session、无 server 级调度面 | #180（诚实性口径，SDR-010）：活会话的任务在应用存活期间触发不可靠 + 静默失约不可见，文档承诺含糊 |
| 手动触发可并发闯入 | cron.trigger 无 busy gate | #177：busy 会话手动触发与运行中回合并发 |
| 幽灵开关 | `MIRRICODE_DISABLE_CRON` 文档宣传、代码零实现 | #177：用户设置了开关，行为毫无变化（R4 信号真实性违背） |
| workspace 级零实现 | 12 处会话耦合（§5 清单） | #181：用户的例行工作委托无法脱离单会话存活 |

## 8. 与相邻域的关系（本域设计不可违反的既有裁决）

| 约束 | 出处 |
|---|---|
| cron 表达式语义（5 字段/本地时区/jitter/coalesce/stale 投递形态/DST 边界）已由 subagent 域 SDR-005/006/B6-01 裁决，本域**沿用不重定义** | `docs/specs/subagent-requirements.md` SDR-005、SDR-006、SDR-B6-01 |
| fork 一律不携带定时任务（三方域一致） | glossary A9；会话管理域 SDR-007；会话持久化域 SDR-002 |
| 删除会话级联清除其定时任务 | 会话持久化域 |
| run-now 不动日程、无 manual 事件标记 | subagent SDR-B6-01 |
| 每会话 ≤ 50 活跃任务、prompt ≤ 8KiB、one-shot 首触发 ≤ 350 天 | subagent N12 |
| 等待语义/回合准入（忙则并入、闲则自起）由回合调度域统一裁决，本域注入遵守同一准入契约 | `docs/specs/turn-scheduling-requirements.md`（L1 异步唤醒源） |
| 审批策略链的形态（policy chain + Ask/Deny/Approve）与「无人值守 ask 一律拒」的先例 | approval 域（SDR-007 d 先例：`deny_when_unattended`） |
| cron 事件 `cron.fired` / 注入信封 `<cron-fire>` 已定型（B2/B6） | subagent E11/E12、SDR-B2-01 |
