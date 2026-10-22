# Agent Profile 域 ROADMAP（Rust-first 批次结构）

> 状态：活文档（随执行推进更新） · 建立于 2026-09-09 ·
> **本文是 agent-profile 域执行状态的唯一家**（按 `docs/specs/AGENTS.md` 治理约定：设计文档不持有进度，批次拆分与验收映射在这里维护；本文不承载需求与设计内容）。
> 需求事实源：[agent-profile-settings-page-requirements.md](agent-profile-settings-page-requirements.md)（配置面 US-A1–A5、US-C1）· [agent-profile-consumption-requirements.md](agent-profile-consumption-requirements.md)（使用域 US-U1/U2 等）· 设计事实源：[agent-profile-settings-page-architecture.md](agent-profile-settings-page-architecture.md)（终态架构与 ADR-001~005、开放裁决位）· 现状参考：[agent-profile-settings-page-context.md](agent-profile-settings-page-context.md)
> 执行协议：每批工作走 SDD（brief → 实现 → 评审 → 提交 → 账本 `.superpowers/sdd/`）；批次实施细节见 `plan/agent-profile-rust-first.md`（R1 任务拆分 Task 0–6；R2–R5 每批启动时先跑批内「调研定案」再出实施细则）。
> 状态标记：☐ 待办 · ◐ 进行中 · ✅ 完成 · ⛔ 受阻（注明原因）。

## 1. 批次总览（R1–R5）

| 批次 | 内容 | 引擎 | 依赖 | 状态 |
|---|---|---|---|---|
| **R1** | 契约与页面：protocol schema 扩展 + TS 引擎最小集（W1/W2 重做）+ kap-server 读面迁移 + PUT 止血 + mirri-web 页面 | TS（过渡） | — | ✅ |
| **R2** | rust `.md` 格式对齐（SDR-002 落地）：frontmatter 解析 + roots/优先级语义 + `.toml` legacy 读兼容 | Rust（终态） | R1 契约 | ✅ |
| **R3** | rust 引擎语义：`enabled` 文件内化 + 退役 `disabled_agents` 双轨 + 报错语义 + essential 锁定 + suppressed/skipped 进 wire | Rust（终态） | R2 | ✅ |
| **R4** | rust 写通道（`.md` round-trip 保真）+ web rust 模式接线（配置页全功能）+ 双引擎一致验收（US-C1） | Rust + mirri-web 适配层 | R3 | ✅ |
| **R5** | 终端 `/agents` 同源化：SDK harness 数据面 + v2 服务下沉共享 + 终端改造 + `disabledAgents` 双轨终剥 | Rust（客户端直连）+ TS v2 过渡面 | R1–R4 | ✅ |

**R2 进度注记（2026-09-09，批次关闭）：** 格式对齐主体已交付 — Task 8 手写 frontmatter 解析器（零 YAML 依赖、超子集硬错）、Task 9 模型对齐（enabled 进模型、extends 废弃拍平、serde 双拼写读兼容）、Task 10 多格式发现层（.md>.toml>.json 去重、roots 对齐、skip-and-collect、rust 递归监听）、Task 12 kap-server 顺延项（含 TS 用户目录 fsWatch）、Task 13 web 顺延项、Task 11 跨引擎一致验收（共享 fixture 语料库 `packages/agent-profile/test/fixtures/cross-engine/`，TS `parseAgentFileText` 与 rust `parse_agent_file_text` 逐字段一致）。Task 14 收尾已完成：全量终验、尾注释分歧补进 fixture README 排除清单、CI 增设 rust 跨引擎一致测试 job（`cargo test -p mirri-core --test agent_md_cross_engine_tests`，机器化验收）、changeset 统一复核（rust `.md` 格式能力 + web denylist 编辑/能力人话 + server 校验/文件监听各一条）。已知尾注释（`tools: # note`）解析分歧为文档化例外，移交 R3+。

**R2 终验补记（2026-09-09）：** ① 归属修正——上文 Task 10 的监听指 rust 侧递归 watch（`spawn_watcher` recursive）；TS 用户目录 fsWatch 是 Task 12 顺延项交付。② R3 backlog 登记：watch 语义分歧对齐——rust 侧不监听尚不存在的候选 roots（root-creation 覆盖缺失，新建 `agents/` 目录需重启才可热重载）且坏保存时保留 last-known-good 注册，TS 侧则重新发现并丢弃；两侧对齐方向需在 R3 显式裁决。`tools: # note` 尾注释解析分歧（js-yaml 将列表绑定到键、rust 子集不绑定）已在 cross-engine fixture README 排除清单登记，同为 R3 对齐项。③ 可见性预期修正：rust REST / TS 角色可见性缺口**并非**「R2 格式统一后自然消解」——rust REST 写通道仍写 `.toml` 进 `profiles/`（TS 引擎从不扫描该格式目录），rust-REST 创建的角色对 TS 引擎不可见，直到 R4 rust 写通道切 `.md` 才消解。R4 终验修正（2026-09-10）：**已消解**——Task 23/25 将写通道切 `.md` 并指向 agents root，rust REST 创建/修改的角色以 TS 引擎可见的格式落盘，计划 §7 所登记的「rust REST 写 `.toml` 产品债」就此关闭。

**R3-7 分歧裁决（2026-09-10，Task 21）：** ② 中的两项 R3 对齐项已裁决并清偿。其一（root-creation 覆盖）：rust 侧已按 TS `watched_parent_dirs` / `subtreeWatchFilter` 模式对齐——尚不存在的候选 root 通过其已存在的父目录监听（事件过滤收窄到各 root 子树，含 macOS 路径别名的双拼写字形），新建 `agents/` 目录即刻可热重载，无需重启；父链整段不存在的 root 仍不监听（与 TS generic `.agents` root 行为一致，等下次显式 reload）。其二（坏保存语义）：裁决为**有意分歧**，不强行对齐——TS `UserAgentProfileLoaderService` 每次事件全量重算 contribution（坏文件掉出注册表），rust watcher 按 R2-3 契约保留 last-known-good 注册；任一方向对齐代价都不小（rust 侧丢弃将撤销 R2-3 已验收的热重载安全契约并使坏保存注销在役角色；TS 侧保留需在 workspace 作用域 loader 内新增按文件 memo 状态），且两侧各自契合宿主模型（声明式 contribution 重算 vs 必须持续服务的命令式注册表）。裁决已记入 `rust/crates/mirri-cli/src/watcher.rs` 的 `reload_profiles` 文档注释。`tools: # note` 尾注释分歧已在前项清偿：rust 解析器对齐 js-yaml 绑定语义，语料样本 `trailing-comment-list.md` 已入跨引擎语料库，README 排除清单条目已移除。

**R3 进度注记（2026-09-10，批次关闭）：** 引擎语义主体已交付 — Task 15 `enabled` 引擎消费（派发面过滤、`list_all` 管理面「列出但标记」、显式指定报错文案逐字对齐 TS `requireSubagentProfile`）、Task 16 essential 引擎级锁定（`register`/`upsert`/`delete`/发现路径守卫，同名用户文件压制为 `builtin-override-required`）、Task 17 `disabled_agents` 双轨退役（config 键保留解析/round-trip/wire 暴露 + 启动弃用警告，不再作为启停事实源，用户文档同步退役）、Task 18 子角色名字派发白名单（模型收敛为 `Option<Vec<String>>` + Agent/Swarm 工具候选交集 + 派发执行拒绝越权 + legacy `.toml` 内联定义迁移）、Task 19 多来源合并投影（优先级胜出、落败者进 `suppressed_candidates`、`profile_wire_row` 补齐缺字段与 `ProfileSource` 六值标注）、Task 20 `agent.listProfiles` 升级 `{items, skipped}` 信封（EMB RPC 与 serve REST 两面）+ web RPC 模式读面全量消费（替换过期两值形状与硬编码空 skipped）、Task 21 R2 移交债清偿（`extends: null` 语义、`tools: # note` 尾注释绑定对齐入跨引擎语料、`rest-config` 改从 `@mirri-ai/protocol` 复用、不可读子目录 warn-skip、root-creation watch 覆盖、bootstrap 测试封闭性——裁决记录见上方 R3-7 节）、Task 22 收尾终验（五 TS 包 typecheck + vitest 全绿；rust 三门禁绿——`cross_backend_contract` 的 3 例 session 域存量失败按台账引用豁免：fact-table 计数漂移 / model.list / session.export，均为既有债、非本批引入；changesets 就绪）。移交 R4：web rust 模式写通道（六个 `not supported` 桩）、CRUD 输入字段面、跨引擎同一集合验收（US-C1）、effective system prompt 用户行裁决、discover 排序与优先级关系、CRUD 删除与 `.md` 覆盖率、`disabled_agents` 的 v2 持久化取舍（见 R4 开放问题）。

**R4 开放问题（2026-09-10 已全部裁决关闭，记录留档）：**

- `disabled_agents` 的 v2 持久化取舍（Task 21 调查发现）：**已裁决（ADR-009）**——配置写入面显式忽略该键，读取兼容与弃用警告保留，弃用警告进诊断面；web 配置写入剥除该键映射。kap-server 的 `rest-config` 复用 `@mirri-ai/protocol` 配置超集后，该 schema 含 `disabled_agents` 键，v2 `POST /config` 因此会把它持久化进 config.toml（v1 对等方向）。而 rust 引擎侧该键已退役为「读取兼容、不生效、弃用警告」（R3-3），TS v2 引擎亦无此消费方。web 侧 `config.set` 仍会把 `disabledAgents` 写回 `disabled_agents`（`apps/mirri-web/src/api/daemon/client.ts`），是 v2 持久化的实际写入方之一。
- US-C1 已知分歧——essential 同名用户文件的 `skipped` 双通道（R3 终审发现）：**已裁决（ADR-006 + ADR-008）**——同名文件正常加载并覆盖内容（保护收缩为不可删/不可禁），双通道场景自然消失；`skipped` 通道语义固化不变式：仅加载失败。原记录：rust 侧该文件**同时**出现在 `agent.listProfiles` 的 `skipped`（token `builtin-override-required`）与 essential 行的 `suppressed_candidates` 两条通道；TS kap-server 的 `skipped` 只含解析失败，该文件仅进 inspection.suppressed。这是 R3-2 的计划内裁决结果（非缺陷），但意味着 web 的「解析失败警告区」会展示一个可读且合法的文件。

**R4 裁决（2026-09-10，批内调研定案）：** D12 生效提示词全行附带（两引擎统一，配置面 SDR-013 / ADR-007）；D13 skipped 通道仅承载加载失败（ADR-008）；D14 同名文件即覆盖意图、无声明——TS 放弃 `override: true` 要求（使用域 SDR-011 / 配置面 SDR-012 / ADR-006，用户要求记住该决策）；D15 `disabled_agents` 配置键写入面彻底退役（ADR-009）；D16 缺省主 agent 内容可覆盖（兜底保护收缩为不可删/不可禁，页面可编辑附警告，ADR-006 / 使用域 SDR-011）。原登记的 R4 开放问题两项随裁决关闭：`disabled_agents` v2 持久化取舍 → ADR-009（写入面忽略）；essential 双通道 US-C1 分歧 → ADR-006 + ADR-008（同名文件正常加载，双通道自然消失，语义不变式固化）。

**R4 进度注记（2026-09-10，批次关闭）：** 写通道与接线主体已交付 — Task 23 `.md` 写通道（行级外科改写纯函数；upsert/delete 切 `.md` read-modify-write，round-trip 保真：未知键与正文逐字保留、未变键零编辑、损坏 `.md` 守卫回退；legacy `.toml`/`.json` 写出清理；handlers 指向 agents root——R2 终验补记 ③ 的可见性缺口随之消解）；Task 24 CRUD 全字段面（`disallowed_tools`/`subagents`/`capabilities_required` 进 ProfileFields；create 校验链 TS 顺序逐字镜像，9 对错误文案字符级对齐）；Task 25 enable/disable/reset 三 RPC 五环（registry/TSV/EMB 路由/NAPI/fixture 计数 + toggle 幂等 no-op 语义 + 错误文案字符级，含 essential disable/reset/unknown 家族）；Task 26 web rust 模式六桩接线（三生命周期 fire-and-forget 后经 listProfiles 重取行）+ essential 编辑面（D16 兜底警告 Banner + 置灰 Delete/Switch 守卫，i18n zh/en）+ 真实 rust serve 浏览器冒烟（token 模式 6 步全过）；Task 27 双引擎对齐（D16 rust 发现层放开 essential 同名覆盖、D14 TS catalog 放弃 `override: true` 且写侧停发、D12 `effective_system_prompt` 全行附带 + web 详情全角色只读预览、D13 skipped 仅加载失败不变式测试；builtin/has_override wire 矩阵跨引擎核对等价；ADR 追记 6b95a956）；Task 28 US-C1 收口（跨引擎列表投影验收机器化：共享语料 15 例 + `expected-list.json` 共享期望 + 双侧独立归一化断言 + CI `rust-agent-md-parity` job 扩展；坏 extends 伪候选修复——builtin-loser 追加移 flatten 后以 survived_flatten 门控；例外清单四条入 fixture README）；Task 29 移交债清偿 + D15 三侧落地（discover 按 priority 降序稳定排序单点（T19 m1 根因收口）、essential warn-once 闩、`available_profiles` 单 resolve、具名调用者缺席白名单测试（注：该测试锁已随 2026-10-21 消费域 SDR-012 默认拒绝语义移除——缺席 profile 现为不可派发，分歧不复存在）、rust `config.diagnostics` 弃用警告、TS `POST /config` 丢弃 `disabled_agents`、web `setConfig` 剥键 + Omit 类型收紧、TS spec sleeper/coder 硬编码断言补齐）；Task 30 全量终验（本批关闭）。门禁基线如实记录：`cross_backend` 剩 **2 例**存量失败（model.list 形状、session.export——fact-table 计数漂移已由 Task 25 收口修绿），按台账引用豁免（用户 session 域存量，非本批引入）；compaction 域波动为用户并行工作域。

**R5 裁决（2026-09-10，批内调研定案）：** D17（对接侧，用户裁决）= SDK harness 引擎路由——rust 客户端直连既有 `agent.listProfiles`/`agent.enable/disableProfile` RPC（零新 rust 工作），v2 补最小过渡面（kap-server 读/写下沉 agent-core-v2 共享服务 + klient wire），v1 抛 NOT_IMPLEMENTED；D18（v1 行为，用户裁决）= 降级提示；D19（信息密度）= 最小对齐 SDR-010。调研事实：终端 `/agents` 改造前为硬编码 4 内置名 + `disabledAgents` 写入链（默认 v2/rust 引擎下均 placebo）。任务拆分 Task 31–36 已入计划（调研报告 `.tmp/r5-investigation.md`）。R4 遗留 6 跟踪项处置：交接项 6（WS 面剥除）并入 Task 34，其余 5 项留批次外各有归属。

**R5 进度注记（2026-09-10，批次关闭）：** 终端同源化主体已交付 — Task 31（node-sdk harness 数据面方法 `listAgentProfiles()`/`setAgentProfileEnabled(name, enabled)`：rust 客户端直连既有 RPC，行形状经 protocol `profileEntrySchema` parse 断言，v1 抛 `NOT_IMPLEMENTED`，错误经 `decodeRustEngineError` 透出）；Task 32（v2 数据面下沉共享：agent-core-v2 新增 App-scope 角色管理服务（读投影 + 文件写通道单点，读带 workspace root 入参与全局 fallback）、klient contract/facade wire 面、`SDKRpcClientV2` 接线、kap-server 路由换源消费同一服务——读/写与 REST 端点同一实现，净删 ~470 行，1037 例零回退验收；评审修复：M1 用户根注册晚于 project 注册的投影等待收口、M2 suppressed reason 收窄为字面量联合、terminal 读面免传 workspaceRoot 对齐 REST 全局视图语义）；Task 33（终端 `/agents` 改造：删除 `BUILTIN_AGENT_PROFILES` 硬编码与 `disabledAgents` 双轨写入链，列表/启停/essential 标记全部来自 harness 数据面，essential 拒绝文案由数据面透传（不再本地硬编码），v1 引擎降级提示（D18-a），skipped 发现文件一行警告，首个命令级测试 8 例）；Task 34（双轨残留终剥：v2 config mapper 移除 `disabledAgents` 域（`MirriConfig.disabledAgents` 对 v2 引擎不再填充，v1 不受影响）+ web WS 面 `config.set` 运行时剥键（R4 交接项 6 核销，纵深防御）+ web 测试 1 例）；Task 35（双语文档：`docs/{en,zh}/reference/slash-commands.md` 补 `/agents | /agent` 行，`docs/{en,zh}/customization/agents.md` 措辞更新为引擎数据源 + 同一文件通道 + essential 不可禁）；Task 36（终验 + changeset + 本注记，批次关闭）。门禁基线如实记录：rust 三门禁绿（R5 零触碰 rust 代码，`cross_backend` 2 例存量（model.list 形状、session.export）按台账引用豁免）；node-sdk typecheck 4 处（rust-engine-e2e.test.ts 既有）与 mirri-code typecheck 1 处（`test/tui/mirri-tui-message-flow.test.ts:1121` 缺 `pool`——用户 M7 提交 `5e172f37` 将 `McpServerStatusPayload.pool` 改为必填所致，非本批引入）；node-sdk vitest `translate.test.ts` 1 例既有失败；kap-server `agents-cross-engine-list.test.ts` 全量运行下存在既有竞态（用户根注册可晚于 project 注册，快照断言可能先行——R4 收口基线 `7c764b75` 复现同款翻红，其中 `agents.test.ts` 的同类竞态已在本批 Task 32 评审 M1 收口，cross-engine-list 的 poll 谓词仍待同样收口，建议跟踪）；agent-core-v2 / klient / mirri-web typecheck 与 vitest 全绿。

**计划收束注记（2026-09-10）：** **本计划（agent-profile rust-first）全部批次 R1–R5 完成，计划关闭。** 配置面（US-A1–A5）、引擎语义（R3）、写通道与 web 接线（R4）、终端同源化与双轨终剥（R5）均已交付并终验；后续工作入口为 §3 的遗留清单（建议跟踪工单项 + 遗留 minors）与验收映射表 ⏭ 行（A1-4 用途徽章随 U1.1 批次，SDR-004）。

**边界说明**：
- R1 为 TS 过渡最小集（决策见架构 ADR-003）：只做契约与页面消费所必需的引擎最小改动（`enabled` 进 schema + catalog 单点消费 + PUT 止血），**不建** TS FileStore、不做写通道全迁移、不做终端同源化。
- R5 原为占位批次（终端对接哪侧引擎取决于引擎切换时点的架构，架构 ADR-005）；批内裁决 D17 定案为 SDK harness 引擎路由后已交付关闭（2026-09-10）。
- U1 系列（主 agent 选择入口、用途标记编辑 US-B6）不属 R1–R5 范围，随 U1.1 批次交付（配置面 SDR-004；对应验收行 A1-4 见下表 ⏭）。

## 2. 验收映射（US-A1–A5 P0 + U2.1/U2.5 → 批次双栏）

| 条款 | TS 过渡（R1） | Rust 终态 |
|---|---|---|
| A1-1 列表字段 + source 徽章 | ✅ Task 3/4/5（wire + 页面） | ✅ Task 19/20（wire 行补全 + web RPC 模式读面消费） |
| A1-2 压制候选可展开 | ✅ Task 3/4/5 | ✅ Task 19/20（`suppressed_candidates` 进 wire + web 消费） |
| A1-3 解析失败可读警告 | ✅ Task 3/4/5 | ✅ Task 20（`{items, skipped}` 信封 + web 消费真实 skipped） |
| A1-4 用途徽章/筛选 | ⏭ 随 U1.1（SDR-004） | 同左 |
| A2-1 详情全部字段 | ✅ Task 4/5 | ✅ Task 19/20/27（wire 字段面补全 + effective system prompt 全行附带，D12） |
| A2-2 内置提示词只读预览 | 既有，回归验证 | ✅ D12 后全角色预览（Task 27 web 详情放开，非仅内置） |
| A2-3 能力包人类可读 | ✅ Task 13（R2 顺延交付） | 同左（纯前端） |
| A3-1~A3-4 创建流 | ✅ 既有回归 + Task 4 | ✅ Task 23/24（写通道 `.md` 化 + 全字段面 + 校验文案 TS 逐字对齐） |
| A4-1 round-trip 保真 | **TS 不验收**（现状保持，D3） | ✅ Task 23（round-trip 保真：未知键/正文逐字保留、未变键零编辑） |
| A4-2 工具面单概念编辑 | ✅ Task 4/5/13（denylist 编辑面 R2 顺延交付） | 同左 |
| A4-3 子角色启用勾选 | ✅ Task 4/5 | ✅ Task 15/17/26（引擎消费语义 + enable/disable RPC + 页面启停写通道接线） |
| A4-4 实时同步 + 冲突人定 | ✅ Task 5 + Task 12（用户目录 fsWatch） | 同左 |
| A5-1 禁用即移除 + 校验拒绝 | ✅ Task 1/2 | ✅ Task 15/17/25/26（派发移除 + 校验拒绝 + 事实源内化 + 生命周期 RPC + 页面接线） |
| A5-2/A5-3 删除与 essential | 既有回归 | ✅ Task 16/23/27（essential 引擎级锁定 + 删除写通道 + D16 内容可覆盖、页面可编辑附警告） |
| U2.1-2 / U2.5-2 消费语义 | ✅ Task 2 | ✅ Task 15（派发候选排除 + 报错文案对齐） |
| C1 双引擎同一份角色集合 | — | ✅ Task 28（跨引擎列表投影共享语料 + 共享期望 + CI 机器化；`.md` 解析级一致 R2 Task 11） |
| web rust 模式配置页全功能（创建/编辑/删除/启停） | — | ✅ Task 26（六桩接线 + essential 编辑面 D16 + 真实浏览器冒烟） |
| A6 覆盖内置角色的引导（同名文件即覆盖意图，SDR-012/D14） | ✅ R1 Task 4/5（页面引导）· R4 Task 27（放弃勾选声明，语义两引擎统一） | 同左（ADR-006） |
| A7 终端与设置页的对等（列表 + 启停，SDR-010） | ✅ R5 Task 32/33（v2 经共享服务同源；v1 降级提示 D18-a） | ✅ R5 Task 31/33（客户端直连 RPC 同源） |

> 非功能条款达成注记：N4 双端一致随 A7 达成——R5 起终端 `/agents` 与设置页的角色集合、启用状态同源（同一数据面：rust 直连 RPC / v2 共享服务），`disabledAgents` 双轨已终剥（Task 33/34）；唯一例外为 v1 引擎降级提示（D18-a，随 v1 退役消解）。

> Task 编号（Task 0–6）为 R1 批次内部执行拆分，见 `plan/agent-profile-rust-first.md` §4；R2–R4 的批内「调研定案」与开放裁决位（YAML 依赖选型、`extends` 继承去留、`.md`/`.toml` 优先级）见架构文档 §5。

## 3. R5 交接清单（2026-09-10，R4 批次关闭显式化；R5 批次关闭时核销）

**核销注记（2026-09-10，R5 批次关闭）：** R5 主项（终端 `/agents` 同源化）✅ 已交付（Task 31/32/33）；交接项 6（WS 面 `disabledAgents` 运行时剥除）✅ 已交付（Task 34）。其余 5 项留批次外，去向显式记录如下（逐项标注）。

- **R5 主项（终端同源化）**：✅ **已核销（Task 31/32/33）**——列表经 harness 数据面、启停走文件通道（与设置页/REST 同一实现），`config.disabledAgents` 双轨在终端、v2 SDK 面、web WS 面全部终剥（SDR-010 / 架构 ADR-005 / D17）。
- **遗留 minors（抄录自台账 R4 段移交内容，非阻塞）**：
  - Task 23 nit：`needs_terminator` 分支无测试；`watcher.rs` bootstrap 注释过时（现无条件建 agents 目录）。
  - Task 24：create 面空白 `system_prompt_template` 胁迫占位符（TS serializeAgentMd 逐字对齐的有意分歧；create-with-blank 无专门测试）；update 非 kebab 未知名 rust -32001 vs TS 400（两侧都拒，web 映射知悉）；None-over-Some 删除分支覆盖弱化；损坏 `.md` 回退仅 tracing::warn 不上 wire。
  - Task 25：重启后 no-op 判据用注册态而非文件态（disabled override 重启输给 builtin 后 enable no-op 不清文件陈旧 `enabled:false`）；disable legacy `.toml` 注册（无 `.md` 路径）无直接测试。
  - Task 26 nit：死 CSS `.system-prompt-textarea:disabled`、requestId 语义混用；稀疏 update（字段缺省不入 wire）无正例测试。
  - Task 27：死 wire 词汇 `builtin-override-required` 残留三处（protocol 枚举/catalog union/web i18n，两侧无 producer，清理候选；R5 Task 34 顺手项未做——按「核对无字面值匹配消费后再删」的前置条件保留）；web 删除门控矩阵无组件测试锁定（有 @vue/test-utils 基建）。
  - Task 28：oxlint 对 kap-server 整包忽略→「零发现」空真（存量）；rust 复刻 normalize_row 不走真实 wire 路径（嵌入式真实 wire 行另有直接覆盖，双处同步维护提醒已注释）。
  - Task 29：warn-once 进程终身制备忘（删重加不再二次 warn 为规格所求，未来测试需进程首例）；`scanned_roots` 仍按调用方序收集（无消费者，惰性字段，上 wire 前需文档化）。
  - Task 32（R5 追记，**已收口**）：kap-server `agents-cross-engine-list.test.ts` 的投影等待谓词只等 project 注册到位，用户根注册晚于它时全量运行可闪断——R5 终审 fix wave 已将两处谓词补齐 user 落败候选等待（与 `agents.test.ts` 的 81be30b5 同模式），全量 1037/1037 复验通过。
- **建议跟踪工单项**（R5 关闭时去向标注）：
  1. 2 例 `cross_backend` 存量失败（model.list 形状、session.export——用户 session 域，fact-table 已由 Task 25 修绿）——**建议工单**（session 域存量，与 agent-profile 域无关）。
  2. TS PUT essential 拒绝（40302 家族）过渡期分歧——web essential 编辑仅 rust RPC 路径生效，HTTP 模式 Save 报错（ADR-006 文档化有意分歧）——**待产品裁决**（过渡期语义收敛方向）。
  3. web Reset 按钮死门控——`isEditingBuiltin && hasOverride` 双引擎均不可达（覆盖行 `builtin:false`），reset UX 重设计候选——**待产品裁决**。
  4. sleeper 自指 suppressed 候选语义——行把自己列为 disabled 落败者（两侧一致锁定），候选产品裁决改空或显式标记——**待产品裁决**。
  5. rust 生产侧 project 根缺失——跨引擎验收当前靠测试根顺序复刻——**独立后续任务**（引擎基建，非 agent-profile 域）。
  6. ~~WS 面 `disabledAgents` 运行时剥除纵深防御~~ ✅ **已核销（Task 34，2026-09-10）**——web WS 面 `config.set` 补丁在透传前运行时 delete 该键，测试 1 例锁定。
  7. （R5 追记，**已在 R5 终审 fix wave 修复**）kap-server `agents-cross-engine-list.test.ts` 用户根投影竞态（见上「遗留 minors」Task 32 条）——测试稳健性收口，非行为缺陷，1037 全量复验绿。
