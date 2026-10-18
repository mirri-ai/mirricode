# Desktop mirri 服务内存峰值调查报告（26GB 尖峰 + 日常振荡）

- **日期**：2026-10-16（延续 `rust-v3-session-cold-open-perf-20261015.md` 的修复后调查）
- **状态**：**已修复（待 desktop 实机验收）**。P1/P2/P3/D1/D2 + S5 全部落地（见 §八），rust workspace 全测绿 + clippy 零警告；隔离实例完整 desktop 调用序列 ×15 会话 = 3.6s、峰值 188.7MB。desktop 实机猛切换验收待用户操作。
- **调查方法**：vmmap 采样 / WS RPC 探针 / 双二进制对照复现（debug + release）/ sample 栈抓取 / leaks / 日志时间线对齐 / **tcpdump 环回 WS 流量分析**。

## 一、问题现象

### 现象 A：26GB 峰值（主问题）

用户在桌面端（Electron + rust 服务）重新打包（**含全部性能修复**，见历史报告）后：

1. 23:01:53 服务启动，初始 footprint ~3G
2. 23:02-23:08 用户连续点击打开约 15 个会话（磁盘总量 ~130MB）
3. footprint 飙至 **26.2G**（`Physical footprint (peak)`），此后维持 26G 峰值计数
4. 用户体感：**会话加载本身不慢**（修复生效），但连点后内存飙升

### 现象 B：活跃期内存振荡（次问题，日常可见）

修复后的持续观察（02:09-02:15 时间段，footprint 时间序列）：

| 用户动作 | footprint 变化 | 说明 |
|---|---|---|
| 切换到已打开的会话 | 每次切换 +0.5~1G | 切换 = 该会话完整读面重算 |
| 无切换、仅对话（agent 流式输出中） | 1.1G → 2.9G → 3.1G → 6.4G → 7.9G | **打字期间持续增长** |
| 停止动作（agent 输出完毕） | 数秒内回落 4.2G→1.7G→2.3G | 回落但不归零 |

**关键特征：增长与"会话活动"（切换/流式输出）同步、停止即回落、不累积——是瞬时分配振荡，不是泄漏。**

## 二、内存形态证据（vmmap / leaks）

峰值后的进程解剖：

| 指标 | 值 | 含义 |
|---|---|---|
| Physical footprint (peak) | 26.2G | 进程生命周期最大值 |
| Physical footprint（当前） | 振荡，1G-8G | 随活动涨落 |
| leaks | **0 泄漏**，活对象 217MB / 108 万节点 | 无泄漏 |
| MALLOC_SMALL (empty) | 22.9G 虚拟 / 15.1G resident / **6.4G dirty**，5850 regions | 已释放未归还内核的 malloc 空洞（macOS 特性：不主动还页） |
| MALLOC_SMALL（活跃） | ~1.5G dirty | 真实在用 |
| 单 region dirty | 全部 <50MB | **分配形态 = 海量小对象**（serde_json::Value 树节点特征），分散在数千个 4MB malloc 区 |

**指纹结论：短时间内分配了 ~20GB 量级的海量小 JSON 对象，随后释放；dirty 残留是 malloc 不还页的显示假象。**

## 三、已排除项（全部有实测证据）

| # | 排除项 | 证据 |
|---|---|---|
| 1 | resume/fold 路径本身 | 隔离环境（真实数据快照 + 确定含修复的二进制）：15 会话并发 resume，3.6s 全完成，**峰值 170MB** |
| 2 | release 构建差异 | 用 desktop 同款 release 二进制 + 隔离数据重放：**峰值 158MB** |
| 3 | 完整 desktop 调用序列 | 模拟每会话 6 个 RPC（subscribe/resume/get/snapshot/message.list/agent.list）×15 并发：5.4s，**峰值 277MB** |
| 4 | 泄漏 | leaks 0 字节；活对象 217MB |
| 5 | single-flight 缺失导致的并发重复恢复 | `resume_session_with_scope` 步骤 1 的 AlreadyExists 守卫挡住并发第二次 fold（实测并发打开同一会话无重复 replay 日志）；且并发 15 会话场景峰值 170MB |
| 6 | 子代理懒恢复触发全量 fold | `agent.list` 走 roster（实测两实例均返回 20，与名单一致，非 54 全量）；子代理 hydrate 只在 steer/唤醒/通知路径触发 |
| 7 | 订阅风暴 | 日志高频 subscribe 是调查探针自身；desktop 实际为每会话 1 次 + 周期性 re-subscribe（~40s 一次，量小） |
| 8 | 事件环形缓冲 | EVENT_LOG_CAPACITY=256，有界 |

**核心矛盾：所有可控复现（三种构建 × 三种负载）峰值 ≤277MB，但真实 desktop 使用出现 26G——触发器在 RPC 层可模拟范围之外。**（该矛盾最终由 §四"最终因果链"解决：模拟探针消费速度足够快，真实 Renderer 不行。）

## 四、根因定位：最终因果链（tcpdump + 进程状态实证）

### 4.1 关键实验序列（定位过程）

| # | 实验/观测 | 结果 |
|---|---|---|
| 1 | 用户重启 desktop 后点开小会话 ×2，切换 | 75MB → 83MB，正常 |
| 2 | 用户点开本会话 aad4bb23（3607 ops，17MB） | **瞬间 4.40GB**（Activity Monitor：Real 4.40GB / Private 仅 153MB） |
| 3 | 同会话 subscribe+resume（隔离实例探针） | **+45MB**，1.0s |
| 4 | desktop 全启动序列 16 种 RPC 模拟（隔离实例） | **+73MB** |
| 5 | 打开瞬间日志 | fold 40ms 完成无热点；**打开前 18 秒 tx1 workspace 两个小会话出现 3Hz×12s 的 re-subscribe 循环（30+ 次，仅 desktop 客户端有此行为）** |
| 6 | **tcpdump 环回抓包（用户配合猛切换复现）** | 服务端→Renderer 持续满速推 16KB 包；**TCP 接收窗口从 4890 收缩到 254 字节（满背压）**；该连接已传输 1GB+ |
| 7 | 进程状态（猛切换中） | rust 服务 9.1G RSS / CPU 8.7%；**Renderer CPU 60.2% 在拼命解析** |

### 4.2 因果链（每一环都有实证）

```
用户切换/打开会话
  → 服务端: subscribe + resume(40ms, 已修好) + snapshot
      + resumed_session_summary(全量双份, 16MB/会话)
      + message.list(先全量深拷贝再切窗口)
  → Renderer 解析不过来（实验 6: TCP 窗口收缩到 254B）
  → 服务端已序列化的大帧在 outbox 排队积压
  → 猛切换 N 个会话 = N 份"全量拷贝 + 大响应"同时在服务端堆着
  → 26G ≈ 15 会话 × (16MB summary 双树 + message.list 全量拷贝
       + snapshot + 事件 catch-up) × serde 中间态膨胀(3-5×) × resync 循环放大
  → Renderer(60% CPU) 慢慢消化 → 服务端释放 → malloc 空洞保留（footprint 假高）
```

为什么隔离探针复现不出（≤277MB）：探针消费速度快，服务端不积压；真实 Renderer 是慢消费者，服务端响应全部排队等它。

### 4.3 三个根因缺陷（代码定位）

| # | 缺陷 | 代码位置 | 机制 |
|---|---|---|---|
| **P1** | **`message.list` 假分页**：先 `history().to_vec()` 全量深拷贝整个会话历史，再在内存切窗口 | `rust/crates/mirri-server/src/rpc.rs:2776` | 每次翻页 = 全量拷贝一次；`turn_usage_read_face` 也吃全量 history |
| **P2** | **`resumed_session_summary` 全量双份序列化**：每个 agent 的完整 history 两棵 Value 树（history + replay） | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:844-885` | 客户端只需摘要却收到全量历史；93226585 单次 16MB / 2.7s |
| **P3** | **慢消费者 + 无字节级背压**：outbox 256 帧容量有界但**每帧无大小上限**（16MB 响应也是一帧）；RPC spawn 无并发上限 | `rust/crates/mirri-server/src/ws_rpc.rs:118`（OUTBOX_CAPACITY） | 前端慢 → 服务端 GB 级积压；resync 循环（前端 lag → re-subscribe → catch-up replay → 更 lag，3Hz×12s 实证）再放大 |

### 4.4 数据获取架构总览（打开会话时前端怎么拿到数据）

| 数据 | 通道 | 方式 | 问题 |
|---|---|---|---|
| 会话状态/审批/work-facts | `session.snapshot` 单次 RPC | ~几十 KB | 健康 |
| 历史消息 | `message.list` 分页请求 | **假分页**（P1） | 每次全量深拷贝 |
| 错过的事件 | subscribe cursor replay | 逐条推，环形缓冲 256 有界 + epoch 守卫 | 机制健康 |
| 实时流式事件 | live forwarder 逐条推 | 每 token 级一帧 | 正常，但被 P3 积压放大 |
| resume 响应 | `resumed_session_summary` | 全量历史×2（P2） | 最大单次响应 |

结论：**不是"所有信息一条一条推"**——问题是单次响应过大（P2）、翻页假分页（P1）、以及大响应在服务端无界积压（P3）。

## 五、原"剩余嫌疑"的结案陈述

- **S1（summary 双份序列化）→ 确认为根因 P2**。
- **S2（事件洪峰）→ 部分确认**：流式输出期间的振荡（现象 B）与慢消费者积压（P3）机制一致， Renderer 解析速度是反馈环的一环；但"事件投影本身有独立放大缺陷"未证实。
- **S3（Projection repair 重投影）→ 未证实为独立根因**；33 次/窗口的量级不足以解释峰值，降级为观察项。
- **S4（blob 再水化）→ 排除**（本会话 blob 仅 88KB）。
- **客户端 resync 自激 → 确认为 P3 的放大器**（3Hz×12s 实证）。
- **S5（task.list 轮询放大器）→ 确认为 desktop 实机剩余 7G 峰值的根因**（第一轮五项修复后仍存）：web 客户端 `useTaskPoller` 每秒 `task.list` 全量拉取（1241 任务 ≈ 2.6MB/响应，`output` 61% + `kind.command` 23%），且 `WsRpcApi.getTask` 无单任务 RPC——每次都全量列表再过滤，使每秒流量达到 (1+N)×全量。修复：服务端摘要视图（`output` 截尾 2KB + `command` 截 256 字符 + `includeFull` 参数，`task.list`/`task.listWorkspace` 共用）+ 客户端 `activeOnly` 轮询 + 单任务 `task.output` 取尾 + 全量列表仅会话打开时拉一次 + 终态回填并发上限。

## 六、时间线证据（26G 那次的精确窗口）

```
23:01:53  服务启动（新二进制，含 F5+F1+F2+F4 全部修复）
23:01:53+ 启动批量恢复：web 客户端自动恢复最近 15 个会话
          （部分有快照走增量，部分无快照走全量 replay）
23:02-23:08 用户连点打开 15 个会话
          - 每会话：subscribe → resume → 16MB 级 summary 响应 → snapshot → 事件订阅
          - 日志：每会话恰好 2 次 "wire log replayed"（F2 空-steer 缓存 bug 的 fallback，已知）
23:0x    footprint 3G → 26.2G
之后     维持峰值计数；活对象仅 217MB
```

## 七、已知待修问题（调查中发现的独立缺陷）

| # | 缺陷 | 位置 | 状态 |
|---|---|---|---|
| P1 | `message.list` 假分页：`to_vec()` 全量拷贝后切窗口 | `rust/crates/mirri-server/src/rpc.rs:2776` | 已定位，待修 |
| P2 | summary 全量双份序列化 | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:844-885` | 已定位，方案待定（分页/惰性） |
| P3 | outbox 无字节上限 + resync 无退避 | `rust/crates/mirri-server/src/ws_rpc.rs:118` | 已定位，待修 |
| D1 | F2 steer 缓存只在非空时写入 → **空 steer 会话（绝大多数）永远 fallback 第二遍全量 replay**（含全量 parse） | `rust/crates/mirri-core/src/session/manager/restore.rs:203`（`if !state.steer_pending.is_empty()`） | 已定位，待修（follow-up worktree 已建） |
| D2 | 子代理 journal 在 resume 时被 parse 两遍（turn-id 扫描一遍 + steer 检查一遍） | `restore_subagents` + `replayed_pending_agent_steers` | 外部 review 已确认，待修 |
| D4 | b153d84f 会话存在 duplicate turn ids 告警（tripwire 触发，turn-id 4 个重复） | 该会话历史数据 | 登记观察 |
| S5 | task.list 轮询放大器：web 每秒全量拉取 + getTask 无单任务 RPC（每秒 (1+N)×全量列表） | `apps/mirri-web/src/composables/client/useTaskPoller.ts`、`apps/mirri-web/src/api/daemon/ws-rpc-api.ts`、`rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs` | 已修复（服务端摘要视图 + activeOnly 轮询 + 单任务 output 面） |

## 八、修复方案（按杠杆率排序）

1. **P1 `message.list` 真分页**：`to_vec()` 全量拷贝改为按 id 窗口直接索引切片（`rpc.rs:2776`）——最小改动，消灭每次翻页的全量拷贝；
2. **P2 summary 瘦身**：`resumed_session_summary` 不再内嵌每 agent 全量 history/replay，前端按需 `message.list` 分页拉；
3. **P3 outbox 字节级限流**：响应超阈值分片/拒绝，不让 GB 级数据在服务端排队等慢消费者；resync 加退避（lag 后最多重拉一次，防 3Hz 自激）；
4. **D1/D2**：steer 提取合并进上界扫描同一遍 + per-child take-once 缓存（worktree `.worktrees/perf-subagent-journal-single-pass` 已建）。

预期：1+2+3 修完后，"打开会话/猛切换"的服务端瞬时分配从 GB 级回到 MB 级。

## 九、下一步

1. 修复批次立项（P1 → P2 → P3 → D1/D2，按 §八顺序）；
2. 验收标准：desktop 重启后猛切换全部会话，服务端 footprint 峰值 < 1G、Renderer 不再 60% CPU 长跑；
3. 兜底手段（如需终审分配栈）：`launchctl setenv MallocStackLogging 1` + 重启 desktop 复现（无 Xcode，xctrace 不可用）。

## 十、与历史报告的关系

- 前序报告：`docs/specs/reports/rust-v3-session-cold-open-perf-20261015.md`（已修复 R1-R4；SDR-005 / ADR-017）
- 本报告是其后续：**修复后的新现象调查**。resume 路径的性能问题已解决（2000 倍 fold 提速实测），26G 峰值是修复前就存在、被 fold 噪声掩盖的**另一个问题**，在 resume 变快后显形。
- 外部分析简报（面向帮手）：`docs/specs/reports/26g-peak-briefing-20261015.md`；外部帮手回复的主要押注（single-flight 缺失 / 子代理懒恢复触发全量 fold）已被实验 5、6 证伪，其提出的 MallocStackLogging / footprint-日志对齐方法被采纳（本报告第五节即对齐产物）。

## 附：关键文件地图

| 关注点 | 文件 |
|---|---|
| message.list 假分页（P1） | `rust/crates/mirri-server/src/rpc.rs:2742-2800` |
| summary 序列化（P2） | `rust/crates/mirri-embedded/src/embedded_server/session_handlers.rs:793-905` |
| 事件订阅/转发/outbox/resync（P3） | `rust/crates/mirri-server/src/ws_rpc.rs` |
| 事件投影 | `rust/crates/mirri-core/src/session/event_projection.rs` |
| 投影修复（观察项） | `rust/crates/mirri-core/src/context_memory/projector.rs` |
| restore-on-demand | `rust/crates/mirri-server/src/rpc.rs:649` |
| steer 缓存 bug（D1） | `rust/crates/mirri-core/src/session/manager/restore.rs:203` |
| 看门狗脚本 | `.tmp/peak_watch3.sh`（footprint 采样 + 峰值自动 sample） |
| 复现脚本 | `/tmp/bulk_concurrent.py`、`/tmp/desktop_sim2.py`、`/tmp/resp_size.py`、`/tmp/startup_sim.py`（隔离环境重放） |

