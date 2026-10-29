# 会话一致性 — 现状盘点（context）

> 快照：2026-10-13 · 本文件只陈述代码事实与缺陷清单，不含目标与决策（那些在 [session-consistency-requirements.md](session-consistency-requirements.md) 与 [session-consistency-architecture.md](session-consistency-architecture.md)）；设计文档单向引用本文件。
> 调研方式：对 `rust/crates` 与 `apps/mirri-desktop` 的只读代码走查（2026-10-13，双 Rust server 并存场景专项调研）。

## 1. 部署形态（代码事实）

- web 与 desktop 启动的是**同一个 `mirri` 二进制**的两个进程：desktop 经 `apps/mirri-desktop/src/main/ensure-server.ts` detached spawn rust 二进制（端口 58827），web 为 `mirri web` 前台进程（默认端口 58627）。
- 两者共享同一 `MIRRICODE_HOME`（`~/.mirri-code`）：同一 `workspaces.json`、同一 `session-index.db`、同一 `server.token`、同一会话目录树（`v3_sessions/<工作区桶>/<会话 id>/`，ADR-005-R1 切根后）。
- 两进程间无互相发现：desktop 只探测自己的端口；Rust 侧不存在实例注册表（TS 侧 `server/instances/` 是 0.28.0 前旧版残留，Rust 引擎不读不写）。
- 唯一带锁的跨进程协调点是 `workspaces.json` 的注册表锁（`rust/crates/mirri-session-index/src/registry.rs:228-369`，锁文件 + 新建独占 + 10 秒过期接管 + 2 秒等待窗）。

## 2. 缺陷清单（双 Rust server 并存下）

1. **运行状态纯进程内存**：活跃会话表、回合执行、活动追踪、转向缓冲等全部是 `EmbeddedServer`（`rust/crates/mirri-embedded/src/embedded_server/mod.rs:931-1057`）的进程内 HashMap；`state.json` 与索引行中的状态字段是静态列（固定 Closed），不携带"谁在跑"语义。→ B 进程对 A 进程的活动完全不可见（对应 US C1.2/C1.3）。
2. **无会话级互斥**：两进程可同时恢复同一会话（`rust/crates/mirri-server/src/rpc.rs` 的 session.resume/restore），各自持有独立文件句柄追加写入（对应 US C2.1/C2.2）。
3. **对账每进程一次、无锁、锚定磁盘**：`reconcile_session_index_once`（`rust/crates/mirri-embedded/src/embedded_server/session_index.rs:187-303`）以 `state.json` 的修改时刻为增量锚，进程首 RPC 时各跑一次；孤儿清理只在两进程注册同一工作区桶时才会互相删行，但行级互相回滚（A 内存态未落盘时被 B 的磁盘重读覆盖）在任何共享桶上都发生（对应 US C3.2）。
4. **SQLite 双写静默降级**：写失败仅告警并回退磁盘扫描（同上文件 `:400-406`）；忙等超时 5 秒（`rust/crates/mirri-session-index/src/lib.rs:209-242`，WAL + busy_timeout）。双进程双写队列排队，超时被吞（对应 US C3.2）。
5. **state.json 最后写者整份覆盖**：原子写原语（`rust/crates/mirri-persistence/src/fs_utils.rs:39-108`）保证不撕裂但无合并语义；两进程的元数据写互相覆盖且互相 bump 对方的增量锚（mtime 乒乓）（对应 US C3.1）。
6. **wire.jsonl 双流交错**：追加模式（O_APPEND + sync_all，`rust/crates/mirri-persistence/src/append_log.rs`）字节安全，但两进程各自重放后向同一日志追加，历史互相污染（对应 US C2.2 的必要性）。
7. **"对已打开运行中会话的持续增量吸收"不存在**：重放仅发生在恢复时刻（`rust/crates/mirri-core/src/session/replay.rs`）；运行中会话的外部新增记录无吸收路径（对应 US C1.2 的实现缺口）。
8. **无只读打开模式、无不经服务的恢复直连**：会话生命周期只有惰性创建/恢复（可写）/归档三种；只读日志打开仅用于内部重放（`append_log.rs:181-187`）。

## 3. 可复用基础（代码事实）

- **锁原语模式**：`workspaces.json.lock` 的完整实现（过期接管/等待窗/重试）可直接抽出复用（registry.rs）。
- **原子写**：`write_file_atomically`（fs_utils.rs）——临时文件名含 pid + 纳秒，并发写者天然不互踩。
- **追加日志**：`JsonlAppendLog` 的 O_APPEND + fsync + 末行撕裂容忍——跨进程追加与增量读取的基础。
- **存活双验证榜样**：desktop 的 pid 存活 + 健康端点双确认（`ensure-server.ts:79-111, 494-573`）。
- **进程身份**：`server_id`（每次进程新 UUID，`rust/crates/mirri-server/src/state.rs:138`）已经元数据端点暴露，可作为提示文案中"另一入口"归因的现成来源（若未来需要）。

## 4. 已知相关工作

- `plan/rust-session-storage-upgrade.md`（2026-09-09）：目录布局收敛（平铺 → 工作区桶）与 SQLite 索引，**不含多进程并发治理**。
- `docs/specs/session-persistence-*` 域：单进程崩溃安全与索引设计，其"磁盘是唯一事实源、缓存故障不升级"原则被本域继承；本域补的是"多入口并发"维度。
