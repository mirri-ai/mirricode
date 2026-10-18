# Rust core 会话冷启动慢 + 25.8GB 内存峰值问题分析

- **日期**: 2026-10-15（rev3：F4 经用户决策定为「子代理惰性恢复」，与 F2 合并批次）
- **触发**: 用户报告「打开超大会话很慢」（例：`session-93226585-16f6-445f-bc4b-3bd4483352d2`、`session-dc31588f-5e07-4c51-8227-08eb6c6c9619`）；随后又发现 `mirri web` 进程 RSS 高达 16.5GB、`phys_footprint` 峰值 25.8GB。
- **数据源**: 实测进程 vmmap / WS RPC 探针计时 / 会话 wire.jsonl 统计 / mirri-core、mirri-proto、mirri-embedded、mirri-server、mirri-web 源码。
- **状态**: 根因已定位到代码级并经双 reviewer 代码核验，**未修复**（待决策）。两个问题（打开慢、内存峰值）同源。

## TL;DR

打开会话慢与 25.8GB 内存峰值是**同一根因的两个症状**：

1. **Reducer 逐 op 深克隆 AgentState（O(N²)）**——`rust/crates/mirri-proto/src/reducer.rs:101` 的 `reduce_with_limits` 每处理一条 wire op 就 `state.clone()` 整个状态（含全部历史消息与 loop events），无论该 op 是否增长状态。
2. **一次冷恢复中，主日志被重放 2 遍、54 个子代理 journal 各被重放 2 遍**（后者是 rev1 遗漏、rev2 补入的重放源）。
3. **快照检查点几乎从不生效**——只有优雅关闭才写 snapshot，且快照只含主 agent 状态、不含子代理；崩溃/被 kill 的会话冷打开必须全量重放。

三者叠加得到：打开慢（秒级～十秒级阻塞 RPC）、瞬时内存分配达数十 GB 量级（用完即释放，留下 16GB 的 malloc 空洞 RSS 假象）。

## 一、实测证据

### 1.1 进程内存形态（vmmap，PID 8808，2026-10-14 23:47 采样）

| 指标 | 值 | 含义 |
|---|---|---|
| Physical footprint（当前真实占用） | **370MB** | 进程此刻真正持有的脏内存 |
| Physical footprint (**peak**) | **25.8GB** | 进程生命周期内真实达到过的峰值 |
| RSS | 16.5GB（43.7% 全机） | 大部分是已释放、内核未回收的 malloc 页 |
| MALLOC_SMALL (empty) | **22.4GB / 5743 个 region** | 瞬时海量小对象分配后整体释放的指纹 |
| Dirty（真实在用） | 223.8MB | 无泄漏——分配全部归还了应用层 |

结论：**不是泄漏，是一次性病态放大**。峰值发生在 23:11:57（启动）→ 23:42（首次采样）之间，即服务冷启动/冷恢复窗口。

### 1.2 会话实际规模（磁盘上并不大）

`session-93226585`（用户报告慢的会话）：

| 项 | 规模 |
|---|---|
| 主 wire.jsonl | 4768 ops / 1.83MB<br>・`goal.update` 2233 条（不增长状态，但仍付全额克隆成本）<br>・`context.append_loop_event` 1848 条 / 1.27MB<br>・`context.append_message` 87 条 / 160KB |
| 子代理 journal | **54 个**，共 28627 行 / ~27MB<br>（单 journal 平均 ~530 行 / ~459KB，最大 6.0MB） |
| snapshot.json | **不存在** |

`session-dc31588f`：849 行 / 1.11MB，同样无 snapshot.json。全库最大的主 wire.jsonl 为 3.37MB，最大子代理 journal 6.0MB——**数据量本身不足以解释任何 GB 级行为，问题出在代码路径的放大系数**。

### 1.3 WS RPC 探针计时（2026-10-15，针对冷会话）

对 93226585 实测（当时该会话已被前序操作恢复为 live）：

| 调用 | 耗时 | 说明 |
|---|---|---|
| `session.get` | 0.00s / 3KB | 纯元数据，快 |
| `session.snapshot`（live 命中） | 0.04s / 1.4MB | live 会话直接读内存，快 |
| `message.list` | 0.01s / 38KB | 快 |

即：**慢的从来不是读面，而是冷打开时触发的「按需恢复（restore-on-demand）」**——`live_or_restored_session`（`rust/crates/mirri-server/src/rpc.rs:649-667`，约 16 个 RPC 调用点）发现会话非 live 时，同步等待完整 resume（重放 + 子代理恢复 + steer 恢复）才返回。UI 上的表现就是「打开超大会话时白屏/转圈 N 秒」。

## 二、根因链（代码级，经 reviewer 核验）

### R1（主因）: reducer 每 op 深克隆整个 AgentState —— O(N²)

`rust/crates/mirri-proto/src/reducer.rs:101`：

```rust
pub fn reduce_with_limits(state: &AgentState, op: &WireOp, ...) -> (AgentState, Vec<DomainEvent>) {
    let mut new_state = state.clone();   // ← 每条 op 全量深克隆，无任何条件守卫
    ...
}
```

- `AgentState.conversation`（`rust/crates/mirri-proto/src/state.rs:162-172`）持有 `messages: Vec<serde_json::Value>` 与 `loop_events: Vec<serde_json::Value>`——`serde_json::Value` 的 Clone 是逐节点递归深拷贝；
- 克隆发生在 `match op` **之前**：`Metadata`、`goal.update`、`usage.record` 等不增长状态的 op 同样付全额克隆成本（93226585 的 4768 条 op 中 2668 条属于此类）；
- reviewer 核验：reducer 全部 arm 只在 `new_state` 上操作、从不回读旧 `state`；生产调用方共 5 处，**全部丢弃旧 state 与 events**（`restore.rs:878`、`restore.rs:725`、`restore.rs:1274`、`mod.rs:1005`、`mod.rs:675`）——纯函数签名是纯开销。

恢复一条 N-op 会话的克隆流量 ≈ `sum(第 i 条 op 时的状态原始字节数)`。以 93226585 主日志精确计算（只有 append_message/append_loop_event 增长状态）：**原始克隆流量 ≈ 4.15GB**；serde_json Value 树内存膨胀按 2-3× 计，名义分配约 8-12GB。

### R2: 一次冷 resume = 主日志 fold 2 遍 + 每个子代理 journal fold 2 遍

冷恢复路径的完整调用序列（`dispatch_resume_session`，`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:1004`）：

1. **主日志第 1 遍**：`resume_session_with_scope` → `land_resume_tail` → `replay_agent_state_opt(session_id, true, limits)`（`rust/crates/mirri-core/src/session/manager/restore.rs:184-186`）——无快照时从 op 0 全量 fold；
2. **主日志第 2 遍**：resume 返回后 `session_handlers.rs:1127-1129` 无条件调 `replayed_pending_steers` → 内部再跑一次 `replay_agent_state_opt`（restore.rs:669），**只为取 `state.steer_pending` 一个字段**。reviewer 核验：`replay_agent_state_opt` **没有 live-session 短路**（有该检查的是 `read_only_histories`），resume 路径上第 2 遍是全量重放（快照存在时退化为快照反序列化+尾部增量，近乎免费——但本案例无快照）；
3. **子代理第 1 遍**：`restore_subagents`（restore.rs:1231-1358）对 54 个 `agents/*/wire.jsonl` **逐个串行**执行 `migrate_records_in_memory` + 逐条 fold，且每个 child 把 ops 再累积进 `Vec<WireOp>` 双份共存；
4. **子代理第 2 遍**（rev2 补入）：`session_handlers.rs:1201-1226` 对**每个**已恢复子代理再调 `replayed_pending_agent_steers`（restore.rs:699-743），**每个 journal 重新 `migrate_records_in_memory` + 全量 fold 一遍**，同样只为取 `steer_pending`。

即总重放次数 = 主 ×2 + 子代理 ×2。

### R3: snapshot 检查点只在优雅关闭时写 —— 冷会话几乎永远无检查点

- 全仓 `SnapshotStore::write` 生产调用点唯一：`create_snapshot`（mod.rs:634）← `snapshot_session_on_close`（mod.rs:668）← 仅 `close_session`（mod.rs:1325/:1388）触发；其余全在测试。`snapshot.rs:36` 的文档自述「periodically call write after N ops」——周期快照是设计内但未实现；
- 服务器被 kill、桌面端退出、或会话还在运行时，snapshot.json 不存在；实测两个慢会话目录均无 snapshot.json；
- **快照只含主 agent 的 `AgentState`，不捕获任何子代理状态**（`rust/crates/mirri-persistence/src/snapshot.rs:28`）——`restore_subagents` 永远从 journal 全量 fold；
- `replay_records_blocking`（restore.rs:819-868）在快照缺失/stale 时从 op 0 全量 fold。另注意：即使有快照，`wire_log_max_turn_id` / `duplicate_turn_ids` 仍需**全量 parse** 日志（`migrated_records_to_wire_ops` 先行）——快照只省 fold、不省 parse。

### R4: `migrate_records_in_memory` 多份全量中间拷贝

`rust/crates/mirri-persistence/src/migration.rs:654`：`read_raw_lines`（`read_to_string` 全文 + 每行 `to_string`，`append_log.rs:275-282`）→ `Vec<Value>` → `Vec<WireOp>`，同一数据多份共存。rev2 补入第 4 份：`migrated_records_to_wire_ops`（`restore.rs:913`）对每条记录 `serde_json::from_value(record.clone())`——`record.clone()` 是全深拷贝，只因函数签名借用了 `&[Value]`。

### 内存峰值归因（rev2 修正口径）

25.8GB 是 **phys_footprint 峰值（同时驻留）**，而上述放大项产生的是**累计分配流量**（串行执行会摊平驻留）——两者口径不同，精确对账需要 heap profile。量级判断如下：

| 来源 | 累计原始字节流量 | Value 树膨胀后名义流量 |
|---|---|---|
| 主日志 fold ×2（R1×R2） | ~8.3GB | ~17-25GB |
| 子代理 journal fold ×2 | ~37GB（54 journal 各自累计） | 名义更大，但串行摊平，驻留贡献 ≈ 最大单 journal 峰值 ×2 |
| R4 多份拷贝 | ~3× 数据量 | — |

**结论：GB 级瞬时分配、O(N²) 主导，成立；「精确对账 25.8GB」因主日志估算口径（终态 vs 平均态）与子代理聚合口径（累计 vs 驻留）的两处偏差部分抵消，不可作为证据使用。** 修复后应以 `vmmap --summary` 峰值实测重新归因。

## 三、影响面

- **用户可见**：冷打开大/多子代理会话慢（秒级～十秒级阻塞 RPC）；打开瞬间系统内存压力尖峰（38GB 机器上 25.8GB，足以把别的进程挤进 swap——与 2026-10-14 晚「服务卡、RPC 无响应」的体感吻合）；
- **波及路径**（凡是走 fold 的都放大）：resume / restore-on-demand / fork 内存重建 / `read_only_histories`（session.search）/ `read_only_turn_usages` / 子代理恢复 ×2 / 关闭时快照（`snapshot_session_on_close` 自身跑一遍 O(N²) fold——关大会话同样慢）；
- **不受影响**：live 会话的 `session.get` / `session.snapshot` / `message.list` 读面（实测毫秒级；次级细节：live 的 snapshot/message.list 仍会经 `turn_usage_read_face` 从磁盘重读 wire.jsonl 做 parse-only 扫描，非零但快）；事件通道（`EVENT_LOG_CAPACITY=256` 有界）、任务输出（1MB cap）。

## 四、优化方案（rev2：按双审结论修订优先级）

| # | 方案 | 改动点 | 预期收益 | 风险 |
|---|---|---|---|---|
| F1 | **reducer 就地 fold，保留 `reduce` 薄包装**：新增 `apply(&mut AgentState, op)`；`reduce` 改为 `clone+apply` 包装（150+ 测试零改动）；只迁移 5 处生产 fold 循环（restore.rs ×3 / mod.rs fork / snapshot_on_close） | `mirri-proto/reducer.rs` + 5 个循环 | 消除 O(N²)：冷打开 CPU/内存双降一个数量级以上；**免费红利**：`snapshot_session_on_close`（关大会话慢）同步修复 | 中——语义已核验安全（无 arm 回读旧 state、无调用方依赖纯度；TS 无对应纯函数契约，零 parity 债） |
| F2 | **消除全部重复重放（含 child 双 fold）**：resume 内缓存第一遍 fold 的 state/`steer_pending`（挂 session artifacts 或 manager 短暂缓存），`replayed_pending_steers` 与 54 个 `replayed_pending_agent_steers` 优先读缓存——主日志 2→1、每个子代理 journal 2→1 | `restore.rs` + `session_handlers.rs:1127/1201` | **仅次于 F1 的第二大收益项**（rev2 升格：child 双 fold 是 rev1 对账缺口的来源） | 低——调用方重构，行为不变 |
| F5 | **流式化 + 消除隐藏拷贝**：`migrate_records_in_memory` 改 BufRead 逐行；`migrated_records_to_wire_ops` 按值消费（`from_value(record)` 去 `restore.rs:913` 的 `record.clone()`）。注意 `read_raw_lines` 有 8 个调用方 | `mirri-persistence/migration.rs` + `append_log.rs` + `restore.rs` | 峰值内存 3×→1×；纯机械、零语义风险 | 低 |
| F3 | **（backlog）turn 边界/阈值快照**：设计要点——需先引入 live 状态增量维护机制（当前 live 路径只维护 context memory，不维护 fold state，`snapshot_session_on_close` 不得不重放全量即因此）；写入应阈值触发（新增 ops/bytes > N）且走 blocking pool；**快照格式不含子代理状态，对多子代理会话收益有限** | manager 新增 turn-settle 写入点 + live-state 维护 | F1+F2+F5 落地后边际收益毫秒级；为未来百 MB 级日志预留 | 中——真实工作量在 live-state 维护，非「加一个写入点」 |
| F4 | **子代理惰性恢复（rev3 定稿）**：resume 时不 fold 任何子代理 journal，只做三件廉价事——①`seed_subagent_registry` 建 roster（数据源为主日志的 `subagent.registered`）；②轻量扫每个 journal 的 `turn.*` 记录取 `child_max_turn_id`（防 duplicate tripwire 的硬性保留项，`restore.rs:196`）；③派发 approval 通道（ADR-009）。子代理上下文在**首次被唤醒**（steer/继续对话）时按需 fold 该 journal——此时即全量上下文，无失忆。前端详情面板同理按需读。架构依据：主 wire 已完整记录委派内容（`tool.call`）与子代理最终报告（`tool.result` 全文），主 agent 的上下文自洽，不需要子代理过程记录；54 个子代理 resume 后被唤醒的实际利用率极低 | `restore_subagents`（restore.rs:1231） + `session_handlers.rs:1201` 的 steer 重公园改走按需路径 | 与 F2 合并后数据面从「主×2 + 子代理×2」降为「主×1」——93226585 场景读取量 ~30MB→1.8MB（-93%）；唤醒路径多一次单 journal fold（F1 后毫秒级） | 中——需定义"唤醒"触发边界并埋桩；语义无损（区别于被否掉的 roster-only-永不恢复版） |

**rev3 落地顺序**：F5（半天级、纯机械、先行验证测试基线）→ F1（根因，包装策略压缩 blast radius）→ **F2+F4 合并为一个批次**（两者改的是同一片代码：resume 路径的重放编排——F2 消主日志第 2 遍并缓存 state，F4 把子代理从"全量恢复"改为"roster + 按需 fold"，合并实施避免二次返工）。F3 挂 backlog 待数据。

**F4 决策记录（SDR）**：rev2 曾将 F4 标为"需产品决策、风险高（子代理失忆）"。用户质询后核实：主 wire 的 `tool.call`（委派 prompt）与 `tool.result`（子代理最终报告全文）构成主 agent 的自洽上下文，子代理过程记录对主 agent 无用；54 个子代理 resume 后被唤醒的利用率极低。据此否掉"roster-only 永不恢复"（会失忆），定为**惰性恢复**：唤醒时按需 fold 全量 journal，语义无损。决策依据见本文「四、」F4 条目。

**评审补充记录的候选**（不进本期方案）：
- restore-on-demand 同步阻塞 ~16 个 RPC 调用点的 UX 问题：F1-F5 全落地后，超大会话冷打开仍是用户可见等待。候选：`session.get` 先返回 `restoring` 状态元数据 + 恢复完成事件（web 已有事件通道）——改变 API 契约，需单独立项；
- 分配器（jemalloc/mimalloc）：修完 O(N²) 后意义骤降，仅记录备选。

## 五、验证方式（修复后）

1. 复现场景：对 93226585（4768 ops + 54 子代理）冷恢复，断言耗时 < 1s、进程峰值内存 < 500MB；
2. 回归：`cargo test -p mirri-proto -p mirri-core -p mirri-embedded`（reducer 行为等价性——同一 wire log 的 fold 结果与改造前逐字段一致）；
3. 顺带断言：关闭大会话（`snapshot_session_on_close` 路径）的耗时同步下降（F1 免费红利）；
4. 观测：`vmmap --summary` 的 peak footprint 对比（修复前 25.8GB），并以实测峰值重新归因（替代本文 rev1 的估算对账）。

## 附：调查过程存档

- vmmap 采样（PID 8808 / 68163 两个实例均复现 peak 25.8G 特征）；
- WS 探针脚本：`.tmp/ws_probe3.py`、`.tmp/ws_timing2.py`（bearer 子协议 + client_hello 握手后计时）；
- 已排除项：session.list/snapshot/message.list 读面、事件通道（有界 256）、任务输出（1MB cap）、索引 reconcile（只读 mtime）、增量 absorb（锚点读）、泄漏（dirty 恒定 223MB）；
- 双审记录：code-reviewer（事实核验，rev1 的内存对账被判「凑数式吻合」并已修正）、architecture-reviewer（方案评审，F3 降级、F2 扩围、F1 包装策略均为其结论）；
- rev3 决策：用户确认子代理全量恢复不合理（利用率极低），核验主 wire 已含委派+最终报告（`tool.call`/`tool.result`）后，F4 从「需产品决策」定为「惰性恢复」，与 F2 合并实施。

## 附2：正式设计落地（rev4）

本报告为调查/分析件；决策已按 `docs/specs/AGENTS.md` 治理规则写回正式域文档：

- **需求**：`docs/specs/session-persistence-requirements.md` SDR-005（US2.1 验收扩展：从头处理有界性 + 子代理记录不预载）；
- **架构**：`docs/specs/session-persistence-architecture.md` ADR-017（就地折叠/单一折叠结果/流式加载/懒重放执行遵从），§4.4 补降级分支有界性条款；
- **现状事实**：`docs/specs/session-persistence-context.md` §9（六项 file:line 事实与决策归属）；
- **执行**：`docs/specs/session-persistence-ROADMAP.md` 开放项 14（验收标准与落地顺序）；
- **跨域对齐**：`docs/specs/subagent-architecture.md` §3.2.1 恢复重建条目补懒重放遵从引用（语义无变更，对齐持久化域 ADR-006 既有裁决）。

报告 F1–F5 与正式文档的映射：F5→ADR-017 流式加载；F1→ADR-017 就地折叠；F2+F4→ADR-017 单一折叠结果+懒重放；F3（周期快照）与 restore-on-demand UX 为 ROADMAP 独立开放项，不入本期。
