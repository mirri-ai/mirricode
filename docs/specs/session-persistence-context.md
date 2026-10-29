# 会话持久化与索引 — 现状盘点（Context）

> 快照日期：2026-09-10（存储根切割 ADR-005-R1 落地后刷新 §1.1/§1.2/§2；其余沿用 2026-09-09 盘点）· §1.2 增补：2026-09-12（标题自动派生批 `56d81ca5`）· §8 增补：2026-09-13（Rust v3 会话审计 11 项问题登记）· §1.6 新增：2026-10-28（IDE 入口存储归属现状盘点，ADR-019/SDR-009 立项依据）·
> 本文只盘点**当前状态与痛点**，不含目标、不含决策（那些见 [session-persistence-requirements.md](session-persistence-requirements.md) / [session-persistence-architecture.md](session-persistence-architecture.md)）。
> 排查基线：Rust 分支 `feat/rust-agent-profile`，Phase 1 提交 `ef9d8ace`、Phase 2 提交 `0fd773fb`、Phase 3 提交 `c683a73b`。

## 1. Rust 侧现状（按架构分层）

### 1.1 存储布局与派生

- 会话目录：`<sessions_dir>/<wd_id>/<session_id>/{state.json, wire.jsonl, snapshot.json}`；读取端双布局兼容（平铺/桶，结构探测）——`rust/crates/mirri-core/src/session/persistence.rs`。
- 工作区键派生：`rust/crates/mirri-core/src/session/workdir_key.rs`（`encode_work_dir_key`，与 TS 逐字节对齐，词法规范化不解析符号链接；9 个单测）。
- 一次性布局迁移：`SessionPersistence::migrate_legacy_flat_sessions` + 嵌入式服务器首个 RPC 前内联等待（`rust/crates/mirri-embedded/src/embedded_server/mod.rs` 的 `migrate_legacy_sessions_once`）。
- sessions_dir 来源（**存储根切割，ADR-005-R1，2026-09-10**）：注入 home 时 `<home>/v3_sessions`（`rust/crates/mirri-embedded/src/bootstrap.rs`）；未注入时 `<cwd>/.mirri/v3_sessions`（`SessionManager::new`，`session/manager/mod.rs`）。TS 引擎仍只读旧根 `sessions/`——跨引擎恢复自切割起物理不可能；`mirri migrate-sessions`（`rust/crates/mirri-cli/src/migrate_sessions.rs`）为显式迁移路径：`<bucket>/<id>` 目录整体搬移入 v3 根、不重写 wire 日志（1.5/1.6→2.0 迁移 resume 时惰性执行）、幂等（目标根已有同 id 会话跳过不覆盖，`--copy` 保留源）。

### 1.2 数据契约

- `SessionMetadata`：`title / created_at / updated_at / tags / archived / work_dir`——`rust/crates/mirri-core/src/scope/config.rs`。
- **标题自动派生（2026-09-12，`56d81ca5`）**：`SessionMetadata.is_custom_title`（`config.rs:342`，state.json 持久化名 `isCustomTitle`，`persistence.rs:100/133/146`）；派生逻辑 `rust/crates/mirri-core/src/session/title.rs`——`is_untitled_title`（空/空白/缺省占位名判定，`:23`）+ `title_from_prompt_text`（脱敏：私钥块/Bearer/键值密钥/`sk-` 长串/超长 base64 形态 token → `[redacted]`；控制字符与空白折叠；200 字符截断，`:33`），与 TS `sanitizeAndTruncatePromptText`（`packages/agent-core/src/session/prompt-metadata.ts:60`）逐条对齐；send 路径集成（未命名且未自定义时改标题并广播 `session.meta.updated`）；create/rename/fork/child 路径置位 `is_custom_title`（改名置真、创建/派生置假）。
- `state.json` 并集字段 + 双拼法时间戳（RFC 3339 写、epoch-ms 兼容读）——`rust/crates/mirri-core/src/session/persistence.rs`（`PersistedSessionMetadata` + `deserialize_epoch_or_rfc3339`）。
- wire.jsonl：版本链 `1.0→…→1.6→2.0`（`rust/crates/mirri-persistence/src/migration.rs`，`CURRENT_VERSION = "2.0"`；1.6→2.0 为 **identity 迁移**——纯纪元重戳、记录逐字节不变，ADR-006-R2）；恢复时自动迁移（`rust/crates/mirri-core/src/session/manager/restore.rs`，磁盘失败有内存迁移回退）；超前版本原样重放；CLI `mirri migrate` 提供运维路径（`rust/crates/mirri-cli/src/commands.rs`，双根扫描：home 侧 `v3_sessions` 先于旧 `sessions`，workspace 侧 `.mirri/v3_sessions` + `.mirri/sessions`）。
- 检查点：收起时写 `snapshot.json`，恢复只重放增量——`rust/crates/mirri-core/src/session/manager/mod.rs`（`snapshot_session_on_close` / `replay_agent_state`）。

### 1.3 会话索引（S4）

- 新 crate `rust/crates/mirri-session-index`：rusqlite 0.40（bundled），WAL + busy_timeout 5000 + 启动 checkpoint；`sessions`/`workspaces` 两表；15 个单测。
- 接线：`rust/crates/mirri-embedded/src/embedded_server/session_index.rs`——启动对账（回填缺失/过时行、孤儿回收、绑定工作区注册 + `workspaces.json` 写通）、写通点（create/close/archive/restore/rename/updateMeta）、读模型（列表冷缺失回源补齐）。
- 索引位置：`<home>/session-index.db`（仅 home 注入的部署；无 home 全路径回退磁盘扫描）。
- 工作区注册表镜像：`rust/crates/mirri-session-index/src/registry.rs`（容错读 + 原子写、保留删除墓碑）。

### 1.4 归档语义（S2）

- `dispatch_archive_session`：活跃会话收起后置标记；仅落盘会话直接置标记（不再报 Session not found——本批工作的起点缺陷）。
- 默认列表排除归档在读取层执行（`list_persisted_sessions(include_archived)` → `list_all_sessions(include_archived)`）；`dispatch_list_sessions` 接受 `includeArchive` 参数（camelCase + snake_case 别名）。
- `session.restore` = S1 恢复 + 清标记；`session.reload` 不清标记（与 TS 对齐）。

### 1.5 子代理记录现状（2026-09-09 复核，子代理域交接校验）

对子代理域交接清单的逐条复核结论（与清单的差异以复核为准）：

- **Rust 子代理对话未持久化（比交接清单所述更严重）**：子代理宿主装配子代理 turn flow 时不装记录器（`rust/crates/mirri-core/src/subagent/host.rs` 的装配列表无 `set_wire_recorder`；对比主 agent 派发路径 `mirri-embedded/src/embedded_server/message_handlers.rs` 有装）。所谓"子代理事件与主会话混写同一 wire.jsonl"不成立——子代理的对话与工具记录**根本不落盘**，重启后无法续作、无法回放、无法导出。清单引用的 `session_scope.rs:217` 实为 cron 注释，行号错误。
- `SessionScope::try_install_subagent_wire_log`（`session_scope.rs:413`）存在但**无任何调用方**——一个未接线的预留缝。
- TS 侧三条声明均属实：agents 注册表持久化于 `state.json`（`sessionMetadataService.ts:120-128`）；每身份独立 journal `agents/<agentId>/wire.jsonl`（`kap-server/test/workspaceLayout.test.ts:11-12`，含 main）；派发面镜像 `mirrorAgentRun`。
- 其余属实：agents 表仅内存（`session_scope.rs:120`）；任务重启调谐 lost→stopped（`task.rs:403`）；会话级服务逐 agent 注入（`session_scope.rs:450+`）；`max_concurrent_agents` 默认 10（`config.rs:120`）；子代理 live 事件以独立 wire turn id 上会话事件总线（`host.rs:352-364`）。
- "会话事件流 durable 可重放"对 server 后端成立（环形缓冲 seq 源）；embedded 后端的 seq/epoch 标记为事件统一 b3 批次的未竟项（`event_projection.rs` 头注）。

终态裁决见架构 ADR-006（子会话载体）。

### 1.6 IDE 入口（ACP 宿主）存储归属现状（2026-10-28 盘点，ADR-019 立项依据）

- **组合根不注入 home**：ACP 宿主的默认（嵌入引擎）路径以 `EmbeddedServer::for_workspace_root(WorkspaceId::new("acp"), workspace_dir, None, …)` 组装——第三参 `home_dir = None`（`rust/crates/mirri-acp/src/run.rs:103-108`；`MIRRI_ACP_WORKSPACE_DIR` 或当前目录为根）。
- **无 home 分支的存储落点**：`assemble` 在 `home_dir == None` 时走 `SessionManager::new(workspace)`，会话目录 = `<cwd>/.mirri/v3_sessions/<工作区桶>/<会话 id>/`（`rust/crates/mirri-embedded/src/bootstrap.rs:669-676`；`rust/crates/mirri-core/src/session/manager/mod.rs:339-342`）；同时不安装 `<home>/session-index.db` 索引、不还原 `workspaces.json` 注册表行（`bootstrap.rs:265-267/275-276`）。
- **列表钉扎**：`for_workspace_root` 把宿主列表可见性钉死在其根（"the home's workspaces.json may register more … those sessions are not this host's to see"——`rust/crates/mirri-embedded/src/embedded_server/mod.rs:3017-3023`）；按 cwd 的引擎池使同 cwd 会话共享一个引擎（`run.rs:109-138`）。
- **对照组**：`mirri serve`/`mirri web` 注入解析的 home，存储落 `<home>/v3_sessions/` 并享索引与注册表（`rust/crates/mirri-cli/src/main.rs:161/194/215-222`）——两个存储根互不可见，桌面/网页列表看不到工作区本地的 ACP 会话。
- **例外通道**：`MIRRI_ACP_REMOTE_ENGINE` 指向远程引擎时，会话由远程端承载（落其 home）——共享场景存在但默认关闭（`run.rs:72-85`）。
- **存量事实**：`mirri migrate` 的迁移对是「legacy `sessions/` → `v3_sessions/`」（home 侧与 `--workspace` 指定的工作区侧皆然；`rust/crates/mirri-cli/src/migrate.rs:53/83-92`）——它**不覆盖** ACP 会话实际落的工作区本地 rust 会话（`.mirri/v3_sessions/`）。工作区本地 rust 会话目前**没有任何晋升入 home 的路径**（`commands.rs` 的 `session-list` 双根扫描只列不迁），是 ADR-019 落地的配套缺口。
- **治理空白**：`docs/specs` 全域无任何 SDR/ADR 显式裁决过"ACP 会话对 web/desktop 的可见性"——该行为是 ADR-005-R1 认可工作区本地根 + ACP 组合根不注入 home 的叠加涌现，非显式裁决。该空白由需求 SDR-009 裁决。

## 2. TS 参照基线（agent-core-v2）

| 主题 | TS 现状 | Rust 对齐情况 |
|---|---|---|
| 目录布局 | `<sessionsDir>/<workspaceId>/<sessionId>/`，历史大小写分裂桶读取面合并 | ✅ 布局形态同构（桶/平铺双读）；⚠️ **存储根已分离（ADR-005-R1）**：rust 写 `v3_sessions/`、TS 只读旧根 `sessions/`，互不可见（`migrate-sessions` 为单向显式迁移）；大小写分裂桶合并未做（Rust 单桶形态暂未遇到） |
| 工作区键 | `encodeWorkDirKey`（`packages/agent-core/src/session/store/workdir-key.ts`） | ✅ 逐字节对齐 |
| 归档 | `sessionLifecycleService.archive()`：活跃会话置标记；非活跃**静默无操作** | ⚠️ Rust 对非活跃会话置标记（比 TS 更诚实，行为差异已知且有意） |
| 会话索引 | `session-index.db` @ `<homeDir>/cache/`，schema 见 `sqliteSessionStore.ts` | ✅ 同构读模型；文件位置分离；⚠️ `lastPrompt` 列 Rust 未存（元数据不携带） |
| 工作区注册表 | `workspaces.json`（`fileWorkspacePersistence.ts`） | ✅ 读入 + 写通镜像 |
| 标题自动派生 | v1 `session/rpc.ts:334-377`（`needUpdateEasyTitle`：未命名且未自定义 → `titleFromPromptMetadataText`，`session.meta.updated` 广播）+ `prompt-metadata.ts`（脱敏/截断）；v2 同构（`agent-core-v2/src/agent/rpc/prompt-metadata.ts:63-79`）；rename 置 `isCustomTitle:true`（v1 `session-store.ts:140/402`、kap-server `routes/sessions.ts:602`） | ✅ 2026-09-12 对齐（`title.rs`，见 §1.2）；此前为**文档级缺口**——TS 已落地但需求文档未披露，现以 US10（N10）补录基线 |
| 列表过滤 | `includeArchived !== true → archived = 0`；另有 `childOf`（子会话过滤，custom LIKE） | ✅ archived 过滤；⚠️ `childOf`/`archivedOnly` 参数未实现 |
| wire 版本 | `1.5`（`packages/agent-core-v2/src/wire/migration/migration.ts`） | Rust `2.0`（1.5→1.6 载荷归一化 + 1.6→2.0 identity 纪元，ADR-006-R2）；对 TS 日志记录载荷恒等，但根已分离（ADR-005-R1）互不可见 |
| 时间戳 | epoch-ms 整数（读写） | RFC 3339 写、双拼法读；索引定宽文本（ADR-003） |

## 3. 已知差距与痛点（开放项，入 ROADMAP）

1. **"会话管理"域未立项**：置顶（pin）、拖拽排序（工作区级/会话级）无需求文档；存储侧 `workspaces.sort_order` 已预留但无写入面；wire 契约未登记（mirri-proto 注册表无 `session.pin` 等方法）。
2. **列表参数缺口**：`archivedOnly`（仅看归档，Web 设置页已在用）与 `childOf`（子会话过滤）未实现；Web 的 `includeArchive` 已通。
3. **搜索扫描集**：搜索当前固定包含归档会话（保持改动前行为），与列表默认排除的语义存在张力，待"会话管理"域裁决。
4. **CLI 未接索引**：`mirri-cli` 的 list/migrate 仍是直接目录遍历（行为正确，仅不享受索引加速）。
5. **`lastPrompt` 未持久化**：Rust 元数据不携带该 TS 字段（透传保留但从不写入），索引同缺。
6. **测试债**：`mirri-test-harness cross_backend` 存在 2 个与本域无关的既有失败（`model.list` 的 `id` 字段漂移、`session.export` 的 v2-pin manifest 差异），属其他域，修复合流前会干扰本域回归判读。
7. **标题自动派生为 2026-09-12 补录的基线缺口**：两代 TS 引擎均已实现（见 §2 行"标题自动派生"）但需求文档此前未收录——已以 US10（N10）补入需求文档；本行保留记录以免后续盘点误判为"新增需求"。`lastPrompt` 语义裁决（SDR-004）与 US10 的触发条件已衔接：未命名态判定与"从未对话"判定相互独立。

## 4. 缺陷史（已修复，仅作背景）

- 归档仅落盘会话报 `Session not found`（2026-09-09 用户报告，Phase 1 修复）：根因是归档被建模为"收起"，而收起只作用于运行中的会话。

## 5. TS 引擎现存差距（2026-09-09 需求复审复核）

需求文档绑定终态；以下为两代 TS 引擎**当前实现**与终态承诺的差距（若 v1/v2 继续服役需排期对齐，入 ROADMAP）：

| # | 差距 | 违反的终态承诺 | 复核状态 |
|---|---|---|---|
| 1 | v1 会话清单只追加不清理：被删会话的记录永留，列表每次现场核对跳过 | US7.2（自愈不留残留） | 已核实（`session-index.ts` 纯 `appendFile`，无删除路径） |
| 2 | v1 清单丢失后的重建未接线：重建代码只在测试中被触发 | US7.1（删清单重启自动重建） | 未找到非测试调用点，可信；待精确举证 |
| 3 | v1 无锁并发：代码注释自认"接受写入竞态" | US4.1（并发不阻塞不损坏） | 注释可信，未逐条复核 |
| 4 | 两代 TS 均无旧数据自动归置 | US5.3 | 已核实（与 §2 矩阵一致；Rust Phase 2 已实现） |
| 5 | v2 定时任务存工作区层（`bootstrap.scope('cron')` + `<workspaceId>/<id>.json`）：删会话不带走、复制却带走——同一资产两个操作归属矛盾 | SDR-001（定时任务 = 会话资产） | 存储位置已核实；复制带走未逐行复核 |
| 6 | v2 创建失败回滚**已实现**（fork 的 targetDir 失败清理） | US1.3（正面参照） | 已核实（`sessionLifecycleService.ts` fork 错误路径） |

两项原"待举证"声明**已举证并核实（2026-09-09 二次复核）**：

- **收起时后台任务可配置继续跑：属实（v1、v2 都有配置面）**。配置键 `background.keepAliveOnExit`（v1 `config/schema.ts`、v2 `agent/task/configSection.ts` 的 task 段 + legacy background 段），环境变量 `MIRRICODE_BACKGROUND_KEEP_ALIVE_ON_EXIT`，解析顺序环境变量 → 配置 → 默认 `false`。v1 收起路径 `stopBackgroundTasksOnExit()`：keepAlive 为 true 时直接返回（任务继续跑），否则逐 agent `stopAll`（真停止）。配套 `printBackgroundMode: exit | drain | steer`（退出前等待后台任务收尾/保持存活驱动新回合）。
- **关闭时等待回复收尾约 8 秒：属实（仅 v1）**。`ACTIVE_TURN_CLOSE_TIMEOUT_MS = 8_000`（v1 `session/index.ts:148`）——严格语义是**先取消进行中的回合，再给最多 8 秒的收尾沉降窗口**，超时告警继续关闭。v2 无此机制（close 只有逐 agent 移除）。

**裁决影响**：此前对子代理域确认项 4 的回签（"对齐 TS 全部真停止、不加配置面"）作废，修正为"默认真停止 + `keepAliveOnExit` 可配置继续跑"；8 秒有界收尾窗口是 v1 独有语义、Rust 侧缺失，作为关闭行为对齐项记录。

## 6. Rust 侧待加固项（2026-09-09 复核）

- `SessionId::new`（`scope/ids.rs:20`）无合法性校验，`create_session_with_id` 接受任意字符串直接拼路径——违反 US9.1，需在存储层入口加校验（引擎自生成 id 天然安全，风险面在外部指定 id 的接口）。
- `update_metadata`（`session_scope.rs:437-441`）无条件刷新 `updated_at`——"改名跳顶"的真实来源，违反会话管理域 SDR-004，需拆分排序用活跃时间与元数据更新时间。
- `session.createChild` 未建模父子关系（mirri-proto 登记行自注"parent linkage not modeled"）——违反会话管理域 US5.2，补齐项。

## 7. 附件存储现状（2026-09-09 复核，ADR-007 立项依据）

三套实现的附件（媒体 blob）全部存在**会话域内**，无全局存储、无跨会话去重、无引用感知回收：

| 实现 | 存储位置 | 去重范围 | 回收/清理 |
|---|---|---|---|
| TS v1（agent-core） | `<agent 目录>/blobs/<sha256>`（`agent/index.ts:201`，sha256 + `wx` 排他写幂等） | 单 agent 目录内 | 随会话目录删除；无独立回收 |
| TS v2（agent-core-v2） | `<agent 目录>/blobs/<sha256>`（`agentBlobServiceImpl.ts:6`，对齐 v1 布局） | 单 agent 目录内 | 同上；**fork 拷贝会话目录时连本体复制**（`sessionLifecycleService.ts` 的 `copySessionFiles`） |
| Rust | `<会话目录>/blobs/<sha256>`（`blob.rs`：`{home_dir}/blobs/{sha256}`，构造传 session_dir，`manager/restore.rs`） | 单会话目录内 | 随会话目录删除；无独立回收 |
| TS v1 媒体原文件 | `<会话目录>/media-originals/`（`tools/support/image-originals.ts`，sha256 内容寻址） | 单会话目录内 | 显式注释"owned by the session, cleaned up with it" + 1 GiB 容量上限 oldest-first 清扫 |

共同点：内容寻址（sha256 指纹、`blobref:<mime>;<sha256>` 引用只记指纹与格式）已是既成事实——ADR-007 改的只是存储根上移到全局。均**无**跨会话引用保护（删除源会话可能带走分叉会话仍在引用的附件）、**无** 30 天老化窗口、**无**启动/后台的引用感知清理（全库检索无对应实现）。

## 8. Rust v3 会话运行审计：11 项确认问题（2026-09-13，已全部处置）

对 Rust 新内核（v3_sessions 布局）下一次真实会话（`session-6b03e323`）及其与 TS 内核行为/格式对照的审计，共确认 11 个问题（P0 × 1、P1 × 5、P2 × 5）。完整现象、代码依据与复现方式见审计报告：`docs/specs/reports/rust-v3-session-6b03e323-audit.md`；修复实施见 `plan/rust-v3-session-audit-fixes.md`。

**处置状态（2026-09-13 同日）**：9 项代码修复 + 2 项按设计记录（P1-5、P2-3），全部落地并经全 workspace 测试、真实进程 API 验收与浏览器 web UI 体验（含权限审批流）验证；逐条处置映射与提交号见审计报告「六、处置结果」。下表保留审计时点的现状事实与决策归属。

| 编号 | 问题（现状事实） | 处置决策（记录于设计文档） |
|---|---|---|
| P0-1 | Bash 工具在服务器进程 cwd 执行而非会话 workDir（`ls` 返回引擎源码目录内容，对话内不可察觉）；Glob/Grep 默认根同病 | 工具默认 cwd/根 = 会话 workDir，显式入参可覆盖（对齐 TS `effectiveCwd = args.cwd ?? this.cwd`）；设计节见 `plan/rust-tool-calling.md` 追加设计节 |
| P1-1 | `updated_at` 会话创建后从不刷新（turn 结束不推进，索引排序失真） | 回合结束统一 bump + 落盘 + 索引写通 → ADR-009 |
| P1-2 | `last_prompt` 索引有值而 state.json 为 null；Rust 写盘（load-mutate-save）会抹掉该字段 | `last_prompt` 纳入 scope 层 `SessionMetadata` → ADR-009 |
| P1-3 | TS 老会话 state.json 的 `cwd` 键 Rust 读不进来（serde alias 只有 `workDir`），迁移后 workspace 归属丢失 | §4.1 既有并集/兼容读契约内的修复：serde 增加 `cwd` 别名 |
| P1-4 | `usage.record` 字段拼写与 TS 完全不兼容；隐式缓存的 creation/read 账目呈表面矛盾 | 磁盘拼写保持 Rust 自有、客户端口径由投影层负责、隐式缓存语义显式化 → ADR-012 |
| P1-5 | jsonl（TS v1）与 SQLite（Rust）双索引并存，不同入口看到的会话集合分裂 | 维持共存至 TS 退役，迁移后靠对账/磁盘扫描兜底 → ADR-015 |
| P2-1 | wire 事件全部缺 `time` 字段（仅 metadata 有 `created_at`），重放/审计失去事件级时序 | 追加层统一注入 `time`，反序列化天然兼容 → ADR-011 |
| P2-2 | `tool.result` 的 text 为双重编码 JSON（`{exitCode, stdout, …}` 嵌套转义），渲染显示 raw JSON | 弃用 JSON 包装，改 TS 纯文本格式 → ADR-013 |
| P2-3 | 同一份工具输出存三份（output.log / task JSON / wire.jsonl） | 三层保留（各司其职：实时尾随/任务恢复/对话重放）→ ADR-014 |
| P2-4 | state.json 缺 `version` 字段，格式代际无文件内锚点 | Rust 写盘补 `version: 3`，读取容忍缺失 → ADR-010 |
| P2-5 | 模型汇总漏项、回复语言与用户语言不匹配（非引擎 bug，观察项） | 语言跟随指令做 system prompt parity 核对；模型漏项记录观察不修（见审计报告 §二 P2-5） |

## 9. 恢复路径性能现状（2026-10-15 调查，SDR-005 / ADR-017 立项依据）

用户报告重开大会话秒级以上等待 + 恢复瞬间进程内存峰值 25.8GB（38GB 机器）。完整调查（实测、探针计时、逐代码行核验、双 reviewer 交叉审查）见 `docs/specs/reports/rust-v3-session-cold-open-perf-20261015.md`。核心事实（file:line 以调查时点为准）：

| # | 现状事实 | 位置 |
|---|---|---|
| 1 | 折叠采用每条记录先整体深复制折叠状态再应用的纯函数式实现——成本随记录数超线性 | `rust/crates/mirri-proto/src/reducer.rs:101`（`state.clone()` 在 match 之前无条件执行） |
| 2 | 一次冷重开中主日志被重放两遍（第二遍仅提取转向缓冲字段）；`replay_agent_state_opt` 无 live 短路。两遍均为完整折叠**当且仅当无有效检查点**；有检查点时退化为"快照 + 尾部增量折叠"，但**全量解析仍重复两遍** | `rust/crates/mirri-core/src/session/manager/restore.rs:184/669`、`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1127` |
| 3 | 每个子代理记录流被完整折叠两遍（第二遍同样仅提取转向缓冲），54 个子代理串行 | `restore.rs:1231`（restore_subagents）、`session_handlers.rs:1201`（replayed_pending_agent_steers）、`restore.rs:699` |
| 4 | 检查点生产写入点唯一（收起时），被 kill/仍在运行的会话永远无检查点 | `rust/crates/mirri-core/src/session/manager/mod.rs:668`（snapshot_session_on_close） |
| 5 | 记录流加载的全量数据先后驻留三份（原文行 → 解析树 → 强类型记录，峰值两份共存），另有借用签名导致的整条记录深复制 | `rust/crates/mirri-persistence/src/migration.rs:654`、`append_log.rs:275`、`restore.rs:913`（`record.clone()`） |
| 6 | 恢复按需触发面：约 17 个 RPC 调用点对冷会话同步等待完整恢复（含交互/写向调用；命中 live 会话本身毫秒级，慢在恢复） | `rust/crates/mirri-server/src/rpc.rs:649`（live_or_restored_session，调用点 :729/:943/:1036/:1115/:1278/:1331/:1719/:1860/:2089/:2759/:3011/:3142/:3393/:3633/:3912/:4124/:4385） |

对照 ADR-006 已裁决的恢复时序（注册表先行 → 主 agent 记录流为就绪门槛 → 子代理**懒**重放），现状事实 3 属实现违背既定设计；事实 1/2/5 为 SDR-005 新增验收覆盖；事实 4/6 为 ROADMAP 开放项（检查点频率提高、恢复不阻塞读面——后者涉 API 契约，候选单独立项）。
