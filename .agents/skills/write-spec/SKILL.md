---
name: write-spec
description: Create or revise a docs/specs domain suite (requirements / architecture / context / ROADMAP + glossary registration) in the mirri-code repository. Use when drafting a new domain's spec documents, revising an existing suite, or when the user asks to "写需求文档/设计文档/建域/评审 spec".
---

# Write Spec（docs/specs 域文档撰写）

为 `docs/specs/` 建立或修订一个领域的完整文档套件。本文是**流程骨架**；治理规则、条目格式、评审清单的全文见 [docs/specs/AGENTS.md](../../../docs/specs/AGENTS.md)（读 docs/specs 下任何文件前先读它）。

## Quick start

一个域的完整套件 = 5 个文件，同批交付：

```text
docs/specs/<domain>-requirements.md    需求（产品语言，假如-当-那么验收）
docs/specs/<domain>-architecture.md    架构（机制、接口形状、ADR）
docs/specs/<domain>-context.md         现状盘点（代码事实，带快照日期）
docs/specs/<domain>-ROADMAP.md        状态/开放项/批次（唯一进度源）
docs/specs/glossary.md                 跨域术语（同批登记，升版本号）
```

金标准参照：`docs/specs/provider-credential-*.md`（2026-10-28 建域，两轮评审修复后零阻塞项）——写新域之前先读这一套。

## Workflows

### 1. 动笔前：事实与边界（防幻觉）

- [ ] 读 `docs/specs/AGENTS.md` 全文（尤其「四禁」「R1-R9」「SDR/ADR 格式」「验收句式」）
- [ ] 读相邻域的边界节，划清本域/邻域分工（边界表里写「引用不重复」）
- [ ] 每条代码事实用工具核实后才写入 context（file:line 当场验证；竞品调研注明 commit）
- [ ] 检查 `docs/specs/glossary.md` 与 `proposals.md` 是否已有相关条目/提案

### 2. 撰写顺序

1. **context 先行**（现状盘点 + 竞品机制表）——事实基础决定后面的裁决质量；
2. **requirements**（六层结构：领域边界表+术语表 → 角色/场景/功能闭环 → Epic×User Story → 契约级裁决 D-M* → 验收汇总表 V* → SDR 记录 → 未来增强提案）；
3. **architecture**（架构总览图 → 数据模型 → 服务层 → 各消费面 → 机制映射表 → ADR 记录；每个 ADR 含被否决备选）；
4. **ROADMAP**（覆盖状态表、开放项、实施批建议——只做状态，不承载需求）；
5. **glossary 同批登记**（收收录条件：出现在 ≥2 个域或共享面；域内私有术语不入）。

### 3. 硬规则（违者必被评审打回）

| 规则 | 出处 |
|---|---|
| 验收句式用中文：「**假如**…、**当**…，**那么**…」，禁止中文句内嵌 `Given/When/Then` 英文标记 | AGENTS_CN 验收句式 |
| requirements 是产品语言（产品经理无工程师在场能读懂）；技术形状只在 architecture | 四禁之四 / R8 |
| 设计文档不含执行状态、file:line 现状、迁移叙事——现状归 context | 四禁 |
| 需求/设计变更回写为 SDR/ADR（含背景、决策、被否决备选、影响范围），历史追加不改写 | SDR/ADR 约定 |
| context 每行事实带 file:line；glossary 条目无代码无机制 | 各自约定 |
| ROADMAP 不承载需求；requirements 不含进度 | 单一状态源 |

### 4. 自检后送审

- [ ] R1-R9 逐条过（AGENTS.md 评审清单，每条是判断题）
- [ ] 「修改设计文档五问」过一遍（AGENTS.md 尾节）
- [ ] 派 **2 个**（不超过）reviewer subagent：一个查需求完整性/R1-R9/反向幻觉抽查，一个查架构机制一致性/file:line 抽查/相邻域冲突
- [ ] 修复全部 high/medium；low 酌情；评审记录与修复要点回写 ROADMAP
- [ ] 用户既定裁决（SDR 已记录的）不作为 finding 重提

## Advanced

- **修订既有域**：只在设计变更时改 requirements/architecture（附 SDR/ADR）；现状变化改 context（更新快照日期）；进度只改 ROADMAP。
- **建域命名**：`<domain>-` 前缀四件套；域内再分目录（如 `tui/`、`rust-core-design/`）时 ROADMAP 可为目录级。
- **引用竞品**：调研结果进 context §2（机制表 + 借鉴/不借鉴裁决对照），正文只引用不复制。
