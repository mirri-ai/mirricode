# 上下文压缩 — 现状盘点（Context）

> 快照日期：2026-09-11（在 2026-09-10 各批（M1+M2+M3-lite+M4+M5、收尾批、加固批）基础上叠加测试修复批：审计发现的代码迎合测试（S1-S8）全部替换为变异可杀的真实断言族，另落地两处 `#[ignore]`d 缺口夹具（core §5.4 前缀竞态取消——SDR-011；embedded US1.4-C2 turn-active 拒绝/锁序——SDR-012），缺口以 SDR-006~012 + ROADMAP 开放项 14~24 登记；TS v1/v2 沿用 2026-09-09 盘点） ·
> 本文只盘点**当前状态与痛点**，不含目标、不含决策（那些见 [compaction-requirements.md](compaction-requirements.md) / [compaction-architecture.md](compaction-architecture.md)）。
> 排查基线：分支 `feat/rust-agent-profile`；TS v2 = `packages/agent-core-v2`，TS v1 = `packages/agent-core`，Rust = `rust/crates/`。

## 1. 三引擎现状事实

### 1.1 触发与策略（三引擎同构默认）

| 参数 | TS v2 | TS v1 | Rust | 备注 |
|---|---|---|---|---|
| 触发/阻塞比例 | 0.85 / 0.85（`fullCompaction/strategy.ts:18-28`） | 0.85 / 0.85（`compaction/strategy.ts:25-31`） | 0.85 / 0.85（`compaction/mod.rs:121-125`） | 一致；比例相等 ⇒ 同步阻塞、无步后检查 |
| 预留空间 | 50,000（同上） | 50,000 | 50,000（`mod.rs:123`） | 一致 |
| 溢出恢复上限 | 3 次（`strategy.ts:23`） | 3 次 | 3 次（`mod.rs:125`） | 一致 |
| 每 turn 压缩上限 | ∞ | ∞ | u32::MAX | 一致 |
| 有效窗口学习 | 有（写入 `fullCompactionService.ts:314-325`、读取 `252-274`；观测值 = 溢出量 × 0.85，取更小，按模型记名） | 有（`full.ts:113-150`，同式） | 有（`mod.rs:802-812`，观测 = 溢出量 × 0.85 向下取整，取更小） | **M1 后口径一致**（Rust 原为 ÷0.85 向上取整的宽松式，已统一为与 TS 同式的保守估计） |
| 无新内容守卫 | 有（`fullCompactionService.ts:488-498`，`lastCompactedTokenCount`） | 有（`full.ts:274-284`） | 有（`mod.rs:466-470`） | 一致 |

**Rust 阈值式自动压缩已接线（M1）**：生产路径经 `turn_flow_config_from`（`embedded_server/mod.rs:341`）从 `[loop_control]` 配置 + 模型目录窗口（`max_context_window` 经 ConfigModelCatalog，未知 = 0）构造策略；`get_or_create_turn_flow` 按 agent model id 建流；模型切换（agent.setModel / session.updateMeta）经 `set_max_context_tokens` 原地更新既有流。窗口未知（=0）时比率触发仍关闭，靠溢出学习兜底。

### 1.2 压缩范围与落位（痛点核心）

- **保留对象只有"真实用户输入"**：按 token 预算 **20,000**（HEAD 最旧 2,000 + TAIL 最新 18,000，边界消息截断保开头/保结尾）。
  - TS v2：`compactionHandoff.ts:196-258`（`selectCompactionUserMessages`）；elision 省略说明消息插入 head/tail 之间（`compactionHandoff.ts:115-130`）。
  - TS v1：`agent/compaction/handoff.ts:61-88, 264-333`，逐行同构。
  - Rust：**保留尾设计（ADR-001，M2）**——`compaction/retention.rs` 切线（预算 = 上下文窗口 × 0.16 从最新往回数、配对完整性只向老退）；头部精华整条粒度 2,000 token（`select_head_user_messages`，不截断、不丢媒体）；超预算时经 `elision_marker_message` 插入省略说明。TS v1/v2 仍为 20k 用户消息截断式。
- **全部 assistant 回复、工具调用与结果不保留原文**，折叠进摘要。摘要指令明说"every assistant message, tool call, and tool result above will be gone"（`agent-core-v2/src/agent/fullCompaction/compaction-instruction.md:17`；v1 同文案）。
- **落位形态**：TS live = `[head 用户消息][elision][tail 用户消息][摘要（末位）]`（`compactionHandoff.ts:97`）；Rust live = replay = `[head 精华][elision?][摘要（前置，CompactionSummary origin）][保留尾]`（`session/replay.rs:74-130`）——**Rust 两端一致（ADR-002），TS 摘要在末位**。
- **条目编号（ADR-007，收尾批落地）**：Rust 侧每个物化进入模型可见历史的条目获得会话累计逻辑编号 `entry_seq`——`ContextMessage` 上的内存字段（`#[serde(skip)]`，不进 wire 载荷），在唯一咽喉点 `push_history`（`context_memory/mod.rs`）分配：直接追加、`step.begin`/`tool.result` 物化、deferred 落槽、压缩落位（elision + 摘要）全部经此；保留条目跨压缩不改号，undo 只移除不回退计数，`clear()` 归零重启；容量拒绝的条目不进历史、不占号。reducer fold 侧按同一物化顺序镜像编号（`SlotLedger`，apply 重建时对保留/新落位条目盖章），undo→压缩组合经 apply 记录的区间括号做偏移校正（记录 `first/lastReplacedEntrySeq` 与 fold 自算括号之差）；已知残余：校正只产出一对均匀偏移（头部/尾部各一），undo 切点落在头部精华区或保留尾区之内时该区段欠校正（`reducer.rs` `reconstruct_compacted_conversation` 文档已载明；单次 undo 形态由往返闸门钉住）。fold 侧 `entrySeq` 盖章随 `snapshot.json` 持久化（派生态通道，与 apply 记录区间字段同类；旧快照无章解析后按同一规则重编号，结果一致）。已知皱褶：reducer `ContextClear` 不清 `conversation.summary`（先于编号即存在），clear 后重放若状态残留 summary 会前置一条摘要消息、使后续编号偏一。已知缺口：rust 引擎的 live `session.undo` 目前不写 `context.undo` wire 记录（`session_handlers.rs:1605-1638` 仅变更内存）——undo 后的 rust 会话重放会回到 undo 前内容（既有保真缺口）；编号的区间括号修正只作用于 wire 记录形态的 undo（TS 写入的日志与测试夹具）。
- **边界截断丢媒体（TS）**：tail 预算边界的用户消息截断只留文本（`compactionHandoff.ts:224-231`，`truncateTextToTokensFromEnd` + `replaceMessageText`）。Rust 保留尾为整条粒度，无此问题。

### 1.3 摘要生成

- TS 摘要输入 = **全部历史（剥除动态工具协议消息）+ 指令**（`fullCompactionService.ts:633-642` `stripDynamicToolContext` + append 指令）——不是"旧段"，因为不存在切线。Rust（M2 后）摘要输入 = **旧段**：begin 时按切线取 `history[..tail_start]`（US3.1 对齐；apply 前 live 重算切线）。
- 指令 = 第一人称交接笔记模板（`compaction-instruction.md`），支持用户自定义说明插值；摘要前缀告知模型"这是你自己的工作笔记"（`compaction-summary-prefix.md`）。Rust 指令头部另注入编号出处约定（ADR-007，收尾批）——"You are summarizing conversation entries #N through #M …" 告知可引用编号且跨压缩稳定，并随重试阶梯逐次重建：每次 L2/L3 丢弃后显式列出被排除的编号清单（不可从本笔记恢复）；区间取 begin 时计划切线的括号，apply 侧仍按 live 重切线自算区间（`round.rs` `build_compaction_instruction`）。指令全文（含/不含丢弃清单两种形态）经字节级 verbatim 测试冻结（2026-09-10 加固批，`compaction_tests`）。
- 摘要自动附带 live TODO（`fullCompactionService.ts:775-781` `postProcessSummary`）。
- 失败重试阶梯（三引擎大体同构）：L1 媒体剥离重试（v1 `full.ts:479-494` 与 Rust `round.rs:512-537` 为压缩路径自有分支；v2 压缩服务自身无此分支，**但其摘要请求经 `llmRequester` 发出，请求层的 413/格式媒体降级链（media-degraded → media-stripped）对操作请求同样生效——catch 无 source 门控，压缩服务的溢出收缩在其耗尽后接管**。2026-09-16 修正，多模态输入域审查批核验 `llmRequesterService.ts:441-495`）、L2 溢出收缩 0.7/0.5/0.35 从尾保留（`fullCompactionService.ts:846-872`、`round.rs:554-583`）、L3 截断/空丢最旧（上限 5）、L4 退避（上限 5）。
- **L2/L3 丢弃产生"摘要盲区"（TS）**：被丢弃的最旧消息不进摘要输入也不被保留，对模型彻底丢失（v1 `full.ts:423-426` 注释自认；`droppedCount` 如实上报）。
- 并发变化检查（prefix-race）：前缀不变 + 追加仅限真实用户输入/hook 上下文，否则取消（`fullCompactionService.ts:831-844` `historySafeToCompact`；Rust `round.rs:444-465`）。
- 防泄漏：输出通道过滤只取文本（`fullCompactionService.ts:812-829` `collectSummary`）。

### 1.4 修剪与微压缩

- TS v1 **micro compaction 已整体禁用**（`agent/compaction/micro.ts:50-53, 110-113`，实验标志已移除，仅 record 回放兼容存活）；v2 无对应物。
- TS v1 另有**常驻的单条工具输出预算**（与空间压力无关）：`agent/turn/tool-result-budget.ts`——单条工具输出超约 5 万字符即替换为 2 千字符预览 + 落盘路径 + 取回指引；这是追加时的防线，与压缩时的投影修剪是两层独立机制。v2/Rust 无对应物。
- Rust **投影级静态修剪常开**（P8a-2）：正规投影带 `micro_compaction_keep_recent=20`、`min_tokens=100`——每次请求中"最后 20 条消息之前的旧 tool 结果"在**模型视图**里被替换为占位符（`src/context.rs:16-21,110-118`；`context_memory/projector.rs:742-770`），不修改内存。自动压缩判定时先按修剪后估算复测，缓解即跳过摘要（`compaction/mod.rs:454-463`）。**这是 Rust 领先 TS 的增强。**

### 1.5 守护与失败处理

- **必须变小守护：仅 Rust 有**——`apply_compaction` 落位前校验新历史不小于原历史则拒绝应用且不写 wire（`context_memory/mod.rs:737-745`；落盘侧注释 `embedded_server/compaction_handlers.rs:193-200`）。TS v1/v2 无此守护。
- 失败保原状：三引擎一致（摘要失败/取消 → 上下文原状继续，可暂时超预算）。
- 崩溃恢复：v2 `CompactionModel` phase，restore 时 running → idle（`fullCompactionService.ts:432-435` `normalizeAfterReplay`）；v1 同构；Rust wire 占位 `full_compaction.*`（`mirri-proto/src/wire.rs:465-474`）。
- 溢出恢复重试：失败步骤闭合 → 压缩 → 新步重试（v2 `fullCompactionService.ts:443-471`；v1 `agent/turn/index.ts:949-965`；Rust `turn_flow/mod.rs:1256-1283`）。
- undo 遇压缩摘要即停（v2 `contextTranscript.ts:173`；Rust `context_memory/mod.rs:899-902`）。

### 1.6 记录、重放与视图

- wire 记录：`full_compaction.begin/cancel/complete` + `context.apply_compaction`（v2 `compactionOps.ts:89-107`、`contextOps.ts:194-200`；v1 `records/types.ts:81,112-119`；Rust `wire.rs:79-107, 465-474`）。
- v2 `apply` 记录带 `keptUserMessageCount/keptHeadUserMessageCount/legacyTail` 供恢复重建形状（`contextOps.ts:162-190`；v1 同有，`context/index.ts:289-299`）；Rust `apply` 记录现带 `retainedTailCount/keptHeadUserMessageCount/elisionMessage`（`wire.rs`，可选字段未递增版本，M2）+ `firstReplacedEntrySeq/lastReplacedEntrySeq`（被替换旧段的编号闭区间）与 `droppedEntrySeqs`（重试阶梯丢弃、连摘要输入都未覆盖的编号清单，ADR-007）+ `keptUserMessageCount`（TS 写入记录的读取字段，收尾批；rust 内核自身恒写 `None`）。出处三元组（区间两字段 + 丢弃清单）的构造已收敛到唯一 helper `apply_compaction_op_fields`（`compaction/mod.rs`，2026-09-10 加固批），三个生产写点（turn_flow 溢出恢复 block 路径 / hooks 自动压缩路径 / embedded 手动压缩 handler）与测试侧 resume 回放共用，跨路径恒等经测试钉住（§4）。
- **Rust resume 保真已修复（M2）**：reducer 按保留计数重建（`mirri-proto/src/reducer.rs` plan_materialization——模拟 live 物质化：vacuous step 移除、deferred 延迟落槽、容量拒绝），US5.4 往返测试闸门保障。
- **Rust reducer 模拟已修三处保真缺陷（M3-lite，2026-09-10）**：media/think 非空判定、legacy toolCalls 不计槽、恢复期容量上限经 `reduce_with_limits` 接线（file:line 以最终代码为准）。
- **TS 记录两分支重放（收尾批，ADR-006 迁移方向）**：Rust reducer 对 TS 写入的 apply 记录按 `keptUserMessageCount` 有无分两支——**newer**（v2 live 恒写该字段）：重建 `[head 保留用户消息][elision?（合成，省略计数为估算）][tail 保留用户消息][摘要（末位）]`，紧凑性判定用 TS 自己的谓词；**legacy**（缺省）：`[摘要（首位）][compactedCount 起的原文尾逐条保真（含 assistant/tool）]`，`compactedCount ≥ 折叠长度` 即 `[摘要]` 单独成段。已知残余：split 记录（`keptHeadUserMessageCount` 在）的 head 段可能含 TS 实际丢弃的消息（记录计数无法区分，`reducer.rs` `TODO(head divergence on split records)`）；TS 记录无区间字段，undo 间隙不可校正。
- **存储根切割 + wire 2.0（收尾批，ADR-006-R2/ADR-005-R1）**：rust 引擎会话存储根改写 `v3_sessions/`（embedded bootstrap / CLI serve+print / ACP / NAPI 组合根全部切换；会话索引随注入根自动跟随），跨引擎恢复自切割起物理不可能；wire 版本链 `1.5→1.6→2.0`，其中 1.6→2.0 为 **identity 迁移**（纯纪元重戳、记录逐字节不变）；`mirri migrate-sessions` 为显式迁移路径（`<bucket>/<id>` 目录整体搬移、不重写日志、幂等，`--copy` 保留源）。
- 人类视图全量：三引擎一致——日志 append-only，转录 reducer 只插入摘要标记不删除（v2 `contextTranscript.ts:205-218`；v1 `services/message/transcript.ts:245-259`；web `api/types.ts:214-220` 注释明确 TUI parity）。
- 事件面：`compaction.started/blocked/cancelled/completed`（v2 `compactionOps.ts:42-60`；Rust `session/event_projection.rs:331-357, 431-448`，completed 携带计量，另携可选 `firstReplacedEntrySeq`/`lastReplacedEntrySeq`/`droppedEntrySeqCount`（ADR-007，收尾批））。

### 1.7 配置面

| 键 | v2 | v1 | Rust |
|---|---|---|---|
| `compactionTriggerRatio` | ✅ 生效（`loop/configSection.ts:34` → `profileService.ts:535` → strategy） | ✅ 声明已移除（2026-09-10 用户裁决，US6.3） | ✅ 生效（M1：`turn_flow_config_from`，`embedded_server/mod.rs:341`） |
| `reservedContextSize` | ✅ 生效（`configSection.ts:33`） | ✅ 生效（流入策略 + completion budget） | ✅ 生效（M1：同上） |
| 保留尾预算 | ❌ 无此配置 | ❌ | ✅ 生效（M5：ratio/tokens 互斥 + NaN/范围校验） |
| 溢出重试上限（`max_overflow_compaction_attempts`） | ❌ 无此配置 | ❌ | ✅ 生效（M5） |
| 容量上限（`[loop_control] max_history_messages` / `max_deferred_messages`，缺省 10,000 / 1,000，0 合法） | ❌ 无此配置 | ❌ | ✅ 生效（收尾批：live turn flow 与 embedded resume 经 `turn_flow_config_from` 映入 strategy；mirri-server restore 与 close-time snapshot 折叠经 `ServerConfig::replay_limits()` → `manager.replay_limits()`，全量重放线程贯通） |
| Rust `SessionScopeConfig.compaction_threshold_tokens`（默认 100,000） | — | — | ❌ **已删除**（feat 提交；原为死字段——`scope/config.rs:104,119`，全仓无读取——字段已随 M1 移除） |

注：`ReplayLimits` 现有两条派生路径——embedded 侧 `turn_flow_config_from`（live 流 + embedded resume）与 `ServerConfig::replay_limits()`（manager 侧 restore/close 折叠）——今日共享同一 live 配置 Arc（`assemble` 接线到 manager 的引擎配置快照，热重载换 Arc 两侧同见），不存在双源漂移；若未来两路径分属不同宿主，需补交叉一致性验证。

### 1.8 入口与呈现

- 手动：TUI `/compact [instruction]`（v1 `apps/mirri-code/src/tui/commands/registry.ts:286-291`）；REST `POST /api/v1/sessions/{id}` action=`compact`（`kap-server/src/routes/sessions.ts:698-712`）；web Composer `/compact` chip + `compactSession` API（`api/types.ts:1046`）；Rust RPC `session.beginCompaction / cancelCompaction`（`embedded_server/routes/session.rs:131-141`）。
- turn 活跃拒绝手动（v2 `fullCompactionService.ts:365-370`；Rust `turn_flow/mod.rs:603-607`）；压缩期间输入缓冲（v1 `turn/index.ts:204-207`）。Rust 手动压缩无可压内容（整段历史在保留预算内 / 无合法切点 / 空历史）时返回 `false`（info 级跳过，非错误；`compaction/mod.rs` begin 以 `CompactionUnable` 区分，`embedded_server/compaction_handlers.rs` 拦截；US2.6/SDR-003，2026-09-10 落地）。
- 呈现：v1 TUI CompactionComponent 可取消（`apps/mirri-code/src/tui/` `editor-keyboard.ts:422`，Ctrl-O 展开摘要全文）、压缩期间命令被拒提示（同目录 `resolve.ts:139,150`）；v2 activityView 压缩活动条目（`activityViewService.ts:154-170`）；web 转录分隔条 + 右侧详情面板（`types.ts:267-270`，快照恢复后 marker 元数据丢失、只剩纯文本分隔条）。
- hooks：PreCompact 注入附加上下文（v2 `externalHooksService.ts:389-402`；Rust `compaction_handlers.rs:82-97`）、PostCompact 通知。
- 遥测：`compaction_finished/failed` 全字段（v2 `fullCompactionService.ts:734-748`）。

### 1.9 死代码与僵尸实现

| 项 | 位置 | 状态 |
|---|---|---|
| `computeCompactCount`（近端保留窗口雏形：maxRecentMessages=4 / maxRecentSizeRatio=0.2 / maxRecentUserMessages / minOverflowReductionRatio——**条数/比例制的用户消息窗口，非 C2 保留尾**） | v2 `fullCompaction/strategy.ts:131-190` | **生产路径无消费方**（src 内仅接口声明与委托实现；测试有引用 `strategy.test.ts`、`fullCompaction.test.ts` 约 20 处）——雏形存在但从未接线；**v1 无对应物**（v1 接口无保留尾窗口参数，src+test 全库零命中） |
| micro compaction | v1 `agent/compaction/micro.ts` | 禁用（见 §1.4） |
| `renderMessagesToText` | v1 `agent/compaction/render-messages.ts` | 零引用（src+test 全库） |
| `compaction_threshold_tokens` | Rust `scope/config.rs` | 已删除（feat 提交；原为死字段） |
| Rust elision 标记（legacy 路径） | `context_memory/compaction_input.rs` | ADR-005 落位路径已启用：`apply_compaction` 经 `select_head_user_messages`/`elision_marker_message` 在头部精华超预算时插入标记，并随 `context.apply_compaction` 的 `elisionMessage` 字段持久化；legacy `select_compaction_user_messages`（head+tail 截断式）仅剩测试消费方，无生产调用方 |

## 2. 痛点机制（"最新交互内容可能丢失"）

按严重度（三引擎共有的根源 + 各自放大项）：

1. **根源（TS）**：落位只保留真实用户消息（20k 截断式预算），最近的 assistant 回复与工具结果全部折叠进摘要——压缩后模型对自己刚说的话没有逐字记忆，只有自己写的转述；转述质量无硬保证。**Rust（M2 后）保留尾以整条粒度、窗口比例预算从最新往回收，最近交互原文保留（ADR-001）。**
2. **摘要盲区（TS 与 Rust 共有）**：L2/L3 重试丢弃的最旧消息不进摘要也不被保留，对模型彻底丢失（`droppedCount` 如实上报但内容不可恢复；Rust 侧收尾批后经 `droppedEntrySeqs` + 摘要指令显式标注"不可恢复"边界——ADR-007 让丢失可见，不消除丢失本身）。
3. **边界截断丢媒体（TS）**：tail 预算边界的用户消息截断只留文本，附件丢失。
4. **Rust 特有（M1/M2 后已消除）**：原"生产 `max_context_tokens=0` → 阈值式自动压缩不触发、resume 丢保留消息、配置全部未接线"三项缺陷均已落地修复（§1.1/§1.6/§1.7）。
5. **TS 特有**：无"必须变小"守护（可能越压越大）；无修剪优先（v1 micro 已禁用、v2 无对应物）。
6. **用户输入侧总体安全**：tail 预算 18k 且保结尾（Rust 为比例保留尾）、prefix-race 守卫、turn 中拒绝手动压缩 + 输入缓冲——"用户刚输入的东西被吞"路径不存在；**用户界面转录三引擎均全量保留**（模型侧丢失 ≠ 用户侧丢失）。
7. **Rust 特有（P0，2026-09-11 审计确认，SDR-011）**：begin→block 落位面无 §5.4 并发变化检查——竞态取消分支只存活于 `compaction_round`（`round.rs:166-170`，生产零调用方的死代码），摘要调用期间追加的助手/工具/后台内容被静默压缩落位（`applied:true`），模型侧内容丢失且不进任何丢弃计数（不可观测）。
8. **Rust 特有（P0，2026-09-11 审计确认，SDR-012）**：手动压缩 dispatch 锁序倒置（memory→flow，文档序 flow→…→memory）——turn-active 拒绝不可达（RPC 阻塞至回合结束）且与回合路径构成 ABBA 死锁（`ENGINE_TURN_TIMEOUT` 无法救援：timer 与阻塞的同步锁同线程）。

## 3. 与需求文档的覆盖差异（差距/回退核对）

| 需求项 | v2 | v1 | Rust | 差距类型 |
|---|---|---|---|---|
| US1.1 自动压力触发 | ✅ | ✅ | ✅（M1 接线；窗口未知时靠溢出学习兜底） | — |
| US1.3 溢出恢复 | ✅ | ✅ | ✅ | — |
| US1.4 手动触发 | ✅ | ✅ | ✅ | — |
| US1.7 无新内容守卫 | ✅ | ✅ | ✅ | — |
| US1.8 修剪优先 | ❌ | ❌（micro 禁用） | ✅ | TS 回退 |
| US2.1–2.3 保留尾 | ❌ | ❌ | ✅（M2：retention.rs 切线） | TS 回退 |
| US2.4 必须变小 | ❌ | ❌ | ✅ | TS 回退 |
| US2.5 头部精华保留 | ✅ | ✅ | ✅（M2：`select_head_user_messages` + elision 接通） | — |
| US3.1 摘要输入=旧段 | ❌（全历史） | ❌（全历史） | ✅（M2：切线前旧段） | TS 回退 |
| US3.4 重试阶梯 | ✅ | ✅ | ✅ | — |
| US4.x 失败/崩溃/撤销 | ✅ | ✅ | ✅（M2 修复 resume 保真） | — |
| US5.1–5.3 日志/投影/人类视图 | ✅ | ✅ | ✅ | — |
| US5.4 恢复保真 | ✅（kept 计数字段） | ✅ | ✅（M2：保留计数重建 + 往返闸门） | — |
| US6.1 配置接线 | ✅ | ✅（声明已移除，US6.3） | ✅（M1 两键生效） | — |
| US6.2 保留尾预算配置 | ❌ | ❌ | ✅（M5：ratio/tokens 互斥 + 校验） | TS 回退 |
| US7.x 呈现与遥测 | ✅ | ✅ | ✅（cancel 契约返回 null） | — |

## 4. 参考

- Rust 测试基线（2026-09-11 测试修复批后复核）：`rust/crates/mirri-core/tests/compaction_tests.rs`（现为 107 个测试 + 1 个 `#[ignore]`d 夹具：策略阈值、自动判定、溢出学习、round 四级阶梯、prefix-race、hook、手动 settle、修剪优先 P8a-2、媒体物化，M2 增补保留尾切线/落位/编排、reducer 计数重建、US5.4 往返保真闸门用例族，M5 增补保留尾配置用例族 ratio/tokens 互斥 + 范围/NaN 校验 + 绝对值预算，收尾批增补编号往返（含 undo→apply 区间校正）、apply 区间/丢弃清单持久化、摘要指令注入、容量键端到端 resume，加固批增补出处三生产写点恒等、vacuous-step 编号消耗直测、摘要指令 verbatim 冻结，测试修复批（2026-09-11）增补 US1.7 无新内容守卫真实断言（替换零断言空壳）、L2 精确阶梯比例与 L3/L4 预算边界、§5.4 前缀竞态取消夹具、三写点拒绝 apply 门控、等值严格变小边界、往返匹配 is_error/note 标志、US1.5 auto×manual 单飞混合分支）；`session_resume_tests.rs` 17 个。reducer 保真修复（media-part、legacy toolCalls、恢复期容量上限）、TS 两分支重建（newer/legacy）与 think "text" fallback 的测试在 `mirri-proto`（现为 133 个：lib 42 + reducer_tests 50 + proptest_reducer 14 + wire_op_tests 17（含加固批 wire 负向契约四例：负计数拒收/编号字段类型拒收/缺 `summary` 拒收/未知字段容忍）+ schema_tests 5 + events_tests 5）。`mirri-cli` 79 个（含加固批 migrate EXDEV 门控、权限拒绝不降级 copy、flat-id 碰撞守护测试；EXDEV 回退体与 source-twin 分支的执行覆盖仍为零——fs-seam 未来触点，见 ROADMAP 开放项 22）。`mirri-embedded` `--lib` 572 个 + 1 ignored（含加固批溢出配置集成测试，落 `turn_flow_config_wiring_tests` 模块——依赖方向决定：`turn_flow_config_from` 为 `pub(crate)`，mirri-core 集成测试不可见；测试修复批增补 auto×manual 单飞与 US1.4-C2 缺口夹具）。两处 `#[ignore]`d 夹具为缺口归属登记（非跳过的已完成项）：core 侧 `should_cancel_the_round_when_history_diverges_during_the_summarizer_call`（`compaction_tests.rs:3214`，缺口归 SDR-011——§5.4 竞态取消未接入落位面）与 embedded 侧 `should_refuse_manual_compaction_when_a_turn_is_active`（`embedded_server/tests.rs:8673`，缺口归 SDR-012——turn-active 拒绝不可达 + 锁序反转），`-- --ignored` 均红于缺口，是对应实现批的验收就绪测试。
- v2 测试：`packages/agent-core-v2/test/agent/fullCompaction/`（fullCompaction、strategy、preCompactAdditionalContext 等）。

## 5. 决策分析：摘要盲区治理（M4，已裁决）

> 2026-09-10 按用户定规：决策项先在此完成背景、痛点、选项与优缺点分析，再交用户裁决。裁决前 M4 不启动实现。

**背景**：压缩摘要的重试阶梯（L2 溢出收缩 0.7/0.5/0.35 / L3 截断空丢最旧）在失败兜底时会**丢弃最旧的消息**——被丢弃的内容既不进摘要输入、也不在保留尾里，对模型彻底丢失（`droppedCount` 如实上报但内容不可恢复；v1 `full.ts:423-426` 注释自认；Rust `round.rs` 同构）。触发条件：摘要请求反复溢出/返回空，超出重试上限后逐层丢最旧再试。

**痛点**：长会话中模型对被丢弃段落的记忆出现"黑洞"——用户追问旧话题时模型一无所知，且无任何界面提示该段已不可达；`droppedCount` 仅存于记录，用户与模型都看不到。

**选项**：

| | A. 出处链 + 显式标注 | B. 丢弃内容纳入保留尾 | C. 推迟到 P-E1（可回溯压缩） |
|---|---|---|---|
| 做法 | `apply` 记录携带被替换条目编号清单，摘要内可引用编号；被丢弃段在记录中显式标注"不可覆盖"边界 | 重试丢弃的最旧内容强制划入保留尾原文保留 | M4 整体与"读历史/搜历史"翻旧账工具一起设计 |
| 优点 | 轻量、可观测、为 P-E1 打地基；模型可在摘要里说"第 37-52 条已被丢弃" | 保真彻底，零丢失 | 一次解决，避免两套标注体系 |
| 缺点 | 不消除丢失本身，只让丢失可见 | 保留尾预算膨胀、压缩触发更频繁、省窗口效果打折；丢弃本因"摘要都装不下"，塞回保留尾可能让下轮请求直接溢出 | 近期盲区无任何改善；P-E1 立项时间未定 |
| 成本 | 低（wire 可选字段 + reducer 标注） | 中（保留尾预算模型改动 + 触发策略联动） | 零（本域）但 P-E1 本身大 |

**已裁决**：A（出处链 + 显式标注，SDR-004，2026-09-10）；实现另立计划。

**落地状态（2026-09-10 收尾批）**：方案 A 已按 ADR-007 落地——会话累计编号、apply 记录区间 + 丢弃清单、摘要指令注入、事件面携带区间与丢弃计数全部接线（现状见 §1.2/§1.3/§1.6；编号地基上的读回工具归 P-E1，**P-E1 已立项为 history-readback 域（proposal，2026-09-10），见 [history-readback-ROADMAP.md](history-readback-ROADMAP.md)**）。本节分析文本按 verbatim 约束保留裁决时原貌。

## 6. 决策分析：跨引擎压缩记录兼容性（已裁决）

> 用户关切（2026-09-10）："如果 rust 跟 ts 的 session 持久化层没有兼容性，就做好兼容性处理切割。"

**背景与事实**（2026-09-10 调查）：
- 跨引擎恢复是**真实场景**，不是理论：两引擎共用同一 sessions 目录（v2 `<homeDir>/sessions`；rust 注入 home 时同布局 `bootstrap.rs:210-218`），且 `MIRRICODE_RUST_ENGINE=ts` 保留 TS 回退路径（`experimental-v2.ts:24-76`）。
- 机械格式兼容成立：wire 版本 Rust 1.6 / TS 1.5，两侧对未知记录类型跳过、未知字段不崩（持久化域 ADR-005/ADR-008；Rust apply 记录的三个可选字段按"可选追加不递增"纪律落地）。
- **语义兼容两个方向都降级**：
  - Rust→TS v2（回退方向）：v2 的 zod schema 无 passthrough，Rust 的 `retainedTailCount`/`elisionMessage` 被**静默剥离**（`wireService.ts:213-228`）；Rust 记录缺 `keptUserMessageCount`/`legacyTail` → `legacyTail` 缺省 true → 误入 legacy-tail 分支，用 Rust 的 `compactedCount`（live 槽位索引）在 v2 自己物化的历史上切片——**近似视图，切点可能错位**（`contextOps.ts:216-233`、`compactionHandoff.ts:54-70`）。
  - TS→Rust（迁移方向）：Rust wire 枚举没有 `keptUserMessageCount`/`legacyTail` 字段，未知字段静默丢弃（`wire.rs:19-25`）→ reducer 走 legacy "clear-everything" 分支（`reducer.rs:160-166`）→ **summary-only**，比 TS 自己恢复（保留 tail）降级更狠。
- 另一处未决事实：TS v1 旧日志的「assistant 消息自带 toolCalls + 后随 tool.result 事件」形状，live Rust 物化时因 pending 注册只认 `tool.call` 事件而**丢弃**这些 tool.result（`context_memory/mod.rs:498,506-509`；v2 fold 同样只认 tool.call，`loopEventFold.ts`）——v1 会话在 rust 下模型视图缺 tool 结果。

**选项**：

| | A. 方向性切割（推荐） | B. 双向互操作 | C. 硬切割（会话打标拒绝） |
|---|---|---|---|
| 做法 | 迁移方向（TS→Rust）做对：Rust reducer 学习 TS apply 记录语义（`keptUserMessageCount`/`legacyTail` 保留计数族 → 重建 legacy-tail 形状），并裁决 v1 legacy toolCalls 形状的物化策略；回退方向（Rust→TS）降级契约**文档化**（legacy-tail 近似视图 + 恢复时日志/遥测标记），不改 TS | TS v2 侧同时学习 Rust 记录语义（zod passthrough + retainedTailCount 族） | 新形态压缩过的会话打标（wire 版本递增或元数据标记），TS 拒绝恢复并明确指引用 Rust |
| 优点 | 投入集中在产品主线（rust 终将取代 TS）；回退是过渡态，容忍降级符合 rust-first 战略；M3-lite 的 reducer 保真修复（Task 3-5）本来就在推 TS→Rust 方向 | 两个方向零降级，回退用户体验完整 | 零静默降级，一切显式 |
| 缺点 | 回退用户对 rust 压缩过的会话得到近似视图（有标记可观测，但内容保真降级） | 改两个引擎；TS v2 zod strip 行为变更有回归面；与"少改 TS"方针冲突；切换后全部作废 | 回退用户遇到"会话打不开"硬错误；需要版本递增/标记纪律（ADR-008） |
| 成本 | 中（Rust reducer 扩展 + 文档） | 高 | 中（标记机制 + 双端拒绝逻辑） |

**已裁决**：A（方向性切割，ADR-006，2026-09-10）；实现另立计划。

**落地状态（2026-09-10 收尾批）**：已按 ADR-006-R2/ADR-005-R1 落地——本节调查所述"两引擎共用同一 sessions 目录"的前提已被**存储根切割**取代（rust 改写 `v3_sessions/`，wire 升 2.0 identity 纪元，`mirri migrate-sessions` 显式迁移），回退方向的降级问题随切根物理消失；迁移方向（TS→Rust 记录语义）按方案 A 落地为 reducer 三分支（rust-native + TS newer + TS legacy，现状见 §1.6）。本节分析文本按 verbatim 约束保留裁决时原貌。
