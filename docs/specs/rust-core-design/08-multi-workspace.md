# Rust Core 多工作区架构（Multi-Workspace）— 设计文档

> 状态: Draft（rev 2，吸收独立设计评审 H1-H4/M1-M6 发现）· 原稿日期: 2026-09-11 ·
> 作者: Architecture Team
> 上游: [00-overview.md](00-overview.md) §2 目标架构（Workspace 层 = 一等并发单元——本文是该层的落地设计）
> **执行状态不在本文维护**：阶段进度、批次状态、开放项统一见 [ROADMAP.md](ROADMAP.md)；现状证据（单工作区绑定的代码事实盘）见 [context.md](context.md) §6

## 1. 背景与动机

总纲已把 Workspace 层定为"一等并发单元"（desktop 同时开多项目），但引擎运行时至今是**单工作区绑定**：server 进程把启动目录当作唯一工作区，工作区的注册/切换对客户端是无效操作。由此产生三个抽象问题（终态达成后即消失）：

1. **多项目用户被锁死在单目录** — 用户在同一台机器上开发的每个项目，都需要一个独立的 server 进程；web 客户端的"添加/选择工作区"流程在 rust 后端下是无效点击。
2. **跨引擎工作区身份体系缺席** — TS 服务器与 web 客户端共享一份跨引擎工作区注册表（同一份磁盘注册表 + 会话索引），rust 引擎不参与该体系，两边各自维护互相看不见的工作区事实。
3. **会话归属失真** — 引擎上报的会话工作目录被统一回填为进程绑定根，历史会话无法按真实工作区分组，客户端的分组/过滤/排序全部失真。

## 2. 目标架构

Workspace 层的运行时形态：**进程内一个 `WorkspaceRegistry`，注册表之下每工作区一组隔离运行时，进程级服务单份共享；会话寻址经统一路由层解析到所属工作区运行时。**

```
宿主进程（mirri serve / EmbeddedServer 宿主）
│
├── App 层共享单例（生命周期 = 宿主进程）
│   ├── ServerConfig（home 级配置）
│   ├── SessionIndex（sqlite 读模型，schema 本身多工作区）
│   ├── UserMcpPool（用户级 MCP 连接，进程一份）
│   ├── PluginManager / OAuth / Flags
│   └── 跨引擎工作区注册表（磁盘 json + 索引写穿，TS 服务器同源）
│
├── 会话路由层（session-id → 所属工作区运行时）
│   活跃会话表 → 索引 workspace_id → not-found（见 §3）
│
└── WorkspaceRegistry（工作区生命周期唯一所有者）
    ├── workspace A → WorkspaceScope + SessionManager（会话持久化桶 A）
    ├── workspace B → WorkspaceScope + SessionManager（会话持久化桶 B）
    └── …（注册表行 = 生命周期单位；除默认工作区外惰性构建，见 §4 D5）
```

职责边界：

- **注册表**：open（注册/恢复）/ close（注销）/ 信任门的唯一所有者；行数据真源是跨引擎注册表（磁盘 json），进程内结构是其内存投影 + 写穿（写策略见 §2.3）。
- **隔离**（per-workspace）：会话树、project 层 MCP 连接池、技能目录发现、文件工具沙箱根、AGENTS.md 注入、git 状态。会话的工作目录、文件工具可写范围都取其所属工作区的根——会话层已有的"沙箱锚定会话工作区根"设计不变，只是根不再恒等于进程绑定根。
- **复用**（per-process）：会话索引、home 级配置、用户级 MCP 连接池、插件管理、事件中继。用户级 MCP 每 server 连一次、N 工作区复用——与 MCP 域 P2 既定设计一致。
- **身份**：工作区 id = 跨引擎注册表的 id（同时是会话持久化桶 key 与索引 `workspace_id` 列的拼写）。三层（注册表 / 持久化桶 / 索引）共用同一身份派生链，不存在引擎私有工作区 id；历史分叉桶（同根异拼写）经别名集折叠视为一个工作区（ADR D3，本期搬入）。

### 2.1 生命周期

```
注册（createOrTouch）──→ 注册表行落盘 ──→（懒）构建 scope+manager
恢复（serve 启动）    ──→ 读注册表逐行登记（坏行/根目录已消失的行：跳过+warn、保留行）
恢复（嵌入宿主启动）  ──→ 钉扎模式（见 §2.2）
会话创建于 W         ──→ touch W（last_opened_at 内存化 + 去抖落盘，见 §2.3）
注销（delete，三选一确认）──→ 物理删除：关活跃会话 → 删桶数据 + 索引行 → tombstone
                             仅移除：关活跃会话 → 桶数据保留 → tombstone（重新注册即恢复）
```

### 2.2 宿主形态与钉扎

多工作区是 **serve 宿主**（`mirri serve`、desktop 常驻）的形态。嵌入宿主（ACP 二进制按协商根启动、NAPI 以进程 cwd 启动）是**钉扎退化**：

- 钉扎根（协商根 / cwd）= 默认工作区，即刻构建运行时。
- 共享 home 下注册表中的**其余行只登记、不构建**（惰性语义的自然延伸）：不加载其会话树、不加载其 project 层 MCP、不参与 `session.list`/`session.search` 的默认可见集——除非调用方显式按 workspace id 寻址。
- 由此嵌入宿主的协商根语义（"这个 ACP 连接就是这个项目"）在注册表非空的共享 home 下依然成立。

### 2.3 注册表写策略与跨引擎并发

跨引擎注册表（磁盘 json）被同 home 的 TS 服务器与本引擎并发读写。原子替换只防损坏、不防丢更新（整文档 read-modify-write 会静默丢弃并发方写入的行），因此写策略为：

1. **锁文件 + 读-合并-写**：写注册表前取 home 级锁文件；持锁后重读磁盘当前内容；合并规则 = 本地行集 ∪ 磁盘行集，**tombstone 胜出**（任一方标记删除的 id 不复活）；合并后原子替换写回。锁陈旧（持锁方崩溃残留）时按 mtime 超时抢占。
2. **读侧过滤**：读注册表时以 tombstone 集过滤行集——tombstone 是删除语义的一部分，不只是写侧保留物。
3. **tombstone 新增面**：注销路径需要"写 tombstone"API（现有写侧只会保留已有 tombstone，不会新增）。
4. **写放大控制**：`last_opened_at` touch 在内存生效、去抖（批量/定时）落盘；注册、改名、注销立即落盘。会话创建不直接触发注册表写。

## 3. 会话路由层（session-id → 工作区运行时）

**所有按 session-id 寻址的入口都必须经路由层解析到所属工作区的 `SessionManager`**，而不是各自持单绑定 manager。这一层覆盖的面（现状全部经单 manager 解析，证据见 context.md §6.1/§6.4）：

| 调用面 | 覆盖的入口 |
|---|---|
| embedded 派发 | `session.get/close/resume/fork/archive/export`、`message.send/steer`、审批、提问、compaction、cron、事件中继、会话级 MCP bridge |
| serve 传输层 | 传输注册表持有的会话寻址 handler（审批/提问/agent/tool/文件/git/btw）、`session.subscribe`（事件订阅经 manager 解析）、restore-on-demand 钩子 |

解析规则（按序）：

1. **活跃会话表**（registry 内存结构：session-id → workspace-id，会话创建/恢复时登记，关闭时摘除）——本进程活跃会话的权威答案；
2. **索引回退**：非活跃但持久化的会话，查 `SessionIndex` 的 `workspace_id`（resume / 消息翻页 / 只读操作的目标）；
3. **不可解析** → not-found（不静默回退默认工作区——静默回退会把跨工作区操作错误落到默认 bucket）。

路由层的存在使 D2（registry-over-managers）成立：`SessionManager` 保持单工作区语义，`EmbeddedServer` 保持单一派发面；改造是机械的（每个 `self.manager.<按 session-id 寻址的调用>` 前插一个 `manager_for(session_id)` 解析），进程级设施（事件中继、cron、配置）不需要复制。弃置"每工作区一个 EmbeddedServer 实例 + 前置路由器"：隔离更彻底，但进程级设施要么复制要么重排，跨工作区方法（`workspace.*`、`session.list/search`）还需额外聚合层——复杂度不成比例（ADR D7）。

## 4. 契约语义（终态）

方法面走 mirri-proto 注册表标准路径（五环联动），语义与 TS v2 对齐：

| 方法 | 终态语义 |
|---|---|
| `workspace.createOrTouch` | 注册 root（校验存在且为目录）或 touch 已注册 root；返回 WorkspaceView；幂等（同 root 重复注册 = touch，不重建）。**root 规范化 = 词法解析**（与 id 派生链同构，不做文件系统 canonicalize——symlink/大小写不敏感 FS 下的同目录异拼写与 TS 同立场，TS parity 而非缺陷） |
| `workspace.list` | 全注册表，`session_count` 取索引读模型（折叠别名集）；按 `last_opened_at` 排序 |
| `workspace.get` / `handlerFor` | 按 id 解析；unknown id → null（klient maybe 语义） |
| `workspace.update` | rename（显示名）；写穿注册表与索引 |
| `workspace.delete` | 携带模式参数（ADR D4）：**purge 物理删除**（关活跃会话、删磁盘桶数据与索引行，不可恢复）/ **remove 仅移除**（关活跃会话、数据保留，重新注册即恢复）；两者均 tombstone 防跨引擎复活；客户端删除入口为三选一确认（物理删除 / 仅移除 / 取消），确认场景鼓励清理 |
| `session.create` | 接受 `workspaceId`，经路由层路由到对应工作区运行时；未知 id → invalid_params（不静默回退）；缺省 → 默认工作区 |
| `session.list` | `workspace_id` 过滤（**折叠别名集**——同一根目录的异拼写桶视为一个工作区）+ id 游标分页（`page_size`/`before_id`/`after_id`，索引层实现，差距见 [context.md](context.md) §6.2）+ `exclude_empty`（需索引扩展"无 prompt 会话"数据源，含存量 db 迁移）/ `include_archive` / `busy` 等过滤面以 TS 现网语义为准钉在 TSV；会话工作目录为真实值（不回填） |
| `session.search` | 跨工作区命中；每个 hit 的 `workspace_id` 为该会话真实所属（现状偏差见 [context.md](context.md) §6.1） |
| 会话寻址方法族 | 经 §3 路由层解析（不逐方法特判） |
| trust（读/写） | per-workspace 信任门（§5 D6）；REST 路由与 RPC 语义与 TS 对齐；**未信任 ⇒ project 层 MCP 不加载的闸门与多根加载同批落地**（时序见 D6） |

客户端（web rust 传输层）对本契约的映射现状与差距见 [context.md](context.md) §6.3——契约落地时的客户端改造范围以该盘点为准。

## 5. 关键设计决策（ADR）

| # | 决策 | 状态 | 内容与理由 |
|---|---|---|---|
| D1 | 进程模型 = **一进程多工作区** | 已裁决（用户，2026-09-11） | 对齐 TS 服务器形态与总纲 §2 的 Workspace 层定位。弃置方案"进程级单工作区 + 注册表仅做发现"：切换需重连/重启，多工作体验残缺，不解决动机 1 |
| D2 | 注册表形态 = **registry-over-managers** | 已裁决（用户，2026-09-11） | `WorkspaceRegistry` 持 N 组（scope + manager），`SessionManager` 保持单工作区语义不动。理由：会话持久化桶按工作区根派生（隔离天然成立）；共享面的进程级单份已有用户级 MCP 连接池先例。**注意：D2 只保证 manager 内部零改动——按 session-id 寻址的派发路径必须配合 D7 路由层，否则非默认工作区的会话在派发面不可见**。弃置"`SessionManager` 内部泛化多工作区"：改动面大且无对应收益 |
| D3 | 工作区身份 = **跨引擎注册表 id + 别名集折叠（本期搬入）** | 已裁决（用户，2026-09-11） | 注册表行 / 持久化桶 key / 索引 `workspace_id` 三处已统一在同一拼写派生链上，取它为唯一身份不引入第二套 id。**历史分叉桶的别名集语义（同一根目录的多种拼写视为一个工作区）随本期移植**（用户裁决：否决"后续再补"的分期方案）——折叠范围以 TS 现网为准：会话列表的 workspace 过滤折叠别名桶、工作区会话计数折叠别名集、索引刷新按别名集收敛；测试矩阵覆盖大小写 / `~` 展开 / 符号链接等历史畸形形态。弃置引擎私有 `WorkspaceId`：跨引擎互通断裂 |
| D4 | 注销语义 = **删除时三选一：物理删除 / 仅移除 / 取消** | 已裁决（用户，2026-09-11） | 删除入口弹确认，按用户意图三选一：**物理删除**（结束活跃会话 + 磁盘会话数据真删，不可恢复）／**仅移除**（结束活跃会话 + 磁盘数据保留，重新添加即恢复）／取消。两种执行均 tombstone 防跨引擎复活（重新注册时清除）。动机：存量会话占满磁盘是真实风险——用户确定要删的场景应**鼓励清理**（物理删除为确认框主推项，产品性能受益）。弃置"一刀切磁盘保留"（无视磁盘风险）与"有活跃会话则拒绝"（把清理成本转嫁给用户）两方案 |
| D5 | **注册先行 + 惰性构建 + 宿主钉扎** | 已裁决（用户，2026-09-11） | 注册表行先登记；默认工作区（serve = 最近打开 / 嵌入宿主 = 钉扎根，§2.2）即刻构建，其余首个触达请求再构建：N 工作区注册不放大启动延迟；嵌入宿主在共享 home 的非空注册表下保持钉扎语义 |
| D6 | 信任门 = **默认未信任 + home 级信任存储 + 加载门先行** | 已裁决（用户，2026-09-11） | 新注册工作区默认不可信任：未信任时 project 层 MCP 配置不加载、AGENTS.md 项目段不注入。信任状态落 home 级存储（不随注销删除）。与 TS v2 的 trust 路由语义对齐。**时序硬约束（完整门 = MCP + AGENTS.md 两半）**：多根 MCP 加载落地批必须同时携带加载门；且在信任门就位前，新注册工作区的 scope 一律按未信任策略构建（project 层不加载、AGENTS.md 项目段不注入）——不存在"已注册但策略未定"的窗口 |
| D7 | 会话路由 = **registry-aware 单服务器 + `manager_for` 解析缝** | 已裁决（用户，2026-09-11） | 见 §3。解析序：活跃会话表 → 索引回退 → not-found。弃置"每工作区一个 EmbeddedServer + 前置路由器"：进程级设施复制/重排 + 跨工作区方法聚合层，复杂度不成比例 |
| D8 | 注册表写并发 = **锁文件 + 读-合并-写（tombstone 胜出）+ touch 去抖** | 已裁决（用户，2026-09-11） | 见 §2.3。原子替换只防损坏不防丢更新；"接受 last-writer-wins"与 §7.3 跨引擎共存验收直接矛盾，弃置。**范围声明：本策略约束本引擎写方**——TS 侧写方现为无锁整目录替换（不持锁、不合并，见 context.md §6.2），残余 write-write 窗口与补救语义见 §7.3；TS 侧持锁合并补丁为 conditional followup（随退役线重估），不阻塞本设计 |

> D2–D8 已全部裁决（用户逐条确认，2026-09-11）：D3 别名集改为本期搬入（否决分期）、D4 删除语义为用户方案（三选一确认），其余五条按建议照准。后续设计变更按 append-not-rewrite 规则以新 ADR 追加，不原地改写已裁决行。

## 6. 与既有架构裁决的关系

- **总纲分层**（00 §2）：App 层（无工作区上下文的全局管理面）/ Workspace 层（隔离与生命周期）职责边界不变，本设计是 Workspace 层的展开。
- **MCP 三池**（mcp-architecture.md）：UserPool 属 App 层进程单份（本设计 §2 复用项）；WorkspacePool 随工作区生灭（本设计 §2 隔离项）；连接数不变式 = 用户级每 server 一连接。
- **契约事实源链**：方法面变更全走 mirri-proto 注册表 ⇄ TSV ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture 五环。
- **跨引擎注册表**：磁盘 json 是工作区身份与 tombstone 的唯一真源；sqlite 索引是会话读模型，db 打开时 seed、写侧写穿——两层存储职责不重叠。
- **zero-IO**（Z1 裁决）：注册表与索引的磁盘读写（含 D8 锁文件）经既有 IO trait 注入，mirri-core 不引入直接 IO。

## 7. 验收标准（设计级）

1. **多项目并发**：一个进程注册 N 个工作区，各工作区会话按真实工作目录分组、过滤、分页、搜索；文件工具与 AGENTS.md 注入锚定各自工作区根。
2. **会话路由完整性**：任意工作区会话上的会话寻址操作（消息发送、事件订阅、审批、提问、恢复）正确路由到所属工作区运行时——不因其非默认工作区而不可见。
3. **跨引擎共存**：同 home 下 TS 服务器与 rust 引擎并发读写注册表：本引擎侧 D8 合并语义保证**读时已见的行不丢失**、注销 tombstone 不复活、身份无碰撞。**已知残余窗口**：TS 侧写方（无锁整目录替换，见 context.md §6.2）与本引擎写交错时，TS 保存可能丢弃其读后本引擎新写入的行——补救 = 重新注册即可恢复；TS 侧持锁合并补丁为 conditional followup（随退役线重估，见 D8 范围声明）。
4. **兼容退化**：单工作区嵌入宿主（ACP/NAPI）在共享 home 的非空注册表下保持钉扎语义（协商根为默认，其余行惰性不干扰）；缺省参数的 `session.create` 语义不破坏既有调用方。
5. **资源不变式**：用户级 MCP 连接数不随工作区数放大；N 工作区注册不显著放大启动延迟（D5，惰性构建下注册行数与启动成本解耦）。

## 8. 文档关系

| 文档 | 关系 |
|---|---|
| [00-overview.md](00-overview.md) | 上游总纲（Workspace 层定位） |
| [context.md](context.md) §6 | 现状参考（单工作区绑定代码事实盘，带快照日期） |
| [plans/08-multi-workspace.md](plans/08-multi-workspace.md) | 执行计划（任务分解 W1–W9、批次协议、验收门） |
| [ROADMAP.md](ROADMAP.md) 1.8 | 执行状态唯一源 |
