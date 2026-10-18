# Rust 内核域计划 — 13: 会话分叉管道实施（session fork pipeline）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **文档类型：计划类**（按 `docs/specs/AGENTS.md` 治理约定）——本文件只持**任务分解、批次协议与验收标准**。
> 设计（终态架构 + 裁决 ADR-F1~F5）：[../12-session-fork.md](../12-session-fork.md) · 需求契约：[../../session-management-requirements.md](../../session-management-requirements.md) Epic E5 · 管道现状证据：[../context.md](../context.md) §9。
> 上游: 会话管理域 E5b 需求批次（2026-10-09 裁决收口）。

**Goal:** 按设计 [12-session-fork.md](../12-session-fork.md) 落地 Rust 内核的分叉管道，用户可感知验收：fork 出的会话重启后历史完整（会话日志 wire journal 的截取复制）、可从任意已收尾回合分叉、多成员全量复制、cron 不带、锚点校验护栏（活跃回合不作锚点）、`Fork of 源标题` 命名 + 血缘落盘。

**Tech Stack:** Rust workspace（mirri-proto / mirri-core / mirri-persistence / mirri-embedded / mirri-session-index）+ 契约联动（TSV/contract-e2e fixture）。

## 设计基线与裁决门

- ADR-F1（cron 域 ops 全过滤）｜ADR-F2（turnId 用 u64）｜ADR-F3（归属标识零改动）｜ADR-F4（锚点已收尾即放行——修订版）｜ADR-F5（agentId 废弃不移除）——均随设计文档本批定稿，实现中若需变更走新 ADR 追加。
- 需求侧 SDR-005/006/007/010（修订版）/011/012 + 7c 已全部用户裁决（2026-10-09）。
- **执行序硬约束**：
  1. **F1 必须先于 F2**——截取核心（含过滤表）是分叉点参数的地基；F2 的 turnId 校验依赖 F1 的截取算法语义。
  2. **F3 与 F4/F5 无依赖可并行**，但 state.json 字段链（forkedFrom/agents）一旦写入就进入持久层兼容面——schema 变更走"新增字段带默认值"模式，老文件读取零迁移。
  3. **F6（收口 e2e）依赖 F1-F5 全部**——V1-V8 验收标准逐条对应设计 §5。

## Global Constraints（继承 08）

- 本地门禁：`cargo test --workspace` 全绿、`cargo clippy --workspace --all-targets -- -D warnings` 零告警、mirri-core zero-IO 守卫 0 命中（截取管道的文件读写走 persistence 层既有原语或 trait 注入）、受影响 TS 包 typecheck + vitest 全绿。
- 测试命名 Given-When-Then；不改实现迁就旧测试（除非实现真有 bug）。
- changeset 走 gen-changesets skill；`major` bump 任何时刻需用户确认（硬门）。
- 契约变更同步事实源链全环节（registry ⇄ TSV ⇄ EMB 路由表 ⇄ NAPI ⇄ contract-e2e fixture）——F2 扩参与 F5 的 agentId 语义变更都要过这条链。
- wire log append-only；fork 管道只写**新会话**的日志，源会话日志零触碰。
- 环境护栏：跑大 TS 套件前查 swap；每批开始前 `git status` 干净。

## 每批循环（SDD 协议，同 08）

brief（侦察 + 设计，`.superpowers/sdd/session-fork/task-<Fn>-brief.md`）→ TDD（红→绿）→ 评审（含 mutation）→ 本地门禁 → changeset → commit → ROADMAP/本档状态更新 → 批次报告。

## 停止规则（同 08）

用户裁决门 → 记队列跳过；mutation REJECT 两轮 → 暂停上报；环境阻断 → 记录替代验证；新架构问题 → 记 ROADMAP §5 候选不当场改。

## 任务总表

| 任务 | 主题 | 优先级 | 依赖 | 状态 |
|---|---|---|---|---|
| F1 | 截取核心：wire 读取→turn 括号边界→过滤表→成员级截取→重写 | P0 | — | ✅ `5917c86d` |
| F2 | 契约扩参：turnId/metadata + 锚点错误语义 + agentId 废弃（ADR-F5） | P0 | F1 | ✅ `586bd6c8` |
| F3 | state.json 铸造：forkedFrom/agents 字段链 + `Fork of 源标题` 命名 | P0 | — | ✅ `e5e9cf73` |
| F4 | 锚点校验护栏（ADR-F4 修订版）+ 原子落位回滚 | P1 | F1 | ✅ `d0bc1015` |
| F5 | blobs 复制 + tasks/snapshot 不复制的实体处置落地 | P1 | F1 | ✅ `c3ee2a3b` |
| F6 | 收口批：V1-V8 e2e + 契约联动（TSV/fixture/pin）+ ROADMAP 收口 | P1 | F1-F5 | ✅ 收口 commit（实现 + 文档两笔） |

## F1: 截取核心

**交付**：`mirri-core` 新模块（建议 `session/fork_pipeline.rs`）：

- [x] `truncate_ops_at_turn(ops: Vec<WireOp>, target_turn: u64) -> Vec<WireOp>`——按 turn 括号结构截取（turn.prompt 的 turn_id > target 即停止，硬规则：目标回合的 turn.ended 为最后一条）。
- [x] 过滤表（ADR-F1）：cron 域 ops（add/delete/cursor）全剔除、`forked` 剔除、goal 域字段级过滤（`goal.create/clear` 原样；`goal.update` 的 `status` 强制改写为未启动——op 名与字段见设计 §3.3 过滤表）、其余原样。
- [x] 成员级入口：对 `agents/<id>/wire.jsonl` 各自执行同算法（各成员自己的括号结构）。
- [x] TDD：红测覆盖——3 回合截第 2（含边界记录恰为 turn.ended）、cron 过滤、goal 执行许可过滤、未知 op 保留、末行撕裂输入容错。

## F2: 契约扩参

- [x] `session.fork` 参数扩展 `turnId?: number`、`metadata?: object`（TSV 行 + `ForkSessionParams` 结构体 + 客户端 alias）。
- [x] 错误语义：turnId 不存在 → INVALID_PARAMS（含可用回合范围）；turnId = 活跃回合 → TURN_ACTIVE 族错误码。
- [x] `agentId` 语义改忽略 + debug 日志（ADR-F5）；TSV notes 更新。
- [x] 契约事实源链全环节联动 + contract-e2e fixture。
- [x] TDD：错误语义三条（不存在/活跃/正常）+ agentId 传入被忽略。

## F3: state.json 铸造

- [x] scope `SessionMetadata` 增 `forked_from: Option<String>`（带 TS alias `forkedFrom`）。
- [x] `PersistedSessionMetadata::from` 增映射（消灭 persistence.rs:144 的硬编码 None）。
- [x] fork 管道铸造：`Fork of <源标题>`（源标题不可得退化源 id）、forkedFrom = 源 id、agents 清单按截取后成员重建（对齐 v2 state.json 形状）、自定义元数据合并（剔除 goal 键）。
- [x] TDD：V5 验收（标题 + forkedFrom 落盘）+ 老文件 round-trip（无 forkedFrom 的存量 state.json 读写不丢字段——load-mutate-save 路径回归）。

## F4: 锚点校验护栏 + 原子落位

- [x] 锚点校验护栏（ADR-F4 修订版）：解析锚点必须命中已收尾回合的 `turn.ended`——显式 `turnId` 指向已收尾回合或缺省取最近收尾回合即放行（源会话可有活跃回合）；指向活跃回合 → `TURN_ACTIVE` 拒绝。**不引入** `session_is_busy` 前置门、**不迁移** v1 的全量忙碌检查。
- [x] 落位顺序：目标目录（journals + blobs）→ state.json → 索引登记 → 事件；任何失败 → 删目标目录 + 清内存态 + 不进列表。
- [x] TDD：V4（修订版——进行中回合收尾点/缺省/活跃锚点三态）、V6（注入失败无残留）。

## F5: 实体处置

- [x] `blobs/` 复制（或按截取流 blobref 选择复制——brief 期定夺，倾向全量复制：blob 量小且实现最简）。
- [x] `tasks/`、`snapshot.json` 确认不进复制面（管道根本不触碰这些路径即可，写测试钉住目录清单）。
- [x] TDD：fork 后目标目录实体清单恰为预期集合。

## F6: 收口批

- [x] V1-V8 e2e（设计 §5 全表）——含 V7 前缀确定性（对拍源会话同分叉点折叠后的请求前缀字节）。
- [x] pin 联动（mirri-proto registry / embedded 路由）+ `createChild` 复用管道的回归（`dispatch_create_child_session` 走同一管道 + 元数据扩展）。
- [x] ROADMAP 收口 + 本档状态更新 + 会话管理域 context §1.3 的 Rust 现状刷新。
