# 历史读回 — 现状盘点（Context）

> 快照日期：2026-09-10 ·
> 本文只盘点**当前状态与痛点**，不含目标、不含决策（那些见 [history-readback-requirements.md](history-readback-requirements.md) / [history-readback-architecture.md](history-readback-architecture.md)）。
> 排查基线：分支 `feat/rust-agent-profile`；TS v2 = `packages/agent-core-v2`，TS v1 = `packages/agent-core`，Rust = `rust/crates/`。

## 1. 三引擎现状事实

### 1.1 读回能力：三引擎为零，本域全部为新建

- **全库无任何读回/卡片/历史检索实现**：不存在"按编号读回被压缩原文"的工具、RPC、事件或界面入口；不存在目录卡片/现状卡的生成或投影；不存在面向模型的会话历史关键词检索。
- TS v1/v2：压缩后模型侧原文即不可达——20k 用户消息截断式保留之外的全部内容折叠进摘要，无取回通道（用户侧转录全量可见，但那是人类视图，模型不可读）。
- Rust：保留尾整条保留 + 摘要前置（M2 后），被摘要替换的旧段同样无模型侧取回通道；投影修剪（P8a-2）占位化的旧工具输出注明"可按需取回"但**取回通道并不存在**（占位说明是有意为之的缺口标注，非已交付能力）。
- 结论：本域能力**全部为新建**，无任何既有实现可对齐、可回退——这是 rust-first 提案域，不是基线记录。

### 1.2 Rust 已有的编号地基（本域的依赖，非本域产物）

收尾批（2026-09-10）按压缩域 ADR-007 落地的全套地基，本域全部只读消费：

| 地基 | 位置（file:line 以最终代码为准） | 状态 |
|---|---|---|
| 编号分配（live） | `ContextMemory` 计数器 `next_entry_seq_counter`（`rust/crates/mirri-core/src/context_memory/mod.rs:258`；分配咽喉 `push_history`，`#[serde(skip)]` 内存字段不进 wire 载荷） | ✅ 已落地 |
| 编号镜像（重放） | reducer fold `SlotLedger` 盖章 + undo 区间括号偏移校正（`rust/crates/mirri-proto/src/reducer.rs:1004-1073` `reconstruct_compacted_conversation`）；盖章随 `snapshot.json` 持久化（派生态通道） | ✅ 已落地 |
| 区间/丢弃清单字段 | `context.apply_compaction` 可选字段 `firstReplacedEntrySeq` / `lastReplacedEntrySeq` / `droppedEntrySeqs`（`rust/crates/mirri-proto/src/wire.rs:160-180`；可选追加未递增 wire 版本） | ✅ 已落地 |
| 区间自算与哨兵校验 | begin 时计划区间、stamped-0 哨兵断言、live 重切线自算（`rust/crates/mirri-core/src/compaction/mod.rs:186-243`） | ✅ 已落地 |
| 摘要指令注入 | 编号引用约定 + 重试逐次重建 + 丢弃清单显式列出（`rust/crates/mirri-core/src/compaction/round.rs` `build_compaction_instruction`；指令文本已 verbatim 冻结测试，`ffda630f`） | ✅ 已落地 |
| 事件面投影 | `compaction.completed` 携带 `firstReplacedEntrySeq` / `lastReplacedEntrySeq` / `droppedEntrySeqCount`（`rust/crates/mirri-core/src/session/event_projection.rs:431-456`） | ✅ 已落地 |

继承自压缩域的已知皱褶（影响本域，处置归压缩域）：undo 区间括号校正只产出一对均匀偏移（头部/尾部各区段欠校正，`reducer.rs` 文档已载明）；reducer `ContextClear` 不清 `conversation.summary` 的编号偏一；live `session.undo` 不写 wire 记录的恢复保真缺口——详见 compaction-context.md §1.2。

### 1.3 TS：编号亦无，本域无地基可用

TS v1/v2 的 apply 记录无任何区间字段（`keptUserMessageCount`/`legacyTail` 计数族之外无编号概念），上下文历史无 entry_seq 对应物——本域在 TS 侧无可消费的地基（rust-first 提案不要求对齐；TS 回退面维持压缩域 ADR-006 既有文档口径）。

## 2. 今日痛点（读回缺席的真实代价）

1. **摘要盲区可见但不可达**：重试丢弃的编号清单已显式标注"不可恢复"（ADR-007 让丢失可见），模型只能说"这段没了"——连"看一眼再说没了"都做不到。
2. **转述的转述**：多次压缩下旧摘要被再压缩，细节逐层流失；模型对早期内容的把握只剩摘要链条，无原文可查证。
3. **翻旧账靠用户复述**：用户追问旧细节时模型只能让用户重贴——记录里明明有原文，模型却取不到。
4. **投影修剪的占位承诺悬空**：修剪占位说明宣称"原文可按需取回"，取回通道不存在——今日该文案与真实能力不符（R5 视角的自知缺口，本域落地后兑现）。

## 3. 与需求文档的覆盖差异（差距核对）

| 需求项 | TS v2 | TS v1 | Rust | 差距类型 |
|---|---|---|---|---|
| US-R1 按编号读回 | ❌ | ❌ | ❌ | 全部新建 |
| US-R2 目录卡片 | ❌ | ❌ | ❌ | 全部新建 |
| US-R3 现状卡 | ❌ | ❌ | ❌ | 全部新建 |
| US-R4 搜历史 | ❌ | ❌ | ❌ | 全部新建 |
| NFR-R1~R5 | — | — | — | 待实现批验证 |

（差距类型只有"全部新建"一列——本域零实现，如实记录。）
