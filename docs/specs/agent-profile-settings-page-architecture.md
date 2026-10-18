# Agent Profile 配置域架构（RFC）

> 状态：设计稿 · 2026-09-09 ·
> 本文描述 agent-profile 配置域（配置面 Epic A + 使用域消费语义 + 双引擎一致）的终态设计：解析模型、catalog 投影、wire 契约、写通道与动作面、终端边界；引擎终态落 rust，TS 侧保留过渡最小集（裁决见 ADR-001~009）。
> 需求事实源：[agent-profile-settings-page-requirements.md](agent-profile-settings-page-requirements.md)（配置面 US-A1–A5、US-C1、SDR-002/004/007/009/010/011）· [agent-profile-consumption-requirements.md](agent-profile-consumption-requirements.md)（使用域 US-U1.1/U2.1/U2.5 等）· [collaboration-requirements.md](collaboration-requirements.md)（协作域关联条款）· 术语：[glossary.md](glossary.md) · 现状参考：[agent-profile-settings-page-context.md](agent-profile-settings-page-context.md)（设计→现状单向引用）· 执行状态：[agent-profile-ROADMAP.md](agent-profile-ROADMAP.md)

## 1. Scope

**IN**：
- 角色文件解析模型：`.md` frontmatter 为事实源、`.toml` legacy 只读兼容，`enabled` 为解析模型一等字段
- 多来源 catalog 单点合并投影（优先级胜出 + builtin override + 禁用跳过 + essential 锁定）与压制候选语义
- 读侧 wire 契约：source 枚举、suppressed_candidates、skipped（path + reason）、disallowed_tools、capabilities、subagents
- rust 写通道（`.md` read-modify-write round-trip 保真）与动作面 RPC（enable/disable/reset）
- 配置页消费（列表/详情/创建/编辑/启停/删除）与远程编辑同步（实时刷新 + 冲突人定）
- 双引擎一致验收（US-C1）

**OUT**：
- 终端（`/agents`）同源化实现（ADR-005 延后，仅范围占位）
- 覆盖内置角色引导（A6）、配置页与终端超出「列表 + 启停」的完整对等（A7 余项）
- 项目级角色创建入口（配置面 SDR-011：随协作域团队功能交付）
- 用途标记编辑面（随 U1.1 交付，配置面 SDR-004）

## 2. Goals & Non-Goals

**Goals**：
- 同一份 `.md` 角色集合在两个引擎下呈现与消费一致（US-C1）
- 启停事实内化到角色文件（`enabled`）：引擎派发、显式指定校验与页面呈现同源一致（A5-1、U2.1-2、U2.5-2）
- 编辑写通道对文件内容逐字保真：未知 frontmatter 键与正文不被破坏（A4-1）
- 页面在无未保存编辑时自动反映外部修改；存在未保存编辑且文件被外部修改时保存拦截并差异三选一（配置面 SDR-009）

**Non-Goals**：
- TS 侧写通道重建（TS 过渡期只止血）
- 终端同源化排期（R5 占位）

## 3. 组件设计（终态）

### 3.1 解析模型：`.md` frontmatter 事实源

角色文件 = frontmatter（可编辑字段全集：name/description/whenToUse/defaultModel/tools/disallowedTools/subagents/capabilitiesRequired/enabled + 正文即系统提示词）。`enabled` 是解析模型的一等字段（ADR-001）；`.toml` 降级为 legacy 只读兼容。frontmatter 解析为纯函数、零 IO，两侧解析产物逐字段一致（验收测试向量从 TS 侧移植）。

### 3.2 catalog 单点投影

多来源（内置/用户/项目/额外目录）同名合并只在引擎内 catalog 一处实现：优先级胜出 + 禁用跳过 + essential 保护（不可删/不可禁，内容可被同名文件覆盖——ADR-006）；压制候选随读侧 wire 输出（suppressed_candidates）。TS 侧承载点为 `SessionAgentProfileCatalogService.reproject`，rust 侧对应 `ProfileManager`，两侧语义对齐（ADR-002，以 R4 跨引擎同一份角色集合的投影对比验收）。

### 3.3 读侧 wire 契约

profile 条目契约扩展固定为：source 六值枚举、suppressed_candidates、skipped（path + reason）、disallowed_tools、capabilities、subagents。契约先行（ADR-004）：rust `profile_wire_row`（R3）与 TS 路由（R1）对着同一契约实现。

### 3.4 写通道与动作面

- **写通道**归 rust 终态：`.md` read-modify-write，未知 frontmatter 键与正文逐字保留（A4-1 完整验收唯一挂 R4）；TS 侧仅止血（修复 PUT 抹 `enabled` 的缺陷），不重建 FileStore（ADR-003）。
- **动作面**（已裁决 2026-09-09）：启停/重置走新增 RPC 方法 `agent.enableProfile` / `agent.disableProfile` / `agent.resetProfile`，`WsRpcApi` 直连调用（与 CRUD 同一传输通道，不引入 REST 桥混用）；全链五环同步，disposition `RustFirst`——TS 侧不实现，TS 过渡期继续走既有 REST action 端点，退役时随 v2 一并消失；essential 角色（`agent`）disable/reset 拒绝（对齐 TS 41307 语义），未知角色拒绝并指名（ADR-003）。

### 3.5 配置页消费

web 消费 catalog 投影契约：来源徽章、压制候选展开、解析失败可读警告区（A1-1/2/3）、详情全字段含可派发子角色（A2-1）、工具面单一概念编辑（具体工具 / 按 MCP 服务器整组 / 能力包视图，A4-2）、可派发子角色以启用角色勾选（A4-3）、创建流（A3-1~A3-4）。远程编辑同步（配置面 SDR-009）：无未保存编辑时自动刷新；存在未保存编辑且文件被外部修改时保存拦截并并排展示差异由用户定夺；推送订阅面不可靠时以轮询兜底（§6 风险 R1）。

## 4. 决策记录（ADR）

### ADR-001 `enabled` 进解析模型，启停事实内化到角色文件（2026-09-09）

1. **背景与动机**：角色的启停事实与其定义文件分离（配置层开关 vs 文件本身），同一角色在不同消费面——派发候选、页面呈现、显式指定——状态不一致；配置层开关成为一份无法随文件流转的附加事实，跨引擎切换时尤其分裂。
2. **决策**：`enabled` 为角色文件解析模型的一等字段，进 `.md` frontmatter，双侧落地（TS agent-profile 解析 + rust R2）；rust R3 起以其为启停唯一事实源，`ServerConfig.disabled_agents` 双轨退役（config 键保留读取但不再作为启停事实源）；显式指定禁用角色时报错区分「已禁用」（含恢复指引）与「不存在」（含可用集合）。
3. **被否决的备选**：① 配置层开关继续作为启停事实源——双轨分裂依旧，文件流转携带不了启停状态；② 仅页面层过滤——引擎派发不消费，禁用角色仍可被显式指定与派发（A5-1/U2.5 语义落空）。
4. **影响范围**：US-A5-1、US-U2.1-2 / U2.5-2、US-C1；R1（TS schema 重做）与 R2/R3（rust frontmatter 解析与消费）。

### ADR-002 catalog 单点消费（2026-09-09）

1. **背景与动机**：角色来源多层、同名多份需要优先级合并与内置覆盖；若各消费点各自实现合并，优先级与禁用语义随消费面漂移，页面呈现与引擎派发可能给出不同集合。
2. **决策**：多来源合并为引擎内单一 catalog 投影（优先级胜出 + builtin override + 禁用跳过 + essential 锁定），只在 catalog 一处实现；压制候选随读侧契约输出。TS 侧承载点 `SessionAgentProfileCatalogService.reproject`，rust 侧对应 `ProfileManager`（R3 对齐）；两侧语义以 R4 跨引擎同一份 `.md` 集合的关键投影（名称集合、来源、启用状态、压制候选）对比验收。
3. **被否决的备选**：① 页面侧做集合逻辑——依赖引擎数据面的消费点（Agent/Swarm 工具候选、显式指定校验）覆盖不到；② 各消费点各自实现合并——语义漂移。
4. **影响范围**：US-A1-1/A1-2、US-A5-1、US-U2.1-2；R1 Task 2 与 R3。

### ADR-003 写通道归 rust 终态；动作面走新增 RPC（2026-09-09）

1. **背景与动机**：round-trip 保真（A4-1：未知键与正文不被破坏）要求写通道对文件内容逐字保留；若在 TS 侧重建完整写通道，属过渡期双倍实现，随 TS 退役即报废。
2. **决策**：① 写通道终态落 rust：`.md` read-modify-write（未知键 + 正文逐字保留），A4-1 完整验收唯一挂 R4；TS 过渡期只止血（修复 PUT 抹 `enabled` 的缺陷），不重建 FileStore。② 动作面承载（已裁决 2026-09-09）：启停/重置走新增 RPC 方法 `agent.enableProfile` / `agent.disableProfile` / `agent.resetProfile`，`WsRpcApi` 直连调用；按全链五环同步（proto 注册表 → authoritative-methods.tsv（disposition `RustFirst`，TS 侧不实现，过渡期继续走既有 REST action 端点，退役时随 v2 一并消失）→ EMB 路由 → NAPI → contract-e2e fixture）+ parity 测试锁定；错误语义：essential 角色（`agent`）disable/reset 拒绝（对齐 TS 41307），未知角色拒绝（对齐 40418）。
3. **被否决的备选**：① TS 重建 FileStore 写通道——双倍实现成本，TS 退役即报废；② rust `http.rs` 加 REST action 路由 + `WsRpcApi` REST 桥——免注册表联动但通道混用、复用性只覆盖 web。
4. **影响范围**：US-A4-1、US-A5-1（写面）、US-C1；R1 范围界定与 R3/R4。

### ADR-004 wire 契约先行 + 读侧换源（2026-09-09）

1. **背景与动机**：两个引擎各自实现同一读面时，若无先行契约，字段形状与语义随实现分叉；读侧数据源换到 catalog 投影需要同一份契约同时承载两侧。
2. **决策**：① protocol schema 扩展（source 六值枚举、suppressed_candidates、skipped、disallowed_tools、capabilities、subagents）先于两侧实现落地，作为两引擎实现的唯一契约；② 读侧换源：R1 kap-server 读面迁移到 catalog 投影，rust `profile_wire_row`（R3）与 TS 路由（R1）对着同一契约实现；③ 两侧对齐的验证方式：同一份 `.md` 集合在两引擎 list 输出对比（R4 验收，US-C1）。
3. **被否决的备选**：先实现后抽象——两侧各自定型，契约漂移后再对齐成本更高。
4. **影响范围**：US-A1-1/A1-2/A1-3、US-A2-1、US-C1；R1 Task 3/4/5 与 R3。

### ADR-005 终端同源化延后（2026-09-09）

1. **背景与动机**：终端（`/agents`）对接哪侧引擎取决于引擎切换时点的架构；在切换点未定时实现会重复投入或对接错误侧。
2. **决策**：终端同源化（列表经 catalog、启停走文件通道、退役 config 双轨——配置面 SDR-010 语义）作为 R5 占位批次，不在 R1–R4 内排期；本架构只保留范围与验收语义（候选列表来自引擎 catalog 数据面、启停与文件通道同一、essential 判断由数据面提供），对接侧（TS SDK harness 还是 rust mirri-cli）随引擎切换点裁决。
3. **被否决的备选**：在 TS 过渡期实现终端同源化——投入随 v2 退役，且与写通道终态（ADR-003）错位。
4. **影响范围**：US-A7、配置面 SDR-010、使用域 U1.x 的终端侧条目；R5。

**追记（2026-09-10，R5 批内裁决落地）：** 对接侧（D6 终局，用户裁决 D17-a）= **SDK harness 引擎路由**——`MirriHarness` 新增 agent profile 数据面方法，三个客户端各自实现：rust（`SDKRpcClientRust`）直连既有 `agent.listProfiles` / `agent.enableProfile` / `agent.disableProfile` RPC（终态零重复投资）；v2（`SDKRpcClientV2`）消费 agent-core-v2 新下沉的 App-scope 服务（kap-server 读/写逻辑同源共享，路由改消费同一服务）；v1 抛 NOT_IMPLEMENTED，终端降级提示（D18-a，用户裁决：显示「请用设置页或手改配置文件」）。终端信息密度（D19）= 最小对齐配置面 SDR-010（名称 + 状态 + essential 标记 + 描述，仅换数据源）。调研事实：改造前终端列表硬编码 4 内置名、启停写 `disabledAgents`（默认 v2/rust 引擎下均为惰性 placebo，仅 legacy v1 生效）——R5 即修正该失同源。被否决备选：纯 rust 侧对接（默认引擎无数据可用、双进程复杂）；推迟到引擎切换批（placebo 缺陷无限期存续，违反 N4 双端一致）。

### ADR-006 覆盖语义统一：同名即覆盖、无声明；essential 保护收缩（2026-09-10，用户裁决）

1. **背景与动机**：两引擎对「用户文件与内置角色同名」的语义分歧——TS catalog 要求文件内显式 `override: true` 才覆盖（否则压制为 `builtin-override-required`），rust 同名即覆盖；且 essential（`agent`）在 rust 侧被整体压制（Task 16 行为），用户无法自定义主 agent 内容。用户裁决（2026-09-10，使用域 SDR-011 / 配置面 SDR-012）：存在同名文件即覆盖意图，无需声明。
2. **决策**：① TS 侧放弃 `override: true` 要求（catalog 与写侧一并移除该键的强制语义），对齐 rust「同名即覆盖」——跨引擎同名文件行为一致（US-C1 直接受益）；② essential 保护收缩为「不可删、不可禁」：同名用户文件正常加载并覆盖内容（提示词、模型、工具面），`enabled: false` 声明对该角色不生效（既有单点豁免），删除与禁用操作仍被引擎拒绝；rust 发现层移除对 essential 同名文件的无条件压制（Task 16 行为修订）；③ 页面在编辑 essential 角色时附带兜底身份警告提示；④ 写通道（R4 Task 23）不发射任何覆盖声明键。
3. **被否决的备选**：a) rust 对齐 TS 显式声明——被用户明确否决（有文件即意图）；b) 仅写侧自动补声明、读侧维持要求——手写文件两引擎仍分歧，US-C1 永久例外清单；c) essential 维持整体压制——主 agent 自定义需求落空，与「文件即意图」冲突。
4. **影响范围**：ADR-002（catalog 合并语义中移除 builtin override 分支）、ADR-003（写通道不发射声明键）；使用域 SDR-011、配置面 SDR-012；US-A5/US-A6/US-C1；R4 批次（TS catalog 变更 + rust 发现层修订 + web 编辑面）。

**追记（2026-09-10，R4-5 落地）**：① 覆盖 builtin 的压制候选：此前**两侧**均不为被覆盖的内置角色发射任何 suppressed 候选，R4-5 起双侧同步补发「内置落败者」候选（`{source: builtin, priority: 0, reason: priority}`，排在文件落败者之后、与 TS catalog 顺序一致）——essential 与非 essential 内置同构，两侧同形同序。② `register` 直连通道放开 essential 名（生产调用面——upsert/发现/watcher——不受影响；嵌入式 create 路由仍以保留名拒绝）。③ TS 写面（PUT/enable/disable）过渡期保留对 essential 的拒绝（TS 侧为过渡实现，不投入对齐 D16 的写面改造；rust 为终态语义，两侧读面已一致——覆盖行 `builtin:false + has_override:true + essential:true`，双侧投影矩阵逐场景等价）。

### ADR-007 `effective_system_prompt` 全行附带（2026-09-10，用户裁决）

1. **背景与动机**：rust 引擎对全部带提示词的角色行发射渲染后的完整生效提示词，TS 仅内置行，protocol 契约注释亦写「仅内置」——rust 现状违反已锁定契约注释。用户裁决（2026-09-10，配置面 SDR-013）：两个引擎统一为全行附带——自定义角色同样需要基座引用合并后的效果确认面。
2. **决策**：① TS 侧（kap-server 读面）对全部带 `system_prompt` 的行发射 `effective_system_prompt`；rust 现状（全行发射）保持；② protocol 契约注释修订为「全部带提示词的角色」；③ 页面详情为全部角色展示该只读预览（与编辑内容并排）；④ 跨引擎语料锁同步扩展。
3. **被否决的备选**：a) rust 对齐 TS 仅内置——用户角色失去效果确认面；b) 维持现状分歧——契约注释与实现长期矛盾，US-C1 永久分歧项。
4. **影响范围**：配置面 US-A2、SDR-013；US-C1；R4 批次（TS 实现变更 + 契约注释 + web 展示位扩展）。

### ADR-008 skipped 通道仅承载加载失败（2026-09-10，用户裁决）

1. **背景与动机**：R3 实现中 essential 同名用户文件同时进入 `skipped`（token `builtin-override-required`）与 essential 行 `suppressed_candidates` 双通道，而 TS 的 `skipped` 只含解析失败——警告区会展示一个合法可读的文件，误导用户。
2. **决策**：`skipped` 语义收窄为**仅加载失败**（解析失败、读取失败、不可读目录）；被压制的合法文件只出现在对应行的 `suppressed_candidates`。随 ADR-006 落地，essential 同名文件不再被压制而是正常加载，双通道场景自然消失；本 ADR 固化通道语义不变式，防御臂的 skip reason 归一化（token）保留。
3. **被否决的备选**：维持双通道并文档化为例外——警告区永久展示合法文件，用户困惑，US-C1 永久例外清单。
4. **影响范围**：US-A1-3、US-C1；R4 批次（发现层 + wire 消费）。

**追记（2026-09-10，R4-5 落地）**：同目录内同名落败文件的 skip 行为原为 R2 有意决策（记入 skip 通道），与本 ADR「skipped 仅加载失败」不变式冲突——R4-5 起收窄为静默去重（合法落败文件不产生 skip 记录，对齐 TS loader 的 entry 内 dedup），落败事实仍由胜者行的 `suppressed_candidates` 表达。

### ADR-009 `disabled_agents` 配置键写入面退役（2026-09-10，用户裁决）

1. **背景与动机**：R3 已将该键退役为「读取兼容、不生效、启动弃用警告」（ADR-001），但配置写入面（REST/RPC `config.set`）仍接受并持久化该键——「可写但无效」的幽灵配置项误导用户与排查（kap-server 复用 protocol 超集后该缺口重新打开，见 ROADMAP R4 开放问题原登记）。
2. **决策**：配置写入面显式忽略该键（TS v2 服务端丢弃而非持久化；web 配置写入剥除该键映射）；读取兼容与启动弃用警告保留（ADR-001 语义不变）；弃用警告进入 `config.diagnostics` 诊断面（原仅日志）。
3. **被否决的备选**：a) 显式支持（文件内 enabled 的别名写通道）——双事实源复活，与 ADR-001 冲突；b) 维持现状——幽灵配置项债无限期保留。
4. **影响范围**：ADR-001（追记）；kap-server `rest-config`/config 路由、web `setConfig` 映射、rust config 诊断面；R4 批次。

## 5. 开放裁决位（R2 批内调研定案，定案后回写本文档）

以下三项为 R2（rust `.md` 格式对齐）落地前必须由用户定案的设计点；裁决后作为 ADR/追记记录于本文档（不散落在计划与口头）：

1. **YAML 依赖选型**：frontmatter 解析依赖——`serde_yaml` 已停维护；候选 `serde_yml` / `saphyr` / 手写极简 frontmatter 解析（frontmatter 子集为纯标量 + 字符串列表，手写可行性真实存在）。
2. **`extends` 继承去留**：用户面已裁「继承退出用户面，以从现有角色开始取代」（配置面 SDR-005 / 使用域 SDR-007）；rust `AgentFileDef` 模型对 `extends` 字段的继承模型去留（TS 无此概念，倾向废弃或降级）待定。
3. **`.md`/`.toml` 优先级**：同名双格式并存时 `.md` 胜出为既定语义（配置面 SDR-002）；仅存 `.toml` legacy 文件仍可读（兼容存量）的边界细节待定。

## 6. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| R1 | fsWatch 订阅作用域：远程编辑同步依赖推送订阅面，全局订阅改动面大、易引入回归 | R1 以页面轮询兜底（验收语义不变：无未保存编辑自动刷新、有未保存编辑保存拦截差异三选一，配置面 SDR-009）；推送面稳定后再替换轮询 |
| R2 | A4-1 双轨时序：rust 写通道（R4）交付前，round-trip 保真无完整验收承载 | 验收映射行显式标注「TS 不验收（现状保持），完整验收 R4」；TS 过渡期仅止血、不做 round-trip 承诺；R4 以 read-modify-write 测试向量收口 |
| R3 | 双引擎语义漂移（catalog 合并、报错文案、解析产物） | 同一 wire 契约 + R4 跨引擎同一份 `.md` 集合的 list 输出对比验收（US-C1）；解析产物逐字段对比测试（R2，从 TS 测试向量移植） |
| R4 | 开放裁决位（§5）未定案即实现，裁决散落 | 三项裁决前不落地实现；裁决回写本文档（ADR 追加），保持设计的单一事实源 |
