# Rust 内核域设计 — 12: 会话分叉管道（session fork pipeline）

> **文档类型：设计类**（按 `docs/specs/AGENTS.md` 治理约定）——本文持终态架构与设计裁决（ADR-F1~F5）。
> 需求契约（验收依据）：[../session-management-requirements.md](../session-management-requirements.md) Epic E5（US5.1/5.2/5.4/5.5/5.6，SDR-005/006/007/010/011/012）· 架构裁决（管道五阶段、归属标识规则、按记录截取）：[../session-management-architecture.md](../session-management-architecture.md) §6 + ADR-007/008。
> 现状证据：[../session-management-context.md](../session-management-context.md) §1.3/§1.4/§1.5a/§1.7 · Rust 管道现状的专项侦察见 [./context.md](./context.md) §9。
> 执行状态唯一源：[./ROADMAP.md](./ROADMAP.md) —— 本设计的实现批次排队于该处，本文不持进度。

## 1. 目标与范围

把 Rust 内核的 `session.fork` 从"内存态单成员分叉"（复制 context memory、不落 wire、血缘断裂）重构为**按记录截取重写的会话级分叉管道**，满足 E5 全部已裁决需求：

- **输入**：源会话 + 可选分叉点（回合标识）+ 可选标题/元数据。
- **输出**：完全独立的新会话——截至分叉点的全部成员的对话历史（wire 截取复制）、血缘（forkedFrom）、`Fork of <源标题>` 命名、锚点校验护栏（活跃回合不作锚点，ADR-F4 修订版）。
- **不做**：v2 引擎的改动（cron 克隆移除、`Fork: `→`Fork of ` 等对齐项随退役线，见 §7 范围声明）；web/TUI 的分叉点选择 UI（消费端接线，另行批次）。

**架构对齐**：本设计是会话管理域架构 §6 五阶段管道（锚点解析 → 成员级截取 → 元数据铸造 → 运行中资产重置 → 原子落位）在 Rust 内核的落地；管道语义以那边为准，本文只定 Rust 侧的机制与接口形状。

## 2. 现状差距（摘要，证据见引用）

| # | 差距 | 现状证据 |
|---|---|---|
| G1 | fork 不复制 wire 日志——新会话重启即丢历史 | manager/mod.rs:674 只 `create_session_inner` + `fork_agent`（内存深拷贝） |
| G2 | 单成员分叉（RPC `agentId` 缺省取第一个） | session_handlers.rs:1519；SDR-011 要求全量成员 |
| G3 | `forked_from` 不写入（`PersistedSessionMetadata::from` 硬编码 `None`） | persistence.rs:144；scope 层 `SessionMetadata` 无此字段 |
| G4 | 无护栏（活跃回合可被选为分叉点） | dispatch_fork_session 全函数无检查；ADR-F4 修订版要求锚点校验（活跃回合 → `TURN_ACTIVE`） |
| G5 | 命名 `Fork of <源id>`（应为源标题） | session_handlers.rs:1565 |
| G6 | RPC 不收自定义元数据、无分叉点参数 | ForkSessionParams 仅 session_id/agent_id/title |
| G7 | `agents/<id>/wire.jsonl` 子代理 journal 无复制语义 | restore.rs:228 恢复时折叠它们，fork 不带 |

## 3. 终态设计

### 3.1 契约面（`session.fork` 扩参）

```
session.fork {
  sessionId: string,              // 源会话
  agentId?: string,               // 废弃语义：全量成员复制后不再需要选成员（SDR-011）
                                   // —— 保留参数形状兼容存量调用方，传入时忽略并记
                                   // debug 日志（不是错误：契约不因功能演进而破坏）
  turnId?: number,                // 分叉点 = 目标回合的回合标识（glossary S7"回合"）——
                                   // 截取锚点取该回合的 turn.ended 记录
                                   //（缺省 = 最近已收尾回合——活跃回合不作缺省锚点，ADR-F4 修订版）
  title?: string,                  // 缺省 "Fork of <源标题>"（SDR-012）
  metadata?: object,              // 自定义元数据合并（对齐 v2 ForkSessionOptions.metadata）
}
```

**错误语义**（明确报错、不静默降级——会话管理域架构 §6.1 第 1 步）：
- `turnId` 不存在（日志中无此回合的 `turn.ended`）→ `INVALID_PARAMS`，消息注明可用回合范围。
- `turnId` 指向未收尾回合（有 `turn.prompt` 无 `turn.ended`——只可能是当前活跃回合）→ `TURN_ACTIVE`（复用既有错误码，与 message 并发守卫同族）。
- 锚点指向活跃回合（有 `turn.prompt` 无 `turn.ended`——即当前进行中的回合）→ `TURN_ACTIVE`（该回合尚无完整历史可复制；除此之外源会话是否忙碌不影响放行——ADR-F4 修订版）。

### 3.2 分叉点解析与截取算法

回合在 wire 里是**括号结构**：`turn.prompt` 开括号、`turn.ended` 闭括号，turn 的全部消息记录夹在其间（顺序验证见 context §9.2）。截取算法：

```
截取边界 = 目标回合的 turn.ended 记录（含）
对源会话主日志（wire.jsonl）：
  1. migrate_records_in_memory 读取全部记录（复用 migration.rs:654 原语，
     容忍末行撕裂与版本迁移——fork 读源与 resume 读源用同一套）
  2. 顺序扫描，遇到 turn_id > 目标回合 id 的 turn.prompt 即停止
     （turn_id 单调递增，TurnFlow 分配器保证——context §9.2）
  3. 边界后若有零星记录（跨回合注入等），一并截断——
     硬规则：截取流以目标回合的 turn.ended 为最后一条记录
```

**成员级并行**（SDR-011 全量复制）：对 `state.json` agents 清单的每个成员，截取 `agents/<id>/wire.jsonl` 同样到该成员日志中目标回合的边界。子代理 journal 的回合计数与主 journal 独立（journal_route.rs 的分片语义），截取算法相同、按各成员自己的括号结构执行。

**产物**：每个成员的截取记录流 + 头部 metadata 记录 + 尾部 `forked` 边界记录，经 `JsonlAppendLog::create` 写入目标会话对应 journal。

### 3.3 复制过滤表（运行中资产重置——管道第 4 阶段）

截取**不是**无脑全拷：按记录类型过滤（SDR-007"复制已完成、重置运行中"的落点）：

| wire op | 处置 | 依据 |
|---|---|---|
| `metadata` / `turn.prompt` / `turn.ended` / `turn.steer` / `context.*` / `usage.record` / `permission.*` / `plan.*` / `config.update` | 原样保留 | 已完成的对话记录与随记录落盘的配置 |
| `cron.add` / `cron.delete` / `cron.cursor` | **全量过滤**（从截取流中剔除） | ADR-F1：历史 cron 状态属于"运行中资产"的账本，fork 不带（详见 §4 ADR-F1） |
| `forked` | 过滤（源日志里的历史 fork 标记不带入） | 历史边界标记对新会话无意义 |
| `goal.create` / `goal.clear` | 原样保留（内容与清除都是对话历史） | SDR-007：目标内容带过来 |
| `goal.update` | **字段级过滤**：保留 objective/budget 等内容字段，`status` 字段强制改写为未启动（并非整条剔除——状态就住在 update 里） | SDR-007：内容带过来、一律未启动 |
| `full_compaction.*` | 原样保留（摘要落位记录） | 会话管理域架构 §6.1 第 2 步：压缩摘要按落位顺序保留，模型可见历史由折叠器（reducer fold）重建 |
| 未知 op | 原样保留 | journal_route 的不丢数据原则，同 context §9.1 迁移原语先例 |

**文件实体处置**：

| 目录/文件 | 处置 | 依据 |
|---|---|---|
| `agents/*/wire.jsonl` | 按成员截取复制（§3.2） | G7 |
| `blobs/` | 复制（或按截取流中 blobref 引用选择复制） | media rehydrate 依赖（restore.rs:139-149） |
| `tasks/` | **不复制** | 运行中资产；复制产生幽灵任务（resume 会 reconcile 为 stopped） |
| `snapshot.json` | 不复制 | op_index 与截取后日志不符，resume 的 stale 检测兜底 |
| `state.json` | 重建（§3.4） | 新身份 + forkedFrom + agents 清单 |

### 3.4 state.json 铸造与锚点校验护栏

**state.json 铸造**（管道第 3 阶段）：

```
PersistedSessionMetadata {
  …继承源会话的 work_dir/tags（SDR-007 共享条款）…,
  title: "Fork of <源标题>",        // G5；源标题不可得退化源 id
  forked_from: Some(源会话 id),      // G3——需要打通的字段链：
                                     //   scope SessionMetadata 增字段 →
                                     //   PersistedSessionMetadata::from 增映射
  agents: <按截取后的成员清单重建>,   // 对齐 v2 state.json 的 agents 字典形状
  created_at/updated_at: now,
  …自定义元数据合并（G6，剔除 goal 键——对齐 v2 forkCustomMetadata 语义）…
}
```

**忙碌护栏**（ADR-F4 修订版：锚点已收尾即放行）：护栏语义收缩为**锚点校验**——解析出的锚点必须是已收尾回合的 `turn.ended`。显式 `turnId` 指向已收尾回合或缺省取最近收尾回合 → 放行（源会话可有其他活跃回合，管道不碰源内存，活跃回合记录天然在截断边界之后）；锚点指向活跃回合本身 → `TURN_ACTIVE` 拒绝。**实现注记**：v1 的 `SESSION_FORK_ACTIVE_TURN` 全量忙碌检查**不迁移**（它的语义是"turn 进行中即拒绝"，与本裁决相悖）；embedded 层不引入 `session_is_busy` 前置门，忙碌判定退化为锚点校验在 core 截取算法内的自然结果。

### 3.5 原子落位与回滚

- 写入顺序：目标目录（截取 journals + blobs）→ `state.json` → 会话索引登记 → 事件发布。任何一步失败：删除目标目录、清理内存态（与 v2 fork 的 catch 回滚结构对齐）。
- **可见性门**：全部落盘成功后才 `note_active_session_in` + `spawn_session_relay`——失败路径上不出现"半成品会话进列表"的窗口。

### 3.6 前缀确定性义务（SDR-010 修订版的唯一引擎义务）

分叉会话的首请求前缀与源会话在分叉点的请求前缀**逐字节一致**，由三件事保证：
1. 截取算法的确定性输出（§3.2 的单调扫描 + 括号边界）；
2. 复制过滤表不改写保留记录的字节（过滤是整条剔除，非字段改写）；
3. 归属标识随新会话身份（无需任何引擎改动——Rust 侧 cache key 配线本来就不存在，新增时按"会话自身 id"实现即可）。

## 4. 设计裁决（ADR-F）

### ADR-F1 fork 截取流过滤 cron 域 ops（不带历史 cron 状态）

- **背景与动机**：cron 在 Rust 内核里以 wire op 持久化（`cron.add/delete/cursor`，折叠器（reducer）折叠进 `state.cron_tasks`）——直接截取复制会带回源会话的全部历史 cron 状态，fork 会话 resume 后这些任务全部复活。SDR-007 裁决"定时提醒任何一条都不带"，但字面上"复制已完成对话"又包含这些已落盘记录——两条原则在 cron 上冲突，需要显式裁决。
- **决策**：**全量过滤 cron 域 ops**。判定标准取"资产的当下语义"而非"记录的落盘形态"：cron 的折叠后状态就是**运行中资产**（会触发、有游标、有双跑风险——与 v2 的 cron 克隆同害），历史记录只是它的账本载体。fork 复制的是"对话与随对话演化的状态"，不复制"会触发副作用的调度状态"。
- **否决的替代**：(a) 原样保留（SDR-007 字面最大化）——resume 后 fork 会话响起源会话的闹钟，正是 SDR-007 要消灭的双跑；(b) 保留 `cron.add/delete` 但重写 id——伪造历史记录，账本与源失真对应。
- **影响面**：§3.3 过滤表；`session.fork` 的 e2e 验收（fork 后 `cron.list` 为空 + resume 后仍为空）。

### ADR-F2 分叉点参数用 turn_id（u64），不用消息序号或时间戳

- **背景与动机**：会话管理域架构 §6.2 定了"回合标识"作语义参数，Rust 侧的具体形状有三种候选：turn_id（TurnFlow 分配的 u64）、wire 记录序号（文件行号）、时间戳。
- **决策**：`turnId: u64`。turn_id 是 wire 括号结构的一等公民（`turn.prompt`/`turn.ended` 都携带它）、单调递增、与呈现层（web ChatTurn 的回合序号）可直接对齐。行号是物理存储细节（迁移后漂移）、时间戳有精度与乱序问题。
- **否决的替代**：行号锚点（迁移后失效）；时间戳锚点（并发记录同毫秒时歧义）。
- **影响面**：§3.1 契约、§3.2 算法；klient 侧 turnId 的取值来源（消费端从对话呈现层获得，架构 §6.2 已定）。

### ADR-F3 归属标识随会话身份——Rust 实现义务为零改动

- **背景与动机**：SDR-010 修订版裁决归属标识随新会话身份（不沿袭），引擎义务只剩前缀确定性。Rust 侧 cache key 配线**当前不存在**（context §9.6 缓存归属现状）。
- **决策**：本管道**不为归属标识做任何事**。未来 Rust 补 cache key 配线时按"会话自身 id"实现（与 SDR-010 修订版一致），届时与 fork 管道无交互——管道不传递、不改写任何归属信息。
- **否决的替代**：随管道预埋归属字段——为不存在的配线预留接口，违反"不为未来功能预写代码"。
- **影响面**：§3.6；ROADMAP 的 cache key 配线独立开放项（如有）不受本设计影响。

### ADR-F4 分叉护栏 = 锚点校验：锚点已收尾即放行（修订版，取代首版"忙碌无条件拒绝"）

- **编号与日期**：ADR-F4 · 2026-10-09（同日修订，修订记录见文内）
- **背景与动机**：首版裁决无条件拒绝（与撤销护栏同口径的最简规则）。修订时推演发现两类分叉的安全模型完全不同：撤销**原地改写**会话状态，必须等静止；fork 是记录式管道——从会话日志截取重放，不碰源会话内存，截断边界是目标回合的 `turn.ended`，活跃回合的全部记录天然落在边界之后被截掉。忙碌的传统危害（撕裂内存态、半截对话、增长中的文件）对新管道全部不成立——**"锚点已收尾"本身即是安全证明**。且 turn-end fork 的立项场景是"会话被污染，从干净点重新推进"，污染回合进行中恰是最需要分叉的时刻；首版的"灭火器要等火烧完"与产品动机相悖。7c 用户裁决的原始范围是"**无锚点参数的末尾分叉**遇会话进行中"——首版把拒绝扩展到"有锚点且已收尾"属过度延伸。
- **决策**：**锚点已收尾即放行**。(1) 显式 `turnId` 指向已收尾回合 → 放行（无论源会话是否有其他活跃回合）；(2) 缺省锚点 = 最近一个**已收尾**回合 → 放行，活跃回合内容不带（新会话历史截止到最近收尾回合——首版"缺省即拒绝"同步修订）；(3) 仍拒绝的只剩：锚点指向活跃回合本身 → `TURN_ACTIVE`（该请求无意义——活跃回合尚无完整历史可复制）。
- **否决的替代**：(a) 无条件拒绝（首版，被本修订取代）——与撤销同口径是表面的类比，两者的忙碌暴露完全不同；(b) 锚点早于活跃回合时拒绝、锚点恰为最近收尾时放行——区分"多早算早"引入第二套偏序语义，收益为零。
- **修订记录**：首版裁决无条件拒绝；同日经记录式管道安全模型推演修订为本决策（用户裁决选项 B）。
- **影响面**：§3.4 护栏语义；US5.4 验收的忙碌条款与拒绝反馈验收（需求侧同步改写）；V4 验收改写。

### ADR-F5 `agentId` 参数废弃但不移除（契约兼容）

- **背景与动机**：SDR-011 裁决全量成员复制后，`session.fork` 的 `agentId` 参数失去选成员语义；但存量调用方（TS v2 forkSession、web）仍在传。
- **决策**：参数保留、语义改为**忽略**（记 debug 日志）。不返回错误——契约不因功能演进而破坏；移除走退役线（B6 模式）。
- **否决的替代**：(a) 传入即报错——破坏存量调用方，收益为零；(b) 立即移除——属 breaking change，需 major + 用户确认（不满足触发条件）。
- **影响面**：§3.1 契约；v2 对齐批次（若 v2 侧清理调用参数，按退役线走）。

## 5. 验收标准（e2e 级，对应需求 US）

| # | 验收（Given-When-Then） | 对应需求 |
|---|---|---|
| V1 | Given 源会话有 3 个已收尾回合，When 以第 2 回合为分叉点 fork，Then 新会话恢复后历史恰含前 2 回合（第 3 回合及其后记录不在），且继续对话的上下文不引用第 3 回合 | US5.4 |
| V2 | Given 源会话含主 agent 与 2 个子代理成员，When fork，Then 三者的 journal 全部截取复制，resume 后子代理历史完整 | US5.6/SDR-011 |
| V3 | Given 源会话设有定时任务，When fork，Then fork 会话 `cron.list` 为空，resume 后仍为空 | US5.1/SDR-007 |
| V4 | Given 源会话第 3 回合进行中，When 以第 2 回合为分叉点 fork，Then 成功（新会话历史恰含前 2 回合）；When 以第 3 回合为分叉点 fork，Then 被拒绝（回合尚未收尾）；When 缺省分叉点 fork，Then 成功且新会话历史恰含前 2 回合（活跃回合内容不带） | US5.4 护栏（ADR-F4 修订版） |
| V5 | Given fork 未传标题，Then 新会话标题为「Fork of 源标题」且 state.json `forkedFrom` = 源 id | US5.1/SDR-012 |
| V6 | Given fork 中途失败（注入磁盘错误），Then 无目标目录残留、源会话不受影响、列表无新条目 | US5.1 回滚 |
| V7 | Given fork 出的会话，When resume 后继续对话，Then 模型请求前缀与源会话同分叉点的前缀逐字节一致 | US5.5/SDR-010 |
| V8 | Given 分叉点指向不存在的回合，Then 明确报错（非静默降级到末尾） | US5.4 错误语义 |

## 6. 术语规范（域内视图）

| 规范名 | 域内语境 | 唯一定义 |
|---|---|---|
| fork / 复制会话 | 动作与产物（本域核心对象，中文行文用"分叉/复制"，接口与代码名一律 `fork`） | glossary G2 |
| 分叉点 | 用户语义：目标回合（已收尾）的终点；截取锚点是它的算法内部表达（该回合的 `turn.ended` 记录） | session-management 域内术语 |
| 回合 / turn | S7 唯一定义；wire 括号结构（`turn.prompt` 开、`turn.ended` 闭）的回合标识是 `turnId`（u64，ADR-F2） | glossary S7 |
| 成员 | 会话内 agent 的域内统称（主 agent A2 + 子代理 A3）；成对出现时"成员"与"agent"同义，单指人机主体时用"主 agent" | glossary A2/A3 |
| 会话日志 / wire journal | S5 唯一定义（wire.jsonl）；简称"日志"仅指它，"journal"是代码名同义词 | glossary S5 |
| 折叠器 / reducer fold | 从会话日志重放投影状态与历史的组件；"reducer"只作代码名 | 持久化域 |
| 服务方费用优惠身份 / 归属标识 | G6 唯一定义；本文简称"归属标识" | glossary G6 |
| 运行中资产 | 会触发副作用或持有执行状态的会话级状态（cron、goal 执行状态、后台任务、挂起交互）——SDR-007 的"重置"对象 | session-management SDR-007 |

## 7. 范围声明

- **v2 引擎改动不在本设计内**：cron 克隆移除、`Fork: `→`Fork of `、回合原子截断、锚点校验护栏——随 TS-v2 退役线（ROADMAP 1.3）或 E5b 对齐批次单独排期，本文只保证 Rust 侧终态正确。
- **消费端 UI 不在本设计内**：web 回合入口、TUI 分叉点选择器（US5.4 界面条款）——依赖本设计的 `turnId` 契约落地后另行批次。
- **createChild**：走同一管道 + 元数据铸造阶段额外写父子标记（会话管理域架构 §6.4），实现上复用 fork 管道主体，仅扩展 `dispatch_create_child_session` 的元数据构造——不单独设计。
