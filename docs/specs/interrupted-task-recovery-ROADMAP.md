# 中断任务恢复（interrupted-task-recovery）ROADMAP

> 状态/开放项唯一源。需求见 [interrupted-task-recovery-requirements.md](interrupted-task-recovery-requirements.md)；架构见 [interrupted-task-recovery-architecture.md](interrupted-task-recovery-architecture.md)；现状见 [interrupted-task-recovery-context.md](interrupted-task-recovery-context.md)。本文只做状态管理，不承载需求或设计。

## 覆盖状态（2026-10-28 建域）

| Epic | 主题 | 状态 |
|---|---|---|
| A | 中断判定（退出记录机制） | ✅ 已文档化 |
| B | 识别与状态 | ✅ 已文档化 |
| C | 恢复通知（第一呈现通道） | ✅ 已文档化 |
| D | 会话树中断徽标（第二通道/兜底） | ✅ 已文档化 |
| E | 续跑（从中断处继续） | ✅ 已文档化 |
| F | 非功能 | ✅ 已文档化 |
| 提案 P-1..P-3 | 超出本批的增强 | ✅ 已登记（requirements §11） |

## 实施进度（引擎侧，2026-10-28 更新）

| 批次 | 范围 | 状态 |
|---|---|---|
| 批次 1（rust core） | 生命周期哨兵（含 SIGTERM/优雅退出记录）、尾部扫描、合成收口回填、`recovery.*` RPC（list/resume/resume_all/dismiss/dismiss_all）、启动检查与列表覆写、interrupted 呈现值全链路透传（枚举 `SessionTurnOutcome::Interrupted` + `outcome_of`/`outcome_str` 拦截） | 🚧 已在 worktree `.worktrees/interrupted-recovery` 实现完成（Task 1-8），待合并主工作区后转 ✅ |
| 批次 2（web 消费面） | 类型与 RPC client（`recovery.*` 五方法）、状态层恢复清单与乐观动作（ADR-010 读面合并）、i18n（recovery 命名空间 + 徽标键）、恢复通知卡片（RecoveryNoticeCard）、会话树徽标 + 就地继续/忽略 + 工作区中断计数 | 🚧 已在 worktree `.worktrees/recovery-web` 实现完成（Task 1-7），待合并主工作区后转 ✅ |

> 批次 1 覆盖 V1、V2、V4、V5、V8-V15 的引擎侧；web 消费面（通知卡片、徽标渲染、i18n、`recovery.*` client）属批次 2。

## 评审记录

| 轮次 | 评审者 | 范围 | 结论 |
|---|---|---|---|
| 1 | requirements-reviewer（需求完整性 / R1-R9 / 反向幻觉 / 用户裁决忠实性） | requirements + glossary IT 节 | PASS-WITH-FINDINGS：10 条（F-01 B.4 前提断言无现状支撑 high、F-02 验收缺「当」段、F-03 「服务中断/运行中断」双规范名、F-04 多次重启通知行为未定义、F-05 glossary 越界、F-06 验收覆盖缺口、F-07 成因不明分支悬空、F-08 会话树/列表两叫法、F-09 现状比较句残留、F-10 徽标词指称不精确）→ 已全部修复 |
| 1 | architecture-reviewer（机制一致性 / file:line 抽查 / 相邻域冲突） | architecture + context | PASS-WITH-FINDINGS：12 条（F-01 跨端口存活实例误判异常 high、F-02 append-only 日志上「标记已处理」无机制 high、F-03 turnId 来源、F-04 interrupted_at 取值、F-05 interrupted 被归并进 failed 族、F-06 词汇管辖权未走 SDR、F-07 ADR 缺影响范围、F-08 双实例回填竞态、F-09 时间语义三处不一致、F-10 映射表 F 行不全、F-11 徽标词误称、F-12 fork/压缩/排队/窗口期边界）→ 已全部修复；context 事实清单 16/16 核验通过 |

修复要点（同批回写）：需求侧——B.4 重写（等待类挂起的跨重启存活不再作为前提，其现状为进程内存态、恢复不重建，事实已补入 context §2.1）；规范名统一「运行中断」（「服务中断」废弃并入）；新增 C.8 跨重启重现 + V15-V17；V4-V14 补齐「当」段；A.5 补存活确认硬约束。架构侧——启动检查增加**存活门**（launch.json 携带 port/pid，存活实例不判定不删除）+ 目录 rename 原子占位防双实例竞态（ADR-001 修订）；中断状态「已处理/过期」改为**读时推导**、不改写追加型日志（新增 ADR-008）；合成收口 turnId 推导与重放乱序容错（新增 ADR-007）；§2.1 拦截点清单（归并前特判，防误落 failed 族）；六条 ADR 补影响范围；映射表 F 行展开。跨域——turn-scheduling 追加 SDR-006（interrupted 成因值词汇裁决）；glossary L3 同批扩充 + IT1/IT2 收敛为纯术语（机制回指域内 spec）。

| 2 | requirements-reviewer（修复验证复审） | requirements + glossary IT/L3 + SDR-006 | **PASS**：F-01..F-10 十项全部验证通过，无新引入违规 |
| 2 | architecture-reviewer（修复验证复审） | architecture + requirements 交叉 | PASS-WITH-FINDINGS：F-01..F-12 十二项全部验证通过；新发现 3 条（R-1 同端口存活门探测命中自身与「天然通过」注矛盾 high、R-2 端口被无关进程复用时误判存活致目录永滞留 low、R-3 E.3 推导锚点误指 §1.3 low）→ 已修复（§1.2 伪代码加同端口免探测特例 + pid 裁决辅助与判死清理；§3.1 步骤 2 改为路径无关的 wire 日志推导） |

## 开放项（需人工/后续裁决）

- **O-1** `recovery.*` RPC contract 的注册位置与 klient 契约文件形态——实施批确定，不阻塞设计。
- **O-2** desktop-lite 的实例标识确认：其守护进程端口固定 55828、复用同一 mirri home，lifecycle 目录按端口隔离即天然成立；若未来出现多工作区多 lite 实例并存形态，再扩展 instance-id 规则。
- **O-3** 恢复通知卡片在 web 内的挂载点细节（空白草稿页 vs 全局布局层）：需求只钉「非模态、列表就绪后出现」，视觉落点实施批与设计走查定。
- **O-4** 跨域对齐 P-028（未消费任务结果提示）的呈现整合：两域实施批排期时统一通知组件语义。
- **O-5** C.5 的全局续跑并发上限 3 显式递延（批次 2 裁决）：`resume_all` 现为逐会话 fire 回合（不同会话并发，无全局上限）；fire-mode 排队兜的是同会话单活动回合，非全局并发。递延理由：上限的消费者（大量并发中断恢复）在真实使用中尚未出现，且配置面（config section + 白名单 + web 设置）成本独立成批；到期触觉为多工作区批量恢复的实测反馈。批量续跑的逐批进度反馈（"已启动 N"瞬时提示）同样并入本递延，与并发上限一并裁决。若上限最终不做，需求侧需补 SDR 修订 C.5 措辞。

## 实施批建议（plan 拆分输入，非状态）

1. 生命周期哨兵 + 启动判定 + SIGTERM 补齐（Epic A + 架构 §1/§5）——rust，先行；
2. 中断识别回填 + 列表读面合并 + `recovery.*` RPC（Epic B + 架构 §2/§3）——依赖批 1；
3. web 恢复通知卡片 + 会话树徽标 + i18n（Epic C/D + 架构 §4）——依赖批 2；
4. 续跑链路 + 后台任务提示（Epic E）——依赖批 2；
5. 与 P-028 呈现整合 + 桌面壳联动验证——收尾批。
