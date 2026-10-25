# 会话归档存储管理 — 现状盘点（context）

> 快照日期：2026-10-23 ·
> 本文只记录现状事实与痛点，不含目标与决策（目标见 [session-retention-requirements.md](session-retention-requirements.md)；设计见 [session-retention-architecture.md](session-retention-architecture.md)）。

## 1. 会话存储现状（代码事实）

> 以下为 2026-10-23 的代码事实，随代码演进会漂移；引用以当时代码为准。

- **存储根**：`$HOME/.mirri-code/v3_sessions/`（`MIRRICODE_HOME` 可覆盖；TS 老根 `sessions/` 兼容读取）。常量与装配见 `rust/crates/mirri-cli/src/commands.rs`（`V3_SESSIONS_DIR`）与 `rust/crates/mirri-cli/src/main.rs`。
- **会话目录三件套**（`rust/crates/mirri-core/src/session/persistence.rs` 模块文档）：`state.json`（元数据，原子写）、`wire.jsonl`（追加式操作日志）、`snapshot.json`（可选检查点）。子代理日志在 `<session_dir>/agents/<agent_id>/wire.jsonl`。
- **附件存储**：`{home}/blobs/`，内容寻址（sha256）、跨会话共享去重，由 `rust/crates/mirri-core/src/blob.rs` 的 `AgentBlobService` 管理；会话日志里只存 `blobref:` 引用。**删除会话目录不释放 blobs**。
- **会话索引**：SQLite（`rust/crates/mirri-session-index/src/lib.rs`），行从磁盘派生、可重建。

## 2. 归档现状

- `archived` 是 `state.json` 布尔字段（`PersistedSessionMetadata`）+ SQLite 索引列 + 内存视图 `SessionInfo` 的标志位；**没有 `archived_at` 时间戳**——"归档了多久"当前无法从数据得知。
- 归档/恢复动作：`SessionPersistence::set_archived()` 翻转标记；`mirri-embedded` 的 `dispatch_archive_session` / `dispatch_restore_session`（`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs`）。
- 默认列表/计数排除归档会话（`SessionIndex::list_sessions` SQL `archived = 0`、`count_active`）。

## 3. 清理机制现状：无

- 搜索 retention / purge / prune / cleanup / gc / compact / compress / reclaim 后确认：**没有任何会话磁盘占用回收机制**。sessions 只增不减（除非手动删除：`SessionPersistence::delete_session_files()` + `SessionIndex::delete_session()`，均为显式动作）。
- 既有 "retention/compaction" 命中全部是 **LLM 上下文压缩**（`ContextApplyCompaction` 等），与磁盘无关——同名异义的高危混淆点。
- 孤儿治理任务（`rust/crates/mirri-embedded/src/embedded_server/orphan_governance.rs`）清理的是**运行态残留**（进程状态），不回收磁盘，但其"接线层常驻任务"的装配模式是本域清理任务的直接参照。
- blobs/ 无任何 GC（跨会话共享 + 无引用计数 = 永久增长）。

## 4. 空间占用构成（按典型大小排序）

1. `wire.jsonl`：完整 LLM 请求/响应 + 工具输出全量记录，长会话的主要占用（JSONL 文本，gzip 友好）。
2. `blobs/`：base64 图片等附件的原始字节（全局共享，重度图片用户的主要占用）。
3. `snapshot.json`：折叠状态检查点，随对话增长。

引擎已能按会话读取 `wire_log_size`（`PersistedSessionInfo`），是现成的度量。

## 5. 会话删除的影响面（谁在读旧会话）

| 消费方 | 位置（当时代码） | 删除后的表现 |
|---|---|---|
| 会话恢复（S1） | `rust/crates/mirri-core/src/session/manager/`（restore 重放） | 无法恢复 |
| 列表/搜索 | `mirri-session-index` + `mirri-embedded` 合并视图 | 从列表消失 |
| 回放/可视化 | `rust/crates/mirri-core/src/session/replay.rs` | 无法回放 |
| 派生（fork） | `rust/crates/mirri-core/src/session/fork_pipeline.rs` | 无源可 fork |
| 导出 | `rust/crates/mirri-core/src/session_export.rs` | 无法导出 |
| 迁移 | `rust/crates/mirri-cli/src/migrate.rs` | 迁移对象减少 |
| ACP | `rust/crates/mirri-acp/src/acp_handler.rs` | session/load 不存在 |

已知连带问题（本域设计的直接动因）：
- 删除会话若不同步删索引行 → 磁盘与索引不一致（靠"磁盘是事实源 + 自愈"兜底，但仍需正确顺序）。
- 删除会话不回收 blobs → "删了没降"。

## 6. 配置系统现状

- 主配置 `ServerConfig`（`rust/crates/mirri-core/src/config/mod.rs`），TOML 位于 `<mirri_home>/config.toml`；各段为 `Option<Section>` + `#[serde(default)]` 模式（`TaskConfig`/`SubagentConfig` 为最近范例）。
- 新顶层段需登记 `KNOWN_CONFIG_TOP_LEVEL_KEYS` 白名单，并注意 wire 层 round-trip（`rust/crates/mirri-core/src/config/wire.rs`）。
- 当前无任何 retention 相关配置。

## 7. 并发现状（与本域的约束来源）

- "SC1 另一入口"（会话一致性域）：桌面 + 终端 + 网页服务共享同一份个人数据目录是多进程常态场景。
- 持久化域已有原子写原语（临时文件 → fsync → 原子改名）与"索引是缓存、磁盘是事实源"的自愈原则——本域的崩溃安全直接复用。
- 尚无会话目录级的跨进程文件锁设施；如需目录级互斥需确认既有机制或新增（设计 §5 的"探测-确认窗口"落点）。
