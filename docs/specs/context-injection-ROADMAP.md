# 上下文注入域 · ROADMAP（context-injection-ROADMAP）

> 状态/开放项唯一源。需求/设计内容见同域 requirements / architecture 文档，本文不承载。Rust 迁移总体计划见 `rust-core-design/ROADMAP.md` 与 `plan/rust-*.md`，本文只列本域相关项。

## 覆盖状态（2026-09-16 首次基线）

| 能力 | 文档化状态 |
|---|---|
| 系统提醒信封契约（US1） | ✅ 本次新建 |
| 条件注入框架（US2） | ✅ 本次新建 |
| 事件型系统提醒 ×10（US3） | ✅ 本次新建 |
| 嵌入式提醒（US4） | ✅ 本次新建 |
| 事件驱动消息通道契约（US5） | ✅ 本次新建（内容语义归各域） |
| 生命周期（压缩/撤销/重放，US6） | ✅ 本次新建 |
| 频率控制与幂等（US7） | ✅ 本次新建 |
| 来源标识体系（US8） | ✅ 本次新建（类型清单在 context §1.6） |
| 非功能（US9） | ✅ 本次新建（背压上限以提案 P2 记录，非 TS 现状） |
| 未来增强提案（US10） | ✅ P1–P4 四项登记 |

## 开放项

### Rust 对齐缺口（TS 支持但 Rust 缺失/未接线）

| # | 项 | 证据 | 优先级建议 |
|---|---|---|---|
| O1 | interruptionReminder **已接线**（2026-09-17 落地，commits 606760d6/2d12eb97/60987c07） | `mirri-core/src/interruption_reminder.rs` 全文件；`mirri-proto/wire.rs:609-610` | ~~P0~~ 已落地。实施计划：`docs/superpowers/plans/2026-09-16-context-injection-rust-wiring.md`（Task 1-5，已完成） |
| O2 | Swarm 模式提醒**已接线**（2026-09-17 落地，commits 7ac4dddd/908c539c） | `mirri-core/src/swarm.rs:137-178`；`permission/mod.rs:113` | ~~P1~~ 已落地。实施计划：同上（Task 6-7，已完成） |
| O3 | goal 事件型提醒 ×3：cancelled / budget-stop **已接线**（2026-09-17 落地，commits f9106d1b/501e5c61）；fork-cleared **不适用** | TS `goalService.ts:671,853,1143` vs Rust `injection/goal.rs` 仅三态常驻 | ~~P1~~ 前两项已落地（Task 8-9）。fork-cleared 已裁决**不适用**：TS fork 是跨会话分叉（新会话无 goal），Rust `fork_agent` 是同会话内新 agent 且 goal 为会话级单例（fork 保留 goal）——待 Rust 出现跨会话 fork 能力再评估 |
| O4 | sessionInit（/init 完成提醒）缺失（Rust 无 /init） | TS `sessionInitService.ts:116` | P2——随 /init 整体能力规划 |
| O5 | Ultimate Reminders 环境变量（`$os/$shell/$now/$cwd`）缺口 | `plan/rust-builtin-profiles-parity.md:72` | P2（已在该计划登记） |
| O6 | skill 激活注入投递通道 P1 分歧（用户侧消息 vs system-reminder） | `plan/rust-tool-calling.md:3098,923` | P1（已登记，待裁决落定后回写本域 ADR-004 接缝） |

### 文档侧待办

| # | 项 | 说明 |
|---|---|---|
| D1 | stop hook / hook 附加上下文的产品语义无归属域 | 本域只覆盖通道侧（US5）；**已裁决（2026-09-16）：搁置，不立项**——通道侧覆盖维持现状 |
| D2 | 会话恢复时注入物重放的正向夹具 | conversation-rendering-context.md:122 自认缺口（SkillTool 式注入与压缩后任务提醒的落盘无专门测试夹具） |
| D3 | v1 legacy 路径的淘汰时点 | `[v1]` 标注项随 `MIRRICODE_LEGACY_FLAG` 退场一并清理 |

### 人类确认项

| # | 问题 | 状态 |
|---|---|---|
| C1 | Rust 的"每步重扫历史"替代 TS 的"位置记账 + splice 对账"（ADR-003 两条路线）——语义等价但成本模型不同，是否接受为最终态？（涉及是否要求 Rust 补记账机制） | 待裁决 |
| C2 | Rust deferred 缓冲有上限、TS 无上限——基线是否统一为"有上限"（即提案 P2 转正）？ | **已裁决（2026-09-16）**：TS 保持现状不回补上限；Rust 的"有上限 + 告警"为最终态方向（提案 P2 以 Rust 形态成立） |
| C3 | TitleInjector（Rust 独有，标题生成提醒）是否需要回写 TS 基线作为增强？ | **已裁决（2026-09-16）**：TS 不动；TitleInjector 保留为 Rust 独有增强，不回写 TS 基线 |

## 历史记录

- 2026-09-16：首次基线建立（四件套新建）。来源：v2/v1/Rust 三路代码侦察 + 既有文档审计。同日完成双评审（需求完整性 + 架构一致性）并修复全部发现：删除 2 处幻觉（capability_gap 去重、工具结果注入字段形状）、修正 1 处 v1 引用行号、补齐压缩省略信封 / 动态工具 schema 落点 / 钩子阻断通道披露、治理措辞清理。评审确认的关键判定（interruptionReminder 与 Swarm 未接线、goal 事件×3 缺失、TitleInjector Rust 独有）均与代码核实一致。
- 2026-09-16：O1-O3 立实施计划 `docs/superpowers/plans/2026-09-16-context-injection-rust-wiring.md`；goal_fork_cleared 裁决为不适用（Rust 同会话 fork 模型）；Rust 重扫式扫描域细节（压缩后内存态 + 反向提前退出）补入 context §3。C1（重扫式 vs 记账式是否接受为最终态）仍待用户裁决。
- 2026-09-17：O1-O3 执行落地（实施计划 Task 1-10 全部完成）：打断提醒全链路接线（606760d6/2d12eb97/60987c07，含 wire restore 重放）、蜂群进出提醒 + `SwarmModeService` 生产替换（7ac4dddd/908c539c）、goal 取消与预算停提醒（f9106d1b/501e5c61）；fork-cleared 维持不适用裁决。门禁：`cargo test --workspace` 全绿、`cargo clippy --workspace --all-targets -- -D warnings` 零告警、`node rust/scripts/check-zero-io.mjs` exit 0。context §3 对应行已同步改 ✅。
