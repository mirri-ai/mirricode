# Mirri Code Rust 服务进程 26GB 内存峰值调查简报（面向外部帮手）

> 写给完全不了解本项目的工程师。你只需要 macOS + Rust + 常识。
> 读这份文档不需要读代码库，但文末给了关键源码路径，如果你想看。

## 1. 项目背景（30 秒）

- **Mirri Code** 是一个 AI 编程助手，多端：CLI（终端）、Web UI、Electron 桌面端。
- 核心引擎正从 TypeScript 迁移到 **Rust**（`rust/crates/`，约 20 个 crate）。桌面端/Web 端共用一个 Rust 后台服务进程（下称 **mirri 服务**）。
- 用户和 AI 的每次对话是一个 **session（会话）**，持久化在磁盘：
  - 每个会话一个目录：`<home>/v3_sessions/<workspace>/<session>/`
  - `wire.jsonl`：追加式事件日志（JSONL，一行一条操作记录）。主代理一个文件；每个子代理（subagent，主代理派生的 helper agent）在 `agents/<id>/wire.jsonl` 各有一份。
  - `snapshot.json`：检查点（fold 后的状态快照），优雅关闭会话时写入；被 kill / 仍在运行的会话没有。
- **打开一个旧会话 = 从磁盘恢复（resume）**：读 wire.jsonl → 逐条 fold 成 `AgentState`（fold = 状态机逐条应用操作，类似 event sourcing 的 reducer）→ 重建对话上下文。

## 2. 问题史（已修的部分）

### 最初症状

用户报告：打开大一点的会话非常慢（十秒级），且 mirri 服务进程内存峰值达 **25.8GB**（38GB 机器），当前 RSS 长期 15GB+。

### 根因调查结论（2026-10-15，已写报告 + 双人代码评审）

四个叠加缺陷，全部在同一恢复路径上：

| # | 缺陷 | 位置（修复前） |
|---|---|---|
| R1 | fold 状态机每应用一条记录就**深克隆整个状态**（含全部历史消息的 JSON 树）——O(N²) | `rust/crates/mirri-proto/src/reducer.rs` |
| R2 | 一次恢复中**主日志被完整 fold 两遍**（第二遍只为取一个 steer 字段）；每个子代理日志也被 **fold 两遍** | `rust/crates/mirri-core/src/session/manager/restore.rs` |
| R3 | 检查点只在优雅关闭时写，被 kill 的会话恢复时永远走全量 fold | 同上 |
| R4 | 日志加载在内存中同时存在多份全量拷贝（整文件字符串 + 逐行拷贝 + 解析树 + 借用导致的逐条深拷贝） | `rust/crates/mirri-persistence/src/migration.rs` |

### 已实施的修复（三批，全部合并 + 回归测试通过）

| 批次 | 内容 | 效果（微基准，同一 7069 条记录的日志） |
|---|---|---|
| F5 | 流式逐行读日志（消灭多份全量共存）；转换函数改按值消费（消灭逐条深拷贝） | 加载内存 -28% |
| F1 | 新增就地 fold 接口 `apply_with_limits(&mut state, op)`；旧纯函数接口保留为薄包装（150+ 测试零改动）；生产代码全部迁移到就地版 | **fold 耗时 18.52s → 9.4ms（约 2000 倍）** |
| F2+F4 | 主日志只 fold 一遍（steer 提取走缓存）；子代理**懒恢复**：恢复时只登记名单 + 扫 turn-id 上界，不 fold 子代理日志；子代理被真正使用时才按需 fold 重建 | 冷恢复不再读子代理日志（除一个轻量扫描） |

修复验证（隔离环境 + 真实数据）：

- 同样 15 个会话、同样顺序、相同数据快照、**确定含修复的二进制**：全部 resume 完成，总耗时 3.6s，**进程峰值 170MB**。
- 单会话冷恢复 0.04-3.6s；数据完整性逐字节一致。

## 3. 当前未解问题（需要你帮分析的）

### 症状

用户把桌面端重新打包（含上述全部修复）后，重启桌面端、连续点击打开约 15 个会话后，mirri 服务进程再次出现 **26GB 物理内存峰值**（macOS `vmmap` 的 `Physical footprint (peak)`）。

### 已排除（都有实测证据）

1. **不是 resume/fold 路径**：同一数据 + 同一批会话 + 确定含修复的二进制，复现不出（峰值 170MB）。连并发打开 15 个也只 170MB。
2. **不是泄漏**：`leaks` 报 0 泄漏；峰值过后活对象仅 ~217MB（108 万个 malloc 节点）。
3. **不是当前内存积压**：峰值后 dirty 页大部分是 malloc 已释放未归还内核的空洞（`MALLOC_SMALL (empty)` 区域 dirty 6.4GB+，5850 个 region）。
4. **不是桌面端二进制过期**：二进制含修复的特征字符串，且行为指纹（快照+增量恢复、agent.list 的懒恢复语义）与本地新构建一致。
5. **不是订阅风暴**：日志里高频 subscribe 是调查者自己的探针，非桌面端行为。
6. **不是事件环形缓冲**：容量 256，有界。

### 内存形态（峰值时采样）

```
Physical footprint (peak):  26.0G      ← 一次性的瞬时分配尖峰
Physical footprint (当前):  8-16G      ← 大部分是 dirty 的 malloc 空洞
MALLOC_SMALL (empty):       22.9G 虚拟 / 15.1G resident / 6.4G dirty, 5850 regions
MALLOC_SMALL (活跃):        1.5G dirty
MALLOC_LARGE:               仅 ~10MB
单个 region dirty 都 <50MB  ← 分配是"海量小对象"，分散在数千个 4MB malloc 区
```

**指纹结论：某段代码在窗口内分配了 ~20GB 的海量小对象（serde_json::Value 之类的树节点特征），然后释放。**

### 时间线（关键约束）

- 23:01:53 桌面端 mirri 服务启动（含全部修复）
- 23:02-23:08 用户打开 ~15 个会话（磁盘总量约 130MB），期间峰值从 3G 冲到 25.8G
- 用户体感：**会话加载本身不慢**（符合修复效果），但"连点之后"内存飙升
- 15 个会话的数据构成：多数 <2MB；最大的 51MB（54 个子代理）；几个 10-17MB；其中一个含 5MB 图片 blob

### 仍可疑的方向（按怀疑度排序）

1. **某个非 resume 的读面在每次打开/渲染时做全量重算**：桌面端 web UI 打开会话时会调 `session.snapshot` / `message.list` / `session.subscribe` 等。我们逐个实测过毫秒级、响应只有 KB-MB 级——但那是"单次调用"的测量。**如果某个调用被高频重复触发**（重连、轮询、resync 循环），小放大 × 高频 = 大峰值。日志证据：`session event stream lag → resync` 的机制存在（客户端落后于事件流时全量重拉快照）。
2. **事件投影/重投影**：日志里峰值窗口出现 33 次 `Projection repair: WhitespaceTextDropped`（context memory 的投影修复），全天 816 次。未深入。
3. **后台任务恢复**：启动时有 `reconciling lost running task`（把重启前 lost 的后台任务对账）。桌面端可能批量恢复多个会话的任务，每个任务的输出/状态被全量载入。
4. **blob 再水化**：恢复会话时把 `blobref:` 媒体（图片等）从磁盘读回内存 `data:` URI。一个会话 5MB 图片 → base64 膨胀 1.33× → 如果多会话/多次重复合并……但仍差几个数量级。
5. **release 构建与 debug 构建的行为差异**：所有"无法复现"的实验用的是 debug 构建。理论不应有 150 倍差异，但未验证 release 本地构建。

### 已知的混淆变量（分析时请注意）

- 桌面端进程是 Electron：主进程 + Renderer + rust 服务是三个进程。26G 在 **rust 服务进程**（PID 见 vmmap 报告）；Renderer 当时 CPU 37%（在渲染）。
- 用户"连点"的确切交互序列未知（点了哪些会话、是否有关闭再打开、是否切换）。
- 日志级别是 `--log-level error`，信息有限；INFO 日志来自共享的 `~/.mirri-code/logs/mirri-server.log`。
- 峰值计数器（peak）是进程生命周期累计，26G 的形成过程没有连续采样（看门狗挂上时峰值已经形成）。

## 4. 我们下一步想做的（你可以帮验证/推翻）

1. **MALLOC_STACK_LOGGING 抓分配栈**（最直接）：`MallocStackLogging=1` 重启 mirri 服务 → 复现连点 → 峰值时 `malloc_history <pid> <address>` 看大分配的调用栈。桌面端重启成本高（打断用户），所以还没做。
2. **连续 footprint 采样 + 事件对齐**：每 0.5s 采样 footprint，与 INFO 日志对齐，找"上升沿发生时服务在干什么"。
3. **复现 release 构建**：本地构建 release profile 的 mirri，用相同数据 + 模拟连点重放。
4. **审计 resync 循环**：读 `rust/crates/mirri-server/src/ws_rpc.rs` 的 event lag → resync 路径，确认 resync 是否可能高频自激（客户端 lag → 重拉快照 → 处理慢 → 再 lag）。

## 5. 关键文件地图（如果你想读代码）

| 关注点 | 文件 |
|---|---|
| fold 状态机（已修好） | `rust/crates/mirri-proto/src/reducer.rs` |
| 恢复编排（已修好） | `rust/crates/mirri-core/src/session/manager/restore.rs` |
| 日志加载（已修好） | `rust/crates/mirri-persistence/src/migration.rs`、`append_log.rs` |
| WS RPC + 事件订阅 + resync | `rust/crates/mirri-server/src/ws_rpc.rs` |
| RPC 读面（snapshot 等，restore-on-demand） | `rust/crates/mirri-server/src/rpc.rs`（`live_or_restored_session`） |
| 恢复装配（embedded 层） | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` |
| 上下文投影/修复 | `rust/crates/mirri-core/src/context_memory/projector.rs` |
| 工作事实影子（新加的事件 fold） | `rust/crates/mirri-core/src/session/activity_shadow.rs` |

历史调查报告（含全部实测数据、双评审记录）：
`docs/specs/reports/rust-v3-session-cold-open-perf-20261015.md`

设计文档（ADR-017 单遍 fold 决策）：`docs/specs/session-persistence-architecture.md`

## 6. 一句话总结

**"打开会话慢"已经修好（2000 倍 fold 提速，实测复现通过）；但桌面端真实使用中仍出现一次 26GB 的瞬时小对象分配尖峰，与 resume 路径无关（隔离复现不出），leaks 干净，指纹是"海量小对象分配后释放"——最可能是某个高频重复触发的读面或恢复后的重算环节，尚未定位。**

---

## 更新（2026-10-16）：帮手建议的核验结果

| 建议 | 结果 |
|---|---|
| B. `live_or_restored_session` single-flight | **有守卫**（resume 入口的 AlreadyExists 检查挡住并发第二次 fold）；并发 15 会话实测峰值 170MB |
| C. 子代理懒恢复被 agent.list 触发全量 fold | **证伪**——agent.list 走 roster 名单，不 fold；完整调用序列模拟峰值 277MB |
| 3. release 构建差异 | **排除**——release 二进制同数据重放峰值 158MB |
| 4. footprint-日志对齐 | **采纳并完成**——发现关键新现象：**仅流式输出（无切换）即可 1.1G→7.9G**，停止即回落（振荡非泄漏） |

完整分析见：`26g-peak-analysis-20261016.md`。剩余嫌疑：事件洪峰投影（S2）> summary 双份序列化（S1，实测 16MB/次但次数不足）> 投影修复（S3）。MallocStackLogging 终审待用户配合重启。
