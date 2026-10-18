# Rust 内核域设计 — 13: 内置技能注册通道（builtin skills）

> **文档类型：设计/规格类**（按 `docs/specs/AGENTS.md` 治理约定）——本文持需求与设计终态；现状证据见 [../skills-context.md](../skills-context.md) §10 与 [context.md](context.md)；执行状态在 [ROADMAP.md](ROADMAP.md) 1.10。
> 状态: Draft（2026-10-10 首稿） · 作者: Mirri Code 工程
> 配套：逐技能行为契约（权威规格源）见 [../skills-builtin-skills.md](../skills-builtin-skills.md)——本文不重复技能逐条行为，只设计其 Rust 承载；技能系统通用机制规格见 [../skills-requirements.md](../skills-requirements.md)。
> 决策记录：本文件附录 A 持决策记录（ADR-B1..B6，编号仅在本文件内唯一，沿用 spec 10/11 体例）。

## 1. 需求

### 1.1 动机（抽象问题陈述）

- 引擎侧没有任何内置技能注册通道：六个开箱工作流技能（目标撰写、配置修改、MCP 配置、主题定制、生态迁移、技能整理）在 v3 后端完全不存在——新装用户在 web v3 下连 `/write-goal` 这样的入口都看不到。
- 一半的缺口更隐蔽：模型可见的技能清单注入缺失（系统提示词的技能段被硬编码为空）——即便技能被注册，模型也不知道它们存在；两个**模型可调**技能（write-goal / update-config）的价值完全依赖清单注入。
- 技能定义无法表达门控元数据（禁用模型调用、子技能标记），门控与清单过滤都无从谈起。

期望终态：内置技能随二进制分发、开箱即用、可被任何用户层同名覆盖；模型按清单知晓并可自动调用可调技能；用户可在两端以斜杠显式调用；逐技能启停等既有管理面原生适用。（现状证据见 [../skills-context.md](../skills-context.md) §10.4。）

### 1.2 需求项

- **R1 开箱即得**：空用户目录装配的运行时，技能清单接口返回 8 个内置条目（6 技能，source 为 `builtin`）。
- **R2 最低优先级、可覆盖**：用户/项目/额外/插件任何层的同名技能胜出（对齐 TS `builtin(0)` 语义）；被覆盖时记录与 TS 同类的告警日志。
- **R3 模型侧硬门控**：声明「禁用模型调用」的四个技能（mcp-config / custom-theme / import-from-cc-codex / sub-skill 家族）经 Skill 工具调用时在执行环节被硬拦截，返回「只能由用户触发」方向的错误结果；未声明的两个（write-goal / update-config）模型可正常调用。
- **R4 模型可见清单**：系统提示词注入技能清单——首行声明取代更早清单；按 Project / User / Extra / Built-in 分组；仅含模型可调技能（滤除禁模型调用与子技能条目）；描述截断；空清单不注入（不留空标题）。
- **R5 多端透传**：除一个与 TS 对齐的可选字段（`disable_model_invocation` 置位时外发——TS REST 字面键，snake_case）外 wire 零协议变更——清单接口新增 `builtin` 来源条目即被既有消费端（TUI / web）按裸名斜杠命令呈现。
- **R6 管理面原生适用**：逐技能启停（disabled_skills 种子）与激活排队对内置技能名同样生效。
- **R7 正文即接口**：技能正文以编译期嵌入资产逐字对齐 v2 基线，任何刻意调整（见 ADR-B5）逐条披露。
- **R8 子技能语义**：子技能条目不进模型清单；斜杠命令面不做子技能过滤（与 TS 两端现状一致）。
- **R9 resync 幂等**：配置写后触发的目录重放不会丢失内置条目（重放后清单仍含未被覆盖的内置技能）。

### 1.3 非目标

- 不做用户技能 frontmatter 的 `type`（inline/flow/reference）解析与调用类型门——rust 无该概念，随「用户技能格式 parity」另行立项（本设计只带 `disable-model-invocation` / `is-sub-skill` 两键的解析，作为内置注册的同一条路径，见 ADR-B6）。
- 不做 `whenToUse` 元数据与参数占位符 parity（内置技能正文均不使用；归用户技能格式域）。
- 不做 `/skills` 清单命令与技能浏览器（skills 域提案 1，已裁决推迟）。
- 不引入 `SkillSource` 优先级常量与优先级合并重构（既有 D-7 裁决「不做」，本文不推翻——见 ADR-B3）。
- 不改 TS 侧任何行为；TS 侧消化随退役线（ROADMAP 1.3）走。

## 2. 现状差距摘要

> 完整证据（file:line）见 [../skills-context.md](../skills-context.md) §10；此处仅列设计输入。

| # | 缺口 | 现状 |
|---|---|---|
| G-1 | 无内置注册通道 | `SkillSource::Builtin` 仅是发现来源标签，无任何代码内技能定义 |
| G-2 | SkillDef 无元数据字段 | 5 字段结构，无法承载禁模型调用 / 子技能标记 |
| G-3 | skills_section 硬编码空串 | 系统提示词模板占位符存在，装配时替换为空 |
| G-4 | SkillTool 无禁模型调用门 | 门链只有解析门 + 用户禁用门 |
| G-5 | frontmatter 仅消费 description | 其余键解析后被丢弃 |

## 3. 设计

### 3.1 资产与注册模块

- 位置：`rust/crates/mirri-core/src/skill/builtin/`（`skill.rs` 与同名目录并存的 Rust 2018 模块布局）：六个 `.md` 正文文件 + `builtin.rs` 聚合模块。
- 嵌入方式：**每个 `.md` 经 `include_str!` 编译期嵌入**（ADR-B1）——镜像 TS `?raw` 导入模式，与 `system_prompt.md` 同款惯例；无运行时文件依赖，单二进制分发不丢资产。
- 注册形态：`builtin.rs` 导出 `builtin_skills() -> Vec<BuiltinSkillDef>` 常量级构造（8 条目），`BuiltinSkillDef = { def: SkillDef, content: &'static str }`——**正文随定义一起供给**（`include_str!` 的 `&'static str`，见下条「正文数据通路」）。每条目经与 TS 包装同构的 `make` 函数：复用既有 frontmatter 解析得 description 等基础字段，然后覆盖 `source = Builtin`、`path = 伪路径`、以及门控标志。**覆盖点与 TS 逐一对齐**：mcp-config / custom-theme / sub-skill 家族的禁模型调用在注册层覆盖（其 frontmatter 不声明）；import-from-cc-codex 由 frontmatter 自带；write-goal / update-config 两处皆无（模型可调）。
- 伪路径：`builtin://<名>`（PathBuf 承载任意字符串；wire 投影原样输出，与 TS 一致）。子技能条目名沿用 `sub-skill.review` / `sub-skill.consolidate`（注册名即含点号，与 TS 一致）。

**正文数据通路（内容供给）**：rust `SkillDef` 不含正文字段，既有内容读取链（`read_content` → `path.join(SKILL.md)` → 磁盘读）对伪路径必然失效。设计：

- `SkillCatalog` 新增**内置内容表** `builtin_contents: HashMap<String, &'static str>`（按技能名键）与逐条注册 API `register_builtin(skills: impl IntoIterator<Item = BuiltinSkillDef>)`（写入 `skills` map + 内容表；同批补齐 catalog 此前缺失的逐条注册能力）。
- `read_content` 对 `source == Builtin` 的条目**查内容表**返回嵌入正文，其余来源维持磁盘读取不变——所有下游（SkillTool 注入、用户斜杠激活渲染、`skill.getContent` RPC）经既有 `read_content` 单一通道自动获得，无其他改动点。
- **遮蔽一致性**：用户层同名覆盖后，catalog 中该名条目的 `source` 已非 Builtin，`read_content` 自动落回磁盘路径——内容表中的内置正文残留无害（永不被命中）。
- web 管理面（`skill.getContent`）对内置条目返回嵌入正文（可查看、不可编辑——编辑路径本就按 `path` 落盘，伪路径不可写，行为自然封闭）。

### 3.2 SkillDef 扩展（类型化元数据）

新增两个字段（serde `#[serde(default, skip_serializing_if = "is_false")]`——对齐 `plugin` 字段先例，两者都标缺省才对旧载荷无损；`is_false` 谓词随本批定义；SkillDef 的 serde 目前仅测试往返消费，wire 面为手工投影，兼容声明适用于此范围）：

- `disable_model_invocation: bool`
- `is_sub_skill: bool`

**wire 投影分两半**：`is_sub_skill` 不外发（TS 刻意不外发，两端一致）；`disable_model_invocation` 在置位时**外发**——wire 键取 **TS REST 字面值 `disable_model_invocation`**（snake_case；`skill.list` 手工投影增一个可选布尔字段，与 TS 投影逐字对齐）。当前无消费端读取该字段，外发属 parity 对齐而非功能依赖。

frontmatter 解析（`parse_frontmatter`）新增两键识别，键名做连字符/下划线归一（对齐 TS 兼容写法）：`disable-model-invocation` / `disable_model_invocation` → `disable_model_invocation`；`is-sub-skill` / `is_sub_skill` → `is_sub_skill`（ADR-B6：顺路让用户技能获得这两个键的解析层；`type` 族明确不在本设计范围）。

### 3.3 发现序列插入点

`apply_skill_discovery`（`rust/crates/mirri-embedded/src/bootstrap.rs`）在 `skills.clear()` 之后、plugin roots 之前插入一步「装配内置技能」：将 `builtin_skills()` 逐条写入 catalog。

- **优先级语义**：rust 发现为「后到者胜」，首插即天然最低优先级——后续 plugin / extra / user / project 任何同名覆盖均成立（R2），覆盖时走既有 `previous.source != source` 告警。
- **不受 explicit 分支影响**：插入点在「explicit skillDirs 替换 vs 默认发现」二选一分支**之前**——对齐 TS「内置是 App 级常量、显式目录只替换用户/项目默认源」的语义。
- **resync 幂等**：所有重扫路径（装配、config 写后 resync）都重放 `apply_skill_discovery` 整序列（clear 起步），内置装配天然幂等（R9）。
- 同批修订 `apply_skill_discovery` 头注释与 `bootstrap.rs` 内「rust 无 builtin 技能发现通道」的留档注释（`plan/rust-plugin-parity.md` G 行对应叙述随实现收口时更新）。

### 3.4 门控链（SkillTool）

`rust/crates/mirri-core/src/tools/skill.rs` 执行闭包的门链在**用户禁用门之后**新增禁模型调用门：

```
解析门（bare → 插件限定名）
  → 用户禁用门（disabled_skills）
  → 禁模型调用门（disable_model_invocation → 错误结果，文案对齐 TS「只能由用户触发」方向）
  → 加载注入
```

- 拦截点在工具执行环节（硬强制，先于内容注入），对齐 skills-requirements US4.3。
- 用户斜杠激活路径（`dispatch_activate_skill`）**不加**该门——该标志只约束模型侧，用户显式调用不受影响（US1.2）。
- 子技能条目：builtin 家族全部声明禁模型调用，由本门拦截；`is_sub_skill` 字段只用于清单过滤（§3.5），不设独立工具门。

### 3.5 模型可见清单注入（skills_section）

`rust/crates/mirri-core/src/scope/system_prompt.rs` 的 `apply_workspace_part` 将 `skills_section` 从硬编码空串改为渲染函数输出。渲染规则对齐 TS registry：

- 首行：`DISREGARD any earlier skill listings. Current available skills:`（取代声明）。
- 分组与顺序：Project / User / Extra / Built-in（与 wire 源映射一致），组内按名称排序。
- 过滤：仅含模型可调条目（滤除 `disable_model_invocation == true` 与 `is_sub_skill == true`）。
- 条目：名称 + 描述（截断）+ 路径行（内置条目为伪路径原样）。
- 截断：**字符边界 250 字符，截断时在 247 处截去并以 `...` 结尾**（对齐 TS 截断口径；ADR-B5 类比的刻意简化——TS 为图位素截断，rust 不引入 grapheme 依赖，CJK 主体场景下两者等价，差异披露）。
- 空清单（过滤后为空）不注入（替换为空串，保持现状行为）。
- 注入门控：当前 rust 系统提示词为工作区级单一模板、Skill 工具为无条件 builtin 注册，故清单**无条件渲染**（与 plugin_sections 同层同策略，ADR-B4）；将来 profile 工具面分化（子代理无 Skill 工具）时，清单注入跟随工具面门控——届时沿 TS `skillActiveFor(tools)` 语义补齐，本文留接口说明不预设开关。
- **管线接线**：对齐 `render_plugin_sections` 的既有先例——渲染函数与 plugin sections 同层实现，装配点传入 catalog 引用（新会话的提示词装配时求值；既有会话保留装配时快照，与 plugin_sections 的刷新策略一致——resync 变更对新会话生效）。

### 3.6 wire 面

- **唯一 wire 增量**：`skill.list` 手工投影在置位时外发可选布尔 `disable_model_invocation`（TS REST 字面键，见 §3.2）；除此之外**零协议变更**——清单接口新增的只是 8 条 `source: "builtin"` 条目，而 `"builtin"` 已是 TUI / web 消费端比较的预期值（TUI 技能命令构建按 `source === 'builtin'` 决定裸名无前缀 + 优先排序；web 同构）。
- 逐技能启停（`skill.setEnabled` / `disabled_skills` 种子）与激活排队（`agent.activateSkill`）对内置名原生生效（R6）——禁用门在解析之后，与既有 B8 语义一致。
- `skill.getContent` 对内置条目返回嵌入正文（§3.1 正文数据通路）。
- 五环契约链（registry / TSV / fixture / cross-backend / node-sdk）不触碰：无新 RPC、无必填 shape 变更（可选字段缺省兼容）。

### 3.7 内容对齐口径

基线 = v2 正文**逐字复制**，刻意调整仅以下两处（ADR-B5，均须在落地 PR 中逐条披露）：

1. **品牌统一为 Mirri Code**：import-from-cc-codex 全文的 "Kimi Code" → "Mirri Code"；mcp-config 的 "Kimi-specific" → "Mirri-specific"（v2 文案混入上游品牌词，与产品名不一致）。
2. **去内部代码路径引用**：mcp-config 中指向 `packages/agent-core-v2/src/mcpCore/config-schema.ts` 的「字段权威」表述改为指向官方文档——内部仓库路径不应进模型上下文（刻意改进）。

其余逐字（write-goal / update-config 与 v1/v2 均逐字一致；custom-theme 保留 v2 的文档 URL）。**依赖确认**：write-goal 引用的 `AskUserQuestion` / `CreateGoal`、update-config 依赖的 `FetchURL` 在 rust 工具面均已存在；`/reload`、`/theme`、`mirri doctor` 为客户端面（TS TUI/CLI 继续存在），正文引用保留。

## 4. 备选方案与取舍

| # | 决策 | 备选 | 拒绝理由 |
|---|---|---|---|
| T1 | `include_str!` 逐文件编译期嵌入 | `rust-embed` 目录嵌入（仅 web SPA 一处先例，目录级场景）；随二进制分发运行时文件 | 目录嵌入对 6 个单文件过重；运行时文件破坏单二进制分发与「正文即接口」的编译期锁定（资产漂移无编译屏障） |
| T2 | SkillDef 类型化两字段 | 通用 `metadata: HashMap` / `serde_json::Value` | 放松契约（任意键静默通过）、wire 投影失控；rust SkillDef 是强类型风格，两个布尔字段足够承载本需求 |
| T3 | 发现序列首插，不引优先级常量 | `SkillSource` 加权重 + 优先级合并（对齐 TS source priority 常量） | D-7 既有裁决「不做」；「后到者胜 + 首插」语义等价且改动面最小（零既有发现路径变更，只加一步） |
| T4 | 清单无条件渲染（与 plugin_sections 同层） | 按 agent 工具面门控（TS `skillActiveFor` 语义） | rust 当前无 per-agent 工具面分化的提示词装配，预设门控是无消费者的开关；profile 分化落地时再补（§3.5 留说明） |
| T5 | 字符边界 250 截断 | 引 grapheme 感知截断（TS Intl.Segmenter 等价） | 为一个截断位引入依赖不值；差异已披露，emoji 组合字符场景的偏差不影响模型理解 |

## 5. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| RK1 | **双源内容漂移**：v2 TS 与 rust 各持一份正文，演进不同步 | 正文是规格锁定的契约（skills-builtin-skills NF2「逐字对齐或披露改进」）；变更须走 spec 同步；退役线 1.3 删 TS 侧后单源 |
| RK2 | 「首插 = 最低优先级」是隐性顺序不变式，未来维护者可能在 builtin 之前插入别的步骤 | bootstrap 测试钉住「builtin 先于 plugin 被装配」+ 序列首部的优先级注释链（0/5/10/20/30）同步补 builtin(0) 行 |
| RK3 | 清单注入增加系统提示词体积 | 两级结构保证只进清单不进全文（每条 = 名 + 截断描述 + 路径 ≈ 数十 token；6 内置 + 常规用户库 < 1KB）；空库用户清单极小 |
| RK4 | `is_sub_skill` 不外发，TUI/web 斜杠菜单出现 `/sub-skill.review` 等条目 | 与 TS 两端现状一致（需求评审已确认两端均不过滤），非新缺口；若要过滤属消费端后续统一决策 |
| RK5 | frontmatter 两键解析的顺路范围扩张（用户技能也获得解析） | 解析 ≠ 门控：`disable_model_invocation` 的 SkillTool 门对用户技能同样生效（这正是 TS US1.2 语义，属补齐而非行为变更）；`is_sub_skill: true` 的用户技能同样从模型清单消失（与 TS 清单过滤行为一致，对称补齐）；`type` 族明确非目标，边界在 §1.3 钉死 |
| RK6 | 内置技能内容引用的客户端命令（/reload、mirri doctor）在非 TUI 宿主（ACP 等）不存在 | 正文语义是「告知用户如何生效」而非硬依赖；与 TS 行为一致，不新增风险；宿主差异由客户端各自文档化 |

## 6. 成功标准（验收）

- **V1**（R1）：空用户目录装配 runtime → `skill.list` 返回 8 条 `source: "builtin"` 条目（名称、伪路径、enabled=true；六个门控条目携带 `disable_model_invocation: true`，两个可调条目无该键）。
- **V2**（R2）：放置同名用户技能后重放发现 → 清单中该名只剩用户版；日志含覆盖告警；删除用户版并 resync → 内置版回归（清单一类变更**对新会话生效**，见 §3.5）。
- **V3**（R3）：SkillTool 调用 `mcp-config` → 错误结果（用户专用语义文案），无注入；调用 `write-goal` → 全文注入当前回合（**嵌入正文经 `read_content` 内容表供给**）。
- **V4**（R4）：新会话的系统提示词含 `Built-in` 分组清单且首行为取代声明；四个禁模型调用条目与子技能条目不出现在清单中；清空所有技能来源后清单段不注入。
- **V5**（R5）：web v3 斜杠菜单出现 `/write-goal` 等裸名条目（消费端零改动验证）。
- **V6**（R6）：`skill.setEnabled` 禁用 `write-goal` 后 `agent.activateSkill` 与 SkillTool 双双被拒；恢复后可用。
- **V7**（R7）：六个正文与 v2 基线逐字 diff，仅 §3.7 披露的两处调整。
- **V8**（R9）：修改 `extra_skill_dirs` 触发 resync → 内置条目仍在且未被重复插入。

## 7. 交付批次

> 本文只持批次划分这一设计决策；任务表随首批落地迁至 `rust-core-design/plans/`（对齐 spec 12 体例），执行过程按 rust 域既有程序管理。

| 批 | 范围 | 主要落点 | 测试布局 |
|---|---|---|---|
| M1 数据面 | SkillDef 两字段 + frontmatter 两键解析 + builtin 模块（资产 + 注册 + **内容表与 `read_content` 供给**）+ `apply_skill_discovery` 首插 + 注释链修订 | `mirri-core/src/skill.rs`、`mirri-core/src/skill/builtin/`、`mirri-embedded/src/bootstrap.rs` | `skill.rs` tests（字段/解析/serde 往返/内容表读取与遮蔽回退）；`bootstrap.rs` tests（首插、被覆盖、explicit 分支下仍在、resync 幂等） |
| M2 模型面 | SkillTool 禁模型调用门 + `skills_section` 渲染 | `mirri-core/src/tools/skill.rs`、`mirri-core/src/scope/system_prompt.rs` | `tools/skill.rs` tests（门链顺序/错误文案）；system prompt 渲染测试（分组/过滤/截断含省略号/空清单/取代声明） |
| M3 契约与消费端 | embedded 全链验证（list 含可选字段/activate/setEnabled/getContent 含 builtin）+ web 手动验证 + 内容披露清单落地 PR + 文档评估（gen-docs） | `mirri-embedded/src/embedded_server/tests.rs` | 复用既有 skill 域测试样板（`should_list_workspace_brand_skills…` 同族） |

## 附录 A · 决策记录（ADR）

### ADR-B1 · 资产以 `include_str!` 编译期嵌入，经 catalog 内容表供给

- **背景**：六个正文文件需随二进制分发且与代码同版本演进（「正文即接口」）；rust `SkillDef` 无正文字段、内容读取链走磁盘——嵌入与供给是同一个问题的两半。
- **决策**：每文件 `include_str!`，聚合在 `mirri-core/src/skill/builtin/` 模块；`SkillCatalog` 增内置内容表与逐条注册 API，`read_content` 对内置来源查表返回（§3.1 正文数据通路）。
- **被否决备选**：`rust-embed`（目录级嵌入，本场景单文件过重、仅 web SPA 有先例）；运行时分发文件（丢失编译期锁定与单二进制性）；`builtin://` 虚拟文件系统接入 `SyncFs`（为一个静态表引入 FS 抽象层，过度设计）。
- **影响**：内容变更必然触发重编译与评审（正向约束）；无循环依赖（builtin 构造在 mirri-core，bootstrap 只消费 `builtin_skills()`）；被同名用户技能遮蔽后内容表残留无害（`read_content` 按条目来源分派，永不命中）。

### ADR-B2 · SkillDef 类型化元数据字段（两布尔），wire 投影分两半

- **背景**：需承载禁模型调用与子技能标记；wire 投影须与 TS 对齐。
- **决策**：`disable_model_invocation` / `is_sub_skill` 两个 `bool`（`#[serde(default, skip_serializing_if = "is_false")]`，对齐 `plugin` 字段先例）。wire 投影：`disable_model_invocation` 置位时经 `skill.list` 外发，**wire 键为 TS REST 字面值（snake_case `disable_model_invocation`）**（可选字段）；`is_sub_skill` 不外发（TS 刻意丢弃，两端一致）。
- **被否决备选**：通用 metadata map（放松契约、投影失控）；两字段全部不外发（与 TS 投影行为不一致——TS 在置位时外发 `disable_model_invocation`，不外发属未披露分歧）。
- **影响**：`type` / `whenToUse` / 参数占位符等未来元数据到来时再各自评审，不预铺通用容器。

### ADR-B3 · 序列首插，不引入优先级常量

- **背景**：内置须为最低优先级；rust 发现是「后到者胜」。
- **决策**：`apply_skill_discovery` 在 clear 后、plugin 前插入内置装配；不改合并机制。
- **被否决备选**：`SkillSource` 权重 + 优先级合并重构（D-7 既有裁决「不做」；改动面大且与 TS 的源分组合并语义纠缠）。
- **影响**：优先级正确性依赖插入顺序（RK2 以测试 + 注释钉住）。

### ADR-B4 · skills_section 无条件渲染

- **背景**：清单注入需要门控条件；rust 当前提示词为工作区级单模板、Skill 工具无条件注册。
- **决策**：与 plugin_sections 同层无条件渲染；profile 工具面分化后跟随补门控。
- **被否决备选**：预设 per-agent 开关（无消费者，先建开关属投机设计）。
- **影响**：子代理若未来获得独立工具面（无 Skill），须同步补清单门控，避免「看得见调不了」。

### ADR-B5 · 内容两处刻意调整（品牌统一 + 去内部路径）

- **背景**：v2 正文含上游品牌词（Kimi Code）与内部仓库路径引用。
- **决策**：品牌统一为 Mirri Code；字段权威表述改指官方文档。调整在落地 PR 逐条披露，其余逐字对齐。
- **被否决备选**：完全逐字（品牌词与产品名不一致，模型输出会混用品牌）；自由改写（违反「正文即接口」）。
- **影响**：与 TS v2 存在披露差异；退役后 rust 为唯一源。

### ADR-B6 · frontmatter 两键解析纳入本设计

- **背景**：builtin 注册与用户技能共用 frontmatter 解析；TS 用户技能本就支持这两个键（US1.2），rust 缺失。
- **决策**：解析层新增 `disable-model-invocation` / `is-sub-skill`（连字符/下划线归一）；SkillTool 门对解析所得字段生效（对用户技能是 TS 语义的补齐）。
- **被否决备选**：仅在 builtin 注册常量硬编码标志、不动解析层（双路径、用户技能继续缺门控，属既有缺口延续）。
- **影响**：范围受 §1.3 钉死（`type` 族 / 占位符族不随本设计扩张）。
