# Skills 技能系统 ROADMAP

> 本域状态/开放项唯一维护点。需求见 [skills-requirements.md](skills-requirements.md)、逐技能规格见 [skills-builtin-skills.md](skills-builtin-skills.md)、机制见 [skills-architecture.md](skills-architecture.md)、现状证据见 [skills-context.md](skills-context.md)（快照 2026-09-12；§10 为 2026-10-10 内置技能增量）。
> 评审状态：2026-09-12 需求完整性（design-reviewer）+ 架构/现状一致性（architecture-reviewer）双线评审，4 轮 review-fix 闭环（3 major / 多项 minor / 2 处治理修正），**终轮双线零 comments**。
> Rust 侧整体迁移追踪见 `docs/specs/rust-core-design/ROADMAP.md` 与 `plan/rust-*.md`——本文只登记技能域相关项，不重复。

## 1. 覆盖状态（2026-09-12 首建基线）

本域为新建四件套（此前无 skills-* 文档），首建即全量覆盖，无增量遗留。

| 能力块 | 需求条目 | 覆盖 |
|---|---|---|
| 技能格式与元数据（US1.1–1.3） | ✅ 已文档化 |
| 发现与来源层级（US2.1–2.3） | ✅ 已文档化 |
| 用户斜杠调用与多端入口（US3.1–3.3） | ✅ 已文档化 |
| 技能候选的来源标识与来源过滤（US3.8，2026-10-25 随 SDR-009 增补，采纳 §11 提案 6） | ✅ 已文档化；实现状态见 [plans/05-skill-source-badge-filter.md](skills/plans/05-skill-source-badge-filter.md) |
| 模型自动调用 / Skill 工具（US4.1–4.4） | ✅ 已文档化 |
| 注入形态与门控（US5.1–5.5） | ✅ 已文档化 |
| 插件系统全套（US6.1–6.10） | ✅ 已文档化 |
| 动态一致性（US7.1） | ✅ 已文档化 |
| 热更新（US7.2，2026-10-23 随 SDR-008 增补） | ✅ 已文档化 |
| 呈现与观测（US8.1–8.3） | ✅ 已文档化 |
| 错误路径（US9.1–9.2） | ✅ 已文档化 |
| 非功能（NF1–NF8） | ✅ 已文档化 |
| 内置技能逐技能规格（write-goal / update-config / mcp-config / custom-theme / import-from-cc-codex / sub-skill 家族） | ✅ 已文档化（[skills-builtin-skills.md](skills-builtin-skills.md)，2026-10-10 增量；代码事实见 context §10） |

### v1-source 标记的能力（Rust 对齐时须注意仍被消费的 v1 面）

| 能力 | 现状 |
|---|---|
| [v1] `services/skill` REST 防腐层（`ISkillService` 投影与错误码映射） | v1 server 退役后无运行时消费者；kap-server 以其为逐字节兼容基准平移（context §6.2） |
| [v1] `${MIRRICODE_SKILL_DIR}` / `${MIRRICODE_SESSION_ID}` 占位符名 | v1 引擎与文档站口径；v2 已改短名（O1） |
| [v1] `MIRRICODE_PLUGIN_ROOT` 插件 env | v1 manager 与文档站口径；v2 为 `MIRRI_PLUGIN_ROOT`（缺陷 D1） |
| [v1] 单二进制 MCP node 回退（`__plugin_run_node`） | 两代引擎均有此回退（v2 manager.ts:728, 764-785，另含 Electron 分支）；断链点在 TUI 子命令检查的 env 名与 v2 注入的不一致（缺陷 D1，见 §2） |

## 2. 回归 / 缺口登记（TS 实际 > 文档站承诺，或代码内漂移）

> 详证见 context §7。这些不是需求文档的缺口，而是**现状侧**需要修文档站或由 Rust 侧裁决收敛的点。

| # | 项 | 备注 |
|---|---|---|
| G1 | 技能目录占位符双轨（文档站 `MIRRICODE_*` vs v2 `MIRRI_*`，用户按文档写不展开） | 关联开放项 O1 |
| G2 | 插件 hooks/解释器入口 env 双轨（同上） | 关联缺陷 D1 |
| G3 | Skill 工具非 inline 错误文案残留 "in v1" 措辞 | 文案问题 |
| G4 | v2 插件 manifest 新字段（`agents` / `systemPrompt` / `systemPromptPath`）未入文档站字段表 | 文档站补记 |
| G5 | 会话标识占位符未在文档站记载 | 文档站补记 |
| G6 | 限定名解析缺口：`插件id:技能名` 形态的技能引用在两代引擎的 Skill 工具与激活校验中均落「清单中不存在」（2026-10-09 登记；经 SDR-002 / ADR-001 裁决为受支持形态，修复随 `fix/skill-qualified-lookup` 分支） | 详证见 context §4.2 |
| G7 | 输入区信息架构缺口：斜杠菜单无分组平铺、技能以 `skill:` 文本前缀渲染且前缀参与过滤匹配、无技能专用触发符、无统一技能图标、空描述渲染符号残留（2026-10-12 登记；经 SDR-003/SDR-004 / ADR-002/ADR-003 裁决为受支持终态；web 优先落地，TUI 补全面随后对齐） | 详证见 context §6.3 |
| G8 | **确认缺陷**：内联引用发送前警示对整条消息文本跑引用扫描，粘贴含类引用形态文本（如 PHP 数组下标 `[$var]`）的内容被误拦，且「移除失效引用」会删除粘贴原文（2026-10-23 登记；经 SDR-007/ADR-009 裁决为校验范围缺陷，修复 = 校验对象改为输入框的结构化芯片节点，实施计划见 [plans/02-pre-submit-warning-scoping.md](plans/02-pre-submit-warning-scoping.md)） | 需求与设计见 SDR-007 / ADR-009 |
| G9 | **缺口**：rust 引擎技能目录无热更新——装配时一次性扫描，无文件监视；目录变更只对之后新建的 agent 生效（2026-10-23 登记；经 SDR-008/ADR-010 裁决为受支持终态，含 watcher 重扫 + 清单系统提醒 + 手动重载；实现前现状快照见 context §11） | 需求与设计见 SDR-008 / ADR-010 |
| G10 | **确认缺陷**：工作区根为用户主目录（或经符号链接别名）时，默认根发现的多组通道（项目 `.agents` ↔ 用户 `.agents`、项目品牌目录 ↔ 用户品牌目录）解析到同一物理目录，技能被重复扫描，且逐技能产生跨来源遮蔽告警（单次启动上百条噪音）（2026-10-23 登记；经 ADR-011 裁决为编排层缺陷，修复 = 默认根清单的归一化去重，最终归属不变） | 设计见 ADR-011 |
| D1 | **确认缺陷**：原生二进制 + 插件声明 `command:"node"` 时，引擎注入 `MIRRI_PLUGIN_ROOT`，而 TUI `__plugin_run_node` 隐藏子命令要求 `MIRRICODE_PLUGIN_ROOT`——回退必然失败（context §5.1） | 需修复（env 名对齐），与 G2 同根 |

> **范围注（2026-09-12，B0）**：上表 G1/G2/G4/G5 与缺陷 D1（即全局缺口 D-19～D-21：文档站占位符与插件 env 字段勘误）**并入本程序 B11 执行环境核验批次**——按总计划 DEC-1，TS 侧不改代码，改为核验 Rust 等价路径无同类问题 + 以 Rust 现行 `MIRRI_*` 为规范名归档进域文档。

## 3. 未来增强提案（超出 TS 现状，已记入 requirements §11）

1. `/skills` 清单命令与技能浏览器
2. 逐技能启停（列表级 + 会话级）
3. web 技能管理页（含 `extra_skill_dirs` 编辑）
4. 技能创建脚手架命令
5. 占位符与 env 命名统一
6. 插件技能的来源标识（协议面区分 plugin）
7. 技能版本与更新提示
8. 技能目录只读沙箱选项
9. `allowed-tools` 技能声明
10. 技能清单附限定别名（插件技能条目附 `插件id:技能名` 提示，2026-10-09 随 SDR-002 登记）

> **范围注（2026-09-12，B0，总计划 DEC-2）**：提案 2（用户级逐技能启停）与 3（web 技能管理页含 `extra_skill_dirs`）随本程序 **B8** 落地（全局缺口 D-32）；提案 5（占位符与 env 命名统一）随 **B11** 落地（见 §2 范围注）；提案 9（全局 P-019 allowed-tools）**已裁决推迟**；~~提案 6（插件技能的来源标识，2026-10-25 随 SDR-009 采纳为需求 US3.8）~~；其余提案（1、4、7、8）一并**推迟**——不在本程序裁决清单内（rust+web 落地范围外或收益未排期），待产品侧另行立项。

## 4. 开放项（需人工确认）

| # | 问题 | 影响 |
|---|---|---|
| O1 | 占位符命名双轨是有意改名还是遗漏（context §1、§7） | Rust 占位符命名决策 |
| O2（已升级为确认缺陷 D1，见 §2） | ~~需确认 v2 是否仍有触发场景~~ 已确认：原生二进制 + `command:"node"` 必然触发且因 env 名不匹配失败 | 修复排期（TS 侧）与 Rust 侧 env 命名裁决 |
| O3 | 插件技能协议面 source 归入 extra 是否有意 | ~~决定提案 6 是否落地~~ 已裁决（SDR-009 / US3.8，2026-10-25）：对外契约携带插件标识，候选列表徽章与过滤 |
| O4 | v1 `services/skill` 保留面的 Rust 对应物 | 防腐层是否保留 |
| O5 | **Rust 引擎无 builtin 技能注册通道**：六个内置技能（含模型可调的 write-goal / update-config）在 v3 后端完全缺失，web 端 v3 相应斜杠入口与模型自动调用随之下线（context §10.4） | **✅ 已落地（2026-10-10）**：设计 [rust-core-design/13-builtin-skills.md](rust-core-design/13-builtin-skills.md)（ADR-B1..B6）+ 分支 `feat/rust-builtin-skills` 六提交（TDD 三批 M1-M3）；V1-V8 全测、真端口 e2e 闭环、llvm-cov 区域覆盖 94.5–100%；执行状态唯一源见 [rust-core-design/ROADMAP.md](rust-core-design/ROADMAP.md) 1.10 |

## 5. 跨域术语（**已注册**，glossary v8 起陆续登记；本节保留作域内映射台账）

- **技能（skill）**：glossary T1 的同名异义警示已含指向本域 spec 与 SK1–SK3 的交叉引用——注册闭环。
- **技能来源层级（Project / User / Extra / Built-in）**：已注册为 glossary SK1（本域 spec §2 为域内视图）。
- **技能激活（skill activation）**：已注册为 glossary SK2。
- **插件（plugin）与 marketplace**：已注册为 glossary SK3（贡献面清单条目含 skills / mcp / agent-profile 三域）。
- **插件限定名（plugin-qualified skill name）**：已注册为 glossary SK5（2026-10-09 随 SDR-002/ADR-001 同批登记）。
- **技能触发符（skill trigger symbol）**：已注册为 glossary SK6（2026-10-12 随 SDR-003/ADR-002 同批登记；2026-10-13 随 SDR-005/ADR-004 修订触发位置）。
- **技能引用链接（skill reference link）**：已注册为 glossary SK7（2026-10-13 随 SDR-005/ADR-004 同批登记）。

## 6. 落地程序范围标注（baseline-gap-landing 程序 · 2026-09-12）

> 依据总计划 [`plan/baseline-gap-landing.md`](../../plan/baseline-gap-landing.md)（§一 DEC-1/DEC-2 范围裁决、§二 批次定义、§四 进度跟踪）。本节只做状态与范围管理。

**已裁决推迟（本程序排除，DEC-2）**：见 §3 提案范围注（提案 9 / P-019 明列推迟；~~提案 6 已随 SDR-009 采纳~~；提案 1、4、7、8 一并推迟，待产品侧另行立项）。

**落地中（本程序进行中的缺口项与批次）**：

| 批次 | 缺口项 | 状态 |
|---|---|---|
| B8 web 技能管理页（Rust + web） | D-32（用户级逐技能启停配置 + 技能管理 RPC + web 管理页，含 `extra_skill_dirs` 编辑；对应提案 2/3） | **引擎面 ✅（2026-09-13，B8 Rust 轨 R1-R9）**：`disabled_skills` config 键三处同改、`SkillCatalog` 用户禁用集、`skill.list` enabled 字段、`skill.getContent`/`skill.setEnabled` RPC（五环齐）、`agent.activateSkill`/SkillTool 禁用门控、`extra_skill_dirs` 装配消费 + config 写后热 resync；web 管理页待 web 轨（W1-W7）。实现事实见 [skills-context.md](skills-context.md) §9 |
| B11 执行环境核验与补齐（跨域批次） | D-19/D-20/D-21（文档站占位符与插件 env 字段勘误，`MIRRI_*` 为规范名；即本域 §2 的 G1/G2/G4/G5 与缺陷 D1 的核验处置） | 进行中 |

**留档（D-7 已裁决不做，本程序范围外）**：`merge_all_available_skills` 在 rust 引擎无消费方（发现为"后到者胜"，无 TS 的源分组合并）——web 开关既有，行为差异见 [skills-context.md](skills-context.md) §9.4；shadowing 优先级对齐 TS source priority 常量的漂移同此留档。
