# 会话一致性 — ROADMAP

> 执行状态与批次计划。设计本身见 [session-consistency-requirements.md](session-consistency-requirements.md) / [session-consistency-architecture.md](session-consistency-architecture.md)；本文件随执行自由更新。
> 裁剪/后续项登记规范：每条必须附一行「用户可见影响」——说明该缺口在用户视角下表现为什么（或"无感知"）。

## 域状态

- 2026-10-13：域立项。需求基线 v1、架构基线 v1（ADR-001~004）待用户评审。
- 2026-10-14：批次 1（读侧同步）实现完成（Task 1-7 收口）。已知裁剪见下。
- 2026-10-14：批次 1.5（子 journal 跟随）+ 批次 2（写侧围栏，Task 1-6 收口）实现完成。批次 2 交付：提交围栏（session.conflict + web 回填/toast）、回合中止（步间 + 工具批前）、state.json 三路合并（ADR-003-R1）、索引对账 D8 锁（ADR-004）。

## 批次规划

### 批次 1：读侧同步（US C1.1 / C1.2 / C4.1 / C4.2）✅
版本探测器 + 增量吸收器 + 前台跟随调度器。双开浏览可见、实时跟随、开销有界。**已完成**（`session-consistency-follow` 实验门控，默认关闭）。

### 批次 1.5：子 journal 跟随（US C1.2 子 agent 半边）✅
吸收器扩展到子 agent journal（`SubagentRegistered` 惰性建子 scope + `child_wire_log` 锚），调度器每 tick 探测主 + 全部子 journal，子 agent 事件帧到达 web（`event.subagent.started` 投影既有）。**已完成**（同门控）。

### 批次 2：写侧围栏（US C2.1 / C2.2 / C2.3 + US C3.1 / C3.2 前置半边）✅
提交围栏（消息落盘前裁决 + `session.conflict` domain code + web 消息回填输入框/toast）、回合中止（步间 + 工具批前，通用 `TurnAbortGuard` 钩子）、state.json 三路合并（ADR-003-R1）、索引对账 D8 锁（ADR-004，锁等待超时跳过本轮）。**已完成**（同门控；web 回填部分无门控，仅在被围栏拒绝的错误到达时触发）。

### 批次 3：元数据与索引一致性收尾（US C3.1 / C3.2 剩余半边）
元数据合并写入器在写路径的全面接线（批次 2 已落 save_state 三路合并核心）；索引单行写数据源对齐（磁盘最新元数据文档，ADR-004）。

### 批次 4：活动徽标（US C1.3）
列表/详情的活动标识，基于批次 1 的版本前进信号。

## 批次 1 已知裁剪（Task 7 收口记录，2026-10-14）

批次 1 的读侧跟随在以下场景有界，超出部分由后续批次收口：

1. **子 agent 新注册的延迟可见**：吸收只折**会话级主 journal**（main agent 全部内容 + 会话域 op）。子 agent 的 `agents/<id>/wire.jsonl` 仅对 B 侧 agents map 中**已存在**的 agent 做同样的吸收（复用同一 drain 机制）；A 侧新注册的子 agent 在 B 侧要等重新打开会话（完整 resume）才可见。
   - 用户可见影响：desktop 派新子 agent 后，web 端（已打开同一会话）要刷新/重开会话才能看到该子 agent 的卡与进度；已在册子 agent 的新内容照常 5 秒内跟随。**批次 1.5 已修复**（`SubagentRegistered` 惰性建 scope，新子 agent 即刻吸收）。
2. **未覆盖的 op 域清单**（吸收保守集之外，debug 日志跳过）：
   - `ContextClear`（context.clear）—— 结构敏感，全清语义与增量吸收不兼容，需批次 2 围栏配合；
   - `ContextApplyCompaction`（context.apply_compaction）—— 压缩重写历史，增量折叠会产生双重摘要，需围栏；
   - `TurnPrompt`（turn.prompt）—— prompt 载荷在消息 op 中已有，吸收时跳过；
   - 其余非上下文域 op（goal/task/cron/工具等生命周期 op）—— 不折内存。
   - 已覆盖：`ContextAppendMessage`（消息折叠 + MessageReceived 事件）、`ContextAppendLoopEvent`（reducer 同一折叠函数）、`TurnEnded`（TurnCompleted 事件 + 悬挂 tool 交互闭合）。
   - 用户可见影响：B 侧跟随期间，对面的清空上下文/压缩/目标变更等操作对 B 的已渲染界面**暂不可见**（界面上保留旧貌），刷新（完整 resume）后与磁盘对齐；消息与回复内容类 op 均正常跟随。**批次 2 后仍部分适用**：围栏保证了 B 发送前发现偏差并刷新，但 B 纯观看期间对面的 ContextClear/压缩操作依然只在其下次 resume 时生效（残余窗口小：这两类操作罕见）。
3. **锚 floor 语义依赖批次 2 的写侧围栏**：known-size 锚是**下界**（floor），不是精确偏移。两个实例**并发**写同一 journal 时（一方的 turn 进行中另一方追加），自身的 `fetch_add` 计数不含外来字节，锚落后于真实写入点，drain 会停在外来行中间的撕裂行——增量吸收暂停（不报错、不退出、不丢数据），恢复路径是完整 resume（spec §5）。严格交替写（一写一跟）时锚精确，已由 Task 7 集成测试锁定。批次 2 的提交围栏（单写者串行化）保证「锚=偏移」后，in-flight turn 期间的外来写入才能在 turn 结束后被增量补齐。**同源的 TOCTOU 窗口**：follow driver 的 in-flight 守卫是 check-then-act——turn 在守卫通过之后、首批 wire ops 落盘之前注册时，drain 仍可能撞上一个刚开始写的本地 turn。撕裂容忍 + 锚 floor 使后果有界（增量吸收暂停，无错无退出无丢数据），批次 2 的提交围栏完全关闭该窗口（见 follow_scheduler.rs 守卫处注释）。
   - 用户可见影响：并发写期间 B 的跟随可能停在半条消息处（该条消息稍后显示不全），之后 B 侧新内容暂停出现，直到 B 刷新页面。**批次 2 已大幅收口**：围栏拦截 B 的发送（不再有"双方同时写"常态路径），回合中止拦截 B 回合中途对面的写入——残余并发窗口仅剩"B 回合中对面写入后、围栏检查前的毫秒级竞态"与 task/cron 通知回合（域外）。撕裂行容错保证任何情况下不丢数据。
4. **徽标（US C1.3）与写侧围栏（C2.x）**：分别在批次 4 / 批次 2。
   - 用户可见影响：批次 2 前——B 发送时不知情覆盖，对面正在跑的内容可能被 B 的消息打断且互相残缺；批次 2 后——B 的发送被温和拦截（消息未发送、回填输入框、toast 说明），对面的回合继续。徽标（"另一入口正在活动"提示）批次 4 前不可用——B 只能靠拦截发生才知道对面动过。**批次 2 已交付围栏；徽标待批次 4。**
5. **前台/后台概念**：serve 传输层订阅即前台；本批 follow driver 的 tick 跟随**会话存活**（resolve 失败即退出）而非订阅数，tick 不随订阅断开停止。
   - 用户可见影响：web 标签页关掉后（只要会话还在该服务进程内存活）跟随照常进行，重开标签页看到的内容已是最新——无负面感知；开销上仅多一份跟随成本（tick 有界增量，可忽略）。
6. **合成 error result 只折内存**：外来 turn 崩溃于 tool.call 无 result 时，吸收侧为悬挂交互铸造合成 error result 进内存（与 resume 一致），但**不回写 wire**——与真实写者视角存在镜像偏差（Task 4 评审接受的裁剪）。
   - 用户可见影响：B 看到的工具卡显示"出错"（如实反映对面的崩溃），但 A 侧重新打开同一会话时该工具卡的错误文案可能不同（A 折叠真实写者的记录）——纯展示文案级的差异，无数据损坏。
7. **索引 tick 并发姿态（ADR-004 边界）**：两进程同时 tick 时各自从磁盘收敛，无锁（幂等全快照行写，最后写者胜且内容一致）；跨进程写者串行化是批次 3 的工作。
   - 用户可见影响：并发对账瞬间列表可能出现秒级的行旧值（两边写者竞争同一行），下一 tick 自愈——对用户近似无感知。**批次 2 Task 6 已修复**：对账整段套 `<home>/session-index.reconcile.lock`（create_new + 过期接管 + 有界等待，与注册表锁同一模式；独立文件——对账内嵌的注册表写穿取 `workspaces.json.lock`，同文件嵌套会死锁），并发对账串行化（双实例同时 reconcile 无行丢失已由测试钉住）。

## 批次 2 已知裁剪与后续项（Task 6 收口记录，2026-10-14）

1. **F-2 残余：重启后首次 save 的覆盖窗口**：三路合并的"共同祖先"是进程内存态（`SessionPersistence::last_written`），进程重启后首次 `save_state` 无祖先可查 → 走整份覆盖路径。若进程恢复 → 首次 save 之间有外来写且本进程不知情，该外来写会被覆盖。
   - 用户可见影响：极端场景下（进程重启后的第一次元数据写，恰逢另一入口刚改了标题/归档状态）另一入口的改动丢失，需重改一次；正常双开使用（无重启）不受影响。已登记后续项：恢复路径（load_state/resume）也登记祖先，把窗口收窄到"读盘后、首次写前"的纯竞态毫秒窗。
2. **cron/task 通知回合不带回合中止 guard**：批次 2 的 `TurnAbortGuard` 装在 `message.send`（wait/fire/steer）路径；cron 注入与 background 任务通知驱动的回合不装（其数据源非用户输入，与不变量 2 的"用户回合"域不同）。
   - 用户可见影响：无直接感知——这类回合写 journal 的行为与批次 2 前完全一致；对面的用户消息若恰在其运行中到达，走既有锚 floor 容错（撕裂行暂停吸收，不丢数据）。
3. **web 回填不覆盖附件**：session.conflict 回填只恢复消息文本到输入框；附件（图片/文件）不回填（附件已上传到服务器，重新选择成本高）。
   - 用户可见影响：被拦截的带附件消息，文本自动回到输入框，附件需重新选择/上传；toast 已提示消息未发送。
4. **对账锁的 stale 接管窗口假设 pass 时长 < 10s**：`LOCK_STALE_TIMEOUT`（10s）假定一轮对账远短于 10s；超大会话量的 home 上一轮 pass 可能超窗——另一进程会把"过期"锁接管，两 pass 交错，退化为批次 2 前的无锁姿态（幂等全快照行写容忍交错，最后写者内容收敛、孤儿清除幂等重删）。
   - 用户可见影响：超大 home（会话数千级）双开时的极端场景下列表可能出现秒级行旧值，下一 tick 自愈——与无锁时代相同的有界偏差，无数据损坏。
5. **conflict 后 snapshot 重载反映本进程最近一次吸收点**：`session.conflict` 围栏拒绝后 web 的 `syncSessionFromSnapshot` 读的是引擎内存态（live session 不回读磁盘），其新鲜度以本进程最近一次 follow 吸收（`FOREGROUND_FOLLOW_TICK_SECS`，默认 2s）为界；活跃写者（对面 turn 进行中）期间重载到的可能是上一次吸收点的快照，后续 tick 继续吸收、界面靠事件流继续追——短暂的"已为你刷新"过诺，非数据错误（评审 I-1，本批按文档方案 B 登记，批次 3 收口为 snapshot 前显式 drain-to-quiescence）。
   - 用户可见影响：冲突被拦截后，页面提示"已为你刷新"，但对面仍在活跃写时刷到的内容可能落后于磁盘最新状态（最多约一个 follow tick 的延迟——通常亚秒到 2 秒，多数场景无感知），之后界面经事件流自动补齐到一致。

## 待办与开放项

- [ ] 用户评审需求/架构基线（ADR-005 已随批次 1.5/2 落地，需回写架构文档评审）
- [ ] F-2 残余收窄：load_state/resume 登记三路合并祖先（见上"批次 2 已知裁剪"第 1 条）
- [x] G3 核实（2026-10-14，Task 6 评审 M-2）：**无需立项——名册已存在**。服务端 `session.snapshot` 的 `subagents` 已从 `list_subagent_registrations` 填充（live registry 供数；`restore_subagents` 经 `seed_subagent_registry` 播种，重启/恢复路径同样覆盖——既有测试 `should_include_member_agents_in_the_snapshot_subagents_roster` 断言非空）；web 侧 `mergeSnapshotSubagents` 也已用快照名册播种任务面板（swarm 卡刷新存活）。conversation-rendering 域登记的"快照 subagents 恒 []"（其 G3 行，源自 rpc.rs 旧版）**过时**——名册填充已落地，登记未同步更新。批次 1.5 又补齐了双开跟随侧（新注册子 agent 事件帧即时到达 web）。剩余待办仅一条：**更新 conversation-rendering 域的过时登记**（G3 名册行、G10 身份分片行中与子代理名册/进度相关的部分）——纯文档修正，无代码工作。用户可见影响：双开场景下子 agent 名册与进度（任务面板路径）已可见；转录不折子 agent 帧是**有意设计**（主身份过滤防骨架气泡，子 agent 走任务面板/Agent 工具卡路径），非缺口。
- [ ] 批次 3：索引单行写数据源对齐（ADR-004 的第二半承诺）
- [ ] 批次 4：活动徽标（US C1.3）
