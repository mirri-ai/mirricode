# 定时任务 · 用户直建表单架构设计（cron-task-creation-ux-architecture）

> 本文档是 [cron-task-creation-ux-requirements.md](cron-task-creation-ux-requirements.md)（下称「表单需求」，SDR-107~113 / VU1-18）的技术承载：表单的结构化日程选择器、三时刻预览、三区块信息架构与环境五项的存储-透传链。与主架构文档 [cron-scheduling-architecture.md](cron-scheduling-architecture.md)（下称「主文档」）的关系：**主文档仍是域内调度、执行、审批、存储等机制的唯一源，本文是「创建表单」专项**——凡本文涉及域内既有机制（cron 求值、RunSpawner、提议通道、无人值守姿态）只引用主文档的章节与 ADR 编号，不重定义；本文新增的机制裁决为 ADR-016 起顺延编号，其影响需回写主文档相应章节的清单见 §9。现状代码锚点引用 [cron-scheduling-context.md](cron-scheduling-context.md)（下称「context 文档」）。
> 运行目标为 Rust core + web（引擎目标门：全部机制落 `rust/crates/`，前端落 `apps/mirri-web`）。

## 0. 总览与数据流

表单是一条「前端结构化 draft → 编译为 cron 表达式或 interval 结构 → 既有创建通道」的管线（日程表达二选一互斥——SDR-113），**不在创建路径上引入任何新语义**：

```text
┌─ 前端（apps/mirri-web）──────────────────────────────────────────┐
│ CronCreateForm（三区块容器，复用 CronPanel 的挂载点与提交通道）    │
│  ├─ SchedulePicker（结构化日程，§1）                              │
│  │    └─ CronSchedule 中间表示（§1.1）                           │
│  │         ├─ compileSchedule() → cron 表达式 | IntervalSpec      │
│  │         │      （纯函数分流，§1.3——ADR-016/SDR-113）           │
│  │         └─ describeSchedule() → 可读预览（纯函数，§1.4）      │
│  ├─ 环境五项选择器（§3，复用 Session 面既有组件/纯函数）           │
│  └─ cron.preview RPC（§2，新增只读求值面）                        │
│       → 三时刻预览（上次/下次/下下次，pre-jitter 计划时刻）        │
├─ wire ───────────────────────────────────────────────────────────┤
│ cron.add / 工作区直建 RPC（既有，字段扩展 §4.1 含 endsAt/interval）│
├─ 服务层 ─────────────────────────────────────────────────────────┤
│ CronService.create（主文档 §2，签名扩展 §4.2，含权威校验 §4.1）    │
│  └─ 校验族沿用（上限/8KiB/350 天/5 年）+ 新字段缺省解释            │
├─ 核心库 ─────────────────────────────────────────────────────────┤
│ CronTask 扩展字段（§4.1）；日程求值 = 核心库唯一实现（ADR-017）    │
└─ 执行（每次运行）────────────────────────────────────────────────┘
│ RunSpawner（主文档 §8）：环境字段消费点（§4.3）                   │
└──────────────────────────────────────────────────────────────────┘
```

设计总原则（对应表单需求 §0「复用优先/零新概念」）：

- **创建通道零分叉**：表单提交仍走既有 `cron.add`（会话级）与工作区直建 RPC——扩展的是参数面，不是新端点；
- **求值零重复**：cron 表达式的一切时刻计算只在核心库一处（既有 `compute_next_fire` / `next_fire_for` 家族，`rust/crates/mirri-core/src/cron.rs`），前端不自建 cron 求值（ADR-017）；
- **选择器零重造**：环境五项复用 Session 创建面已在用的组件与纯函数族（§3 逐项列名）；
- **呈现与执法分界**：表单的一切「模式名/预览」都是呈现层，真值（模式映射、时刻、生效环境）在后端（ADR-018）。

## 1. 结构化日程选择器（SDR-108 / 表单需求 §2.2 §3.1-3.3）

### 1.1 组件与位置

新增 `apps/mirri-web/src/components/settings/cron/SchedulePicker.vue`，独立于 CronPanel（cron 面板本体已承载列表/提议/运行记录三个面，表单再内嵌会超出单组件合理规模）；`CronCreateForm.vue`（§3）作为三区块容器挂进 CronPanel 的创建表单位置，替代现状的裸表达式表单形态（context 文档 §4：创建面当前为表达式输入框与指示文本框的直接拼装）。

- **空态**：仅「+ 添加日程」按钮；点击展开六档下拉（每小时/每天/每个工作日/每周/每月/自定义）——六档定义为封闭枚举，无扩展点设计（预设档的新增属产品裁决，不在机制层留暗门）。
- **已选态**：区块标题旁垃圾桶按钮一键清空回空态，无确认弹窗。
- **Custom 档**：打开「自定义重复（Custom Repeat）」弹窗（`ui/Dialog.vue` 既有弹窗基建）——数字 + 单位下拉（频率）、单选 Ends（永不结束为默认）/ 结束日期（复用日期选择交互，含月历网格与月份翻页；本域不新增日历基建，若 UI 库无现成日历则列为实施前置项，机制形状不受影响）。取消作废、确认回写。

### 1.2 中间表示（CronSchedule，ADR-016）

表单状态**不持有 cron 字符串**，持有结构化 draft——六档是判别联合（discriminated union），各档子选项独立、切换档位即整体重置（表单需求 §3.1「切换不保留」的直接结构表达——重置由类型系统保证而非 UI 约定）：

```text
CronSchedule =
  | { kind: 'hourly',  minute: 0-59 }
  | { kind: 'daily',   time: ClockTime }                        // hh:mm
  | { kind: 'weekday', time: ClockTime }                        // Mon-Fri
  | { kind: 'weekly',  days: NonEmptySet<Mon..Sun>, time: ClockTime }
  | { kind: 'monthly', day: 1-31, time: ClockTime }
  | { kind: 'custom',  mode: 'field' | 'interval' | 'raw',       // SDR-113：两模式 + 高级 raw
                       // field 模式（cron 可表达）：
                       field?: { unit: 'day'|'week'|'month', picks: …, time: ClockTime },
                       // interval 模式（流逝间隔——非 cron，结构化字段）：
                       interval?: { every: 1-N, unit: 'day'|'week'|'month', time: ClockTime,
                                    anchor: CalendarDate },      // 锚点 = 首次运行日（首计划时刻的本地日）
                       raw?: { expr: string },
                       ends: { kind: 'never' } | { kind: 'on', date: CalendarDate } }
```

- **interval 变体（ADR-016 修订 / SDR-113）**：Custom 的「每 N 天/周/月」是流逝间隔语义——5 字段 cron 的 dom/dow 是字段枚举（每月 N 号 / 每周几），表达不了「每 N 天」（`*/N` in dom 跳过无 N 号月份、月底断链，非用户输入意图）；interval 走**结构化字段 + 求值内核间隔分支**，不编译为 cron 表达式。cron 与 interval **互斥二选一**（一个任务要么 `cron` 字段、要么 interval 结构，绝不同时持有——服务端 create 对同请求两者并存做拒绝，见 §4.1 校验）。
- `CronCreateDraft`（既有创建草稿模型）以 `schedule: CronSchedule | null` 取代裸 `cron: string`（`cronManagement.ts` 的草稿接口扩展；raw 高级入口见 §1.5，仅 field 模式可用）。
- **Ends 边界（SDR-110）**：`ends: on(date)` 只表达「最后运行不晚于该日（含当日）」；一次性 = 首次触发日恰为结束日，是**推导属性而非独立开关**——判定逻辑在校验/预览函数内（以后端 preview 返回的首时刻与 `ends.date` 比较，§2.4），不是 draft 的一个字段。这从结构上排除「结束日期 < 首次触发日」之外的第三态。该推导对 cron 与 interval 两种日程类型同构成立（SDR-113 下两类型通用，勿引入第二种一次性表达）。
- draft 定义为**可序列化纯数据**（JSON 兼容），编译与描述函数（下）与其同放一个纯函数模块（`cronSchedule.ts`，无 Vue 依赖）——结构、编译、校验、描述四件事在一个可单测的模块里闭环。

### 1.3 编译：六档 → cron 表达式（ADR-016）

纯函数 `compileSchedule(schedule, timezone): CompiledSchedule`，放前端 `lib/cronSchedule.ts`（与既有 `cronManagement.ts` 同层的纯逻辑层）。**按档分流（ADR-016 修订 / SDR-113）**——cron 档位产出表达式，interval 产出结构化字段：

```text
CompiledSchedule =
  | { type: 'cron',  cron: string,           endBoundary: Date | null }
  | { type: 'interval', interval: { every, unit, time, anchor }, endBoundary: Date | null }
// endBoundary：custom+ends.on：结束日（含当日）的本地日界；两种类型同构携带
```

- cron 档编译是直译（每小时 at :03 → `3 * * * *`；每天 09:03 → `3 9 * * *`；工作日 → `* * * * 1-5`；每周多选 → dow 列表；每月 day N → dom；custom field 模式 → 组合规则的表达式合成）。**interval 档不产 cron 表达式**——`every/unit/anchor` 原样进 `CompiledSchedule.interval`（wire 提交值，见 §4.1）。首次触发时刻**不由前端求值**——由 §2 的后端 preview 承载（interval 的 preview 由求值内核间隔分支解出），前端仅作提交前的快速可用性判断（字段齐全即结构合法，沿用既有 submittable 惯例）；**首次触发日与结束日的先后比较以 preview RPC 返回的首时刻为准**（§2.4），避免前端自算时区日历引入第二求值。
- **否决备选**（ADR-016）：直接以 cron 字符串为 draft（每档 UI 只是表达式生成器）。否决理由：a) 「切换档位重置子选项」需要额外 UI 约定防止旧值渗入新表达式；b) 一次性判定（SDR-110）在字符串层无结构可依，需解析表达式反推——为同一信息造第二次表示；c) 可读预览（§1.4）若从表达式反解析生成，与结构化选项之间出现「显示 ≠ 所选」的裂缝；d)（SDR-113 补）「每 2 天」编译为伪 cron 表达式在语义上失真（见 §1.2）。结构化 draft 使「所选 → 提交值 → 描述」三者同源（一次编译、两处消费）。

### 1.4 可读描述预览：复用编译产物 + 既有 humanize 口径

`describeSchedule(schedule, timezone, t): string` 与编译同模块——**描述从 CronSchedule 结构直接生成**（如 `GMT+9 Weekly on Mon, Fri at 09:03`、`GMT+9 Every 2 days at 09:03`），不从 cron 字符串反解析。与既有 `cronHumanize.ts`（聊天提示卡的表达式→短句转换）的关系：

- **边界**：`humanizeCron` 服务「既有表达式 → 简短标签」的**反向**场景（表达式是输入、结构是未知）；表单是「结构已知 → 表达式与描述」的**正向**场景。复用其**口径**（句式族、i18n key 风格、未知时回退原表达式的诚实原则），不复用其**实现**（它以字符串模式匹配为骨架，正向场景下用它等于先编译再反解析，制造 §1.3 否决的裂缝）。
- **防口径分裂（表单需求 §3.4 R4「显示=实际」）**：一致性由「单次编译、双消费」保证——`describeSchedule` 与 `compileSchedule` 接收同一 `CronSchedule` 值，描述的每个字段（频率/星期/时刻）即编译进表达式的同一字段；模块内以同一中间结构（档位 + 子选项）为唯一输入，不存在第二真值源。测试口径：对每档枚举一组 (CronSchedule, 期望描述, 期望表达式) 三元组（§6）。
- 时区前缀（`GMT+9` 形态）由 describe 按 `schedule` 之外的 `timezone` 参数拼接（时区不属于日程结构——改时区不改日程，只改呈现与求值基准，表单需求 §2.2/§3.4）。

### 1.5 cron 表达式的高级入口（表单需求 §3.2 末条）

结构化选择是唯一主入口；技术表达式仅存在于 Custom 弹窗的「高级（Advanced）」折叠区：一个等宽表达式输入框，输入即**替代**该弹窗的结构化选项（draft 的 custom 档 `raw` 模式——`{ expr }` 直接以 expr 为编译输出）。**raw 只对 field 模式可用**（SDR-113：cron 表达式表达不了流逝间隔——interval 模式下高级折叠区如实提示该边界，不提供伪表达）。R5（高级入口残缺即缺陷）由「raw 输入同样获得三时刻预览与描述（描述回退显示原表达式，沿用 humanize 的诚实回退原则）」满足。

## 2. 三时刻预览（SDR-112 / 表单需求 §2.2 §3.6）

### 2.1 计算位置：后端单一求值（ADR-017）

cron 表达式求值（时区日历、DST、月末日、occurrence 步进）在核心库是唯一实现（`compute_next_fire` 家族按任务时区求值，`rust/crates/mirri-core/src/cron.rs`）。前端 TS 重复实现必然漂移（DST 规则、月末缩布、时区数据版本），且预览的全部价值是「显示=实际」——**漂移即违背 R4**。故三时刻由后端计算。

**否决备选**：前端本地引入 cron 求值库。否决理由：a) 双实现漂移直接威胁 R4；b) 引入新前端依赖违背最小化；c) 核心库已带全部所需能力（含时区解析），后端算的边际成本为零。

### 2.2 RPC 形态：扩展 `next_fire_for` 为 `cron.preview`（新增只读投影面）

核心库已有静态求值 `CronManager::next_fire_for(cron, timezone) -> Option<DateTime<Utc>>`（只算「now 之后的下一次」，无副作用、不落任务表）——**复用其求值内核，扩展其窗口与输入**：

```text
// mirri-core：新增（与 next_fire_for 同族、同一 compute_next_fire 内核）
CronManager::preview_fires(schedule: &ScheduleSpec, timezone, count, anchor) -> Vec<DateTime<Utc>>
// ScheduleSpec = Cron(expr) | Interval { every, unit, time, anchor_date }   // SDR-113：互斥二选一
// 顺次解出 anchor 之后（含前向）的 count 个理想计划时刻；不可用表达式/时区 → 空 Vec
// interval 变体：求值内核的间隔分支——从 anchor_date 起按 every×unit 步进（time 为每日内时刻，
//   按任务时区解析；含 DST/月末口径：日历加法而非固定秒数，时区数据与 cron 求值同源）
// anchor 缺省 now；「上次」不回溯推算（无消费点，M2 裁决）——表单态恒「尚未运行」，常驻态用运行记录投影（M2 复核修正：last_fired 是调度游标，skip 时也推进，不能承载「实际运行」语义）
```

wire 面：`cron.preview { schedule | cron, timezone?, anchor? } -> { occurrences: [t0, t1, …] }`——只读 RPC，与 `cron.list` 同注册族（无任务副作用、无状态、不进调度器循环）；MIRRICODE_DISABLE_CRON 禁用态**不**拦截 preview（它是显示投影，非调度行为——与主文档 ADR-006 列表可读同口径）。

- **三时刻的组装**：表单调一次 preview 取 t0、t1（「上次运行」在表单态**恒显示「尚未运行」**——新建任务无历史，诚实口径 R4；不回溯推算「假如之前有」的假想时刻，该回溯无消费点且与常驻态口径分裂）。
- **常驻态（VU18）**：任务列表/详情的三时刻 = 同一 `preview_fires`（对已存任务的日程——cron 或 interval，取 next 两个）+ 运行记录投影——常驻态与表单态共用同一投影函数，`cron.list`/`cron.listAll` 视图扩展 `nextTwoFires`（或前端以 `nextFireAt` + 一次 preview 补齐第二个，实施择一，口径不变：均为后端求值的计划时刻）。「上次运行」= **最近一次非跳过运行的开始时刻**（真值承载 = 运行记录：该任务 runs 中最近一次 status ≠ Skipped 的 started_at；跳过/合并不产生新值）——**不由调度游标（last_fired）承载**：游标在到期检测时即推进、与运行是否被跳过无关，用作「上次运行」显示会把跳过伪装成运行过（R4 违背）；游标继续专职调度（恢复重算 base、TTL/到期执法基准），两真值各司其职（表单需求 §3.6 同步表述）。
- **一次性任务**：结束日期恰为首触发日（§1.2 推导）→ 只有一次 occurrence，第二时刻为空——呈现层如实显示「—」，不虚构。

### 2.3 刷新时机与降级

- **改完即见（VU17）**：`CronSchedule` 每次合法变更 → 前端 debounce（数百毫秒级，实施可调）调用 `cron.preview`——不等到提交。请求带序号丢弃（沿用 CronPanel 既有 loadSeq 惯例），过期响应不覆盖新值。
- **失败降级**：RPC 失败/表达式无效 → 三时刻区显示「—」+ 不阻断表单（求值是显示投影不是提交前置；服务端 `create` 的既有校验族才是权威拒绝——与既有 `cronDraftToInput` 的「honest-minimum fold」注释同口径）。
- **jitter 声明（VU16）**：三时刻与可读描述共享日程区下方的一行轻量说明（「理想时刻，实际投递有防拥塞抖动」，与列表页既有标注同款 i18n key 口径）——纯前端文案，无机制。

### 2.4 结束日期校验（SDR-110 / VU12）

「结束日期早于首次触发日 = 非法」的**判定输入是后端解出的首时刻**：表单在 preview 返回 t0 后，将 `ends.date`（本地日）与 t0（换算到任务时区）比较——早于 → 就地报错；恰等于 → 预览句式切换为「将于 X 日 X 时执行一次后结束」。比较用后端时刻（时区日历真值在后端），本地只做格式化——不在前端重算 occurrence。

## 3. 表单信息架构：三区块与环境五项（SDR-107/109 / 表单需求 §2）

### 3.1 容器与拆分

`CronCreateForm.vue`（新组件，挂 CronPanel 既有 `showForm` 位置与提交通道）内分三区块：

| 区块 | 内容 | 承载 |
|---|---|---|
| 一 · 任务是什么 | 标题（缺省「未命名定时任务」）、指示 | 表单本体（`ui/Input.vue` + `ui/Textarea.vue`） |
| 二 · 什么时候跑 | SchedulePicker（§1）、可读预览、时区、三时刻、jitter 说明 | §1/§2 机制 |
| 三 · 环境五项 | 工作区/权限模式/模型/思考深度/工作目录 | §3.2 复用清单 |

「循环执行（Recurring）」不是独立控件——由 `CronSchedule` 的档位与 custom 的 `ends` 结构表达（表单需求 §2.2：六档皆循环、一次性经结束日期表达）；提交时 `compileSchedule` 产出的语义映射为 wire 的 `recurring`（custom+ends.on 且首触发日恰为结束日 → `recurring: false`，否则 `true`——该推导对 cron 与 interval 两种日程类型同构，SDR-113）。

### 3.2 环境五项：复用清单（零重造）

| 项 | 复用对象 | 说明 |
|---|---|---|
| 模型 | `settings/ModelPicker.vue`（既有选择器）+ 模型目录数据源（面板既有 models 列表加载） | props `models/current`、emit `select`——表单直接挂载，选择结果写入 draft |
| 思考深度 | `lib/modelThinking.ts` 既有纯函数族：`modelThinkingAvailability`（VU11 的判定输入）、`segmentsFor`、`defaultThinkingLevelFor`、`effectiveThinkingLevel` | 选择器 UI 复用 Composer 的 segments 形态；「不支持则如实禁用」= `availability === 'unsupported'` 时渲染禁用态而非隐藏（R5） |
| 权限模式 | Session 面的模式名与选择器形态（Composer 的 PERM_MODES 三档：manual/yolo/auto 命名与文案）+ 第四档「只读观察」（自带说明，不映射 Session 名——§3.3/ADR-018 修订） | **呈现层**用 Session 模式名（三档）；映射到 D-M2 三档经后端常量（§3.3/ADR-018），表单不自带映射表 |
| 工作区 | CronPanel 既有 `defaultWorkspaceId`（聚合视图首个工作区） | 缺省 = 当前工作区 |
| 工作目录 | 文本输入（相对工作区根），缺省空 = 工作区默认目录 | 无既有选择器——工作目录选择在 Session 创建面亦非结构化选择器，表单对齐为文本项 + 占位符提示默认值 |

### 3.3 Session 模式名 → 运行姿态映射的单一真值（ADR-018）

映射（选择器四选项 → D-M2 三档）**放后端常量，单一真值**：

```text
// mirri-core（cron 模块常量，与 PermissionPosture 同域）
MODE_TO_POSTURE = {
  'manual':   AskDeny,
  'auto':     Auto,
  'yolo':     Auto,
  'readonly': ReadOnly,   // M1 修订：不映射 Session 名——Session 无此档，直接以「只读观察」本名
                         // 暴露为第四选项（带用途说明），使 D-M2 三档全部可达
}
```

- **理由**：a) 执法真值（RunSpawner 的 posture 消费）在后端——若前端持映射表、后端持执法档，两表必然漂移，「呈现名」与「实际执法」错位即 VU15 违背；b) 提议通道（ScheduledTaskPropose（原 CronPropose））与未来其他创建入口复用同一映射；c) wire 上传递的恒为后端档位（`permissionPosture`，既有 wire 字段），前端的 Session 名只是选择器标签。
- **四选项形态（M1）**：选择器 = Session 三档（manual/yolo/auto，零学习成本）+ 「只读观察」第四档（Session 没有的档，定时运行专用，带说明文案）——硬把 ReadOnly 映射到某个 Session 名会制造「Session 里有这档」的错觉，直接暴露本名反而无歧义。
- **yolo→Auto 的语义注记**：Session 的 yolo（全自动放行）在无人值守执行语境下等价于「自动执行」档（D-M2 无 yolo 档——cron 会话不存在「有人在场替用户点确认」的场景）；manual→AskDeny（问询一律拒，V16 默认口径）。该对齐本身记入 ADR-018。
- **否决备选**：前端持映射表（wire 传 Session 名、后端反解）。否决理由：wire 多一层间接（后端需理解 Session 命名空间），且任何第二消费方（TUI、提议审批面）都要各自抄一份映射——真值分裂。

### 3.4 校验与提交（VU9/VU12/VU13）

- 就地校验（前端、纯函数、§6 测试）：指示为空（沿用既有 submittable 判定）、custom 频率数字非法、结束日期早于首触发日（§2.4，以后端 t0 为基准）。错误信息就地呈现，提交按钮 gated。
- 提交折叠沿用 `cronDraftToInput` 的既有惯例：**空缺省项 wire 上缺省键**（永不显式 null/0——`apps/mirri-web/src/lib/cronManagement.ts` 的条件展开惯例，服务端按缺省解释），标题空 → 前端折叠为缺省串「未命名定时任务」入库（VU9：缺省在入库前定格，非列表渲染时兜底）。
- 提交成功即建、静默刷新列表（无审批，D.0c）——沿用 CronPanel 既有 submitForm 流。

## 4. 后端字段扩展与透传链（SDR-109 / 表单需求 §3.5）

### 4.1 `CronTask` / `cron.add` / wire 扩展（全缺省可读）

主文档 §1 数据模型的追加段（与既有可缺省惯例同型——旧记录无字段按缺省解释，无迁移）：

```text
CronTask {
  ...(既有 + 主文档 §1 阶段一/二字段)
  title: String,                    // 缺省「未命名定时任务」（服务端兜底，wire 缺键同缺省）
  model: Option<String>,            // 缺省 None = 服务器默认模型（解析点见 §4.3）
  effort: Option<ThinkingEffort>,   // 缺省 None = 模型目录默认档
  workdir: Option<String>,          // 缺省 None = 工作区默认目录
  ends_at: Option<Date>,            // 缺省 None = 永不结束（B1，wire 键 endsAt——全缺省可读）
  interval: Option<IntervalSpec>,    // 缺省 None = cron 表达式承载日程；Some = 流逝间隔任务（SDR-113）
                                     // IntervalSpec { every: u32, unit: 'day'|'week'|'month',
                                     //               time: ClockTime, anchor_date: Date }
                                     // 与 cron 字段互斥：Some 时 cron 字段空置（服务端拒绝两者并存）
}
// wire：cron.add 参数与 TaskView 各加同形可选键；TaskView 的三时刻投影见 §2.2
```

- **ends_at 的调度执法（B1）**：结束日期不是前端展示约定，是存储字段 + 调度停止条件——`WorkspaceCronStore` / `CronManager` 的 `check_due` 判定扩展：计划时刻所属**本地日 > ends_at**（`ends_at` 当日 = 最后可触发日，含当日口径与 SDR-110 对齐）后不再产生 fire；**最后一次触发后移除任务**（该 fire 携带「到期」收尾标记，与主文档 §5 `max_fires` 计满收尾同族判定形态——复用「一次性首触发自删 / 计满收尾」同一参数化收尾路径，收尾原因枚举追加「到期」）。TTL 计时不受影响（ends_at 收尾先于 TTL 达到时以先到者为准）。
- **interval 的调度求值（ADR-016 修订 / SDR-113）**：`check_due` 对 `interval: Some` 的任务不走 cron 表达式分支，走求值内核的**间隔分支**——从 `anchor_date` 起按 `every × unit` 步进（日历加法，按任务时区解析 `time`；与 §2.2 `preview_fires` 的 interval 变体同一实现，两处消费一个内核）。`ends_at` 的收尾判定对 cron 与 interval 同构适用。
- **服务端校验兜底（B1/M3，双端校验——前端即时 + 后端权威）**：`CronService.create` 做权威校验：a) `ends_at` 早于首次可能触发日（按任务时区解出的首个计划时刻——cron 与 interval 同构解出）→ 拒绝创建，理由与表单 §3.5 口径同文案；b) `cron` 与 `interval` 同请求并存 → 互斥拒绝（SDR-113）。前端就地校验仍是第一道（体验），服务端拒绝是权威（绕过表单直调 RPC 不产生「结束日期已过仍在跑」或「双日程」的非法任务）。
- `permissionPosture`/`concurrency`/`timezone` 等 wire 字段**沿用不动**（阶段二既有——wire 参数的空缺省键容忍惯例延续到新键）。
- 提议通道（ScheduledTaskPropose（原 CronPropose），主文档 §9.3）的草案字段是否同步扩展 title/model/effort/workdir：**本批不扩**——表单需求 §0 明确排除提议-审批面；环境扩展进入提议草案属该面的后续深化，登记为开放项（§8 O-1），不阻塞本文。

### 4.2 `CronService.create` 签名扩展

主文档 §2 的 `create` 参数追加 `opts { title, model, effort, workdir, ends_at, interval }`（缺省 None 走 §4.1 解释）；校验族沿用（环境字段不新增拒绝条件——模型别名的存在性解析延迟到运行时，见 §4.3 的失败诚实口径）；新增权威校验两支（§4.1：ends_at 早于首触发日拒绝、cron 与 interval 并存拒绝——环境字段免校验，日程字段必校）。

### 4.3 RunSpawner 的环境消费点（字段透传链）

RunSpawner（主文档 §8）步骤 2-3 的扩展——现状执行会话的模型取服务器默认、工作目录未指定（阶段二批 4b 已落地的 spawner 现状：`rust/crates/mirri-embedded/src/embedded_server/cron_run_spawner.rs` 以 `config.default_model` 构造 main agent，`work_dir` 恒 None——先于本批的阶段二实现，非 context 快照内事实）。透传链：

```text
spawn(fire) {
  ...(既有步骤 1 并发检查)
  2. session.create（形态=无人值守执行，metadata.work_dir = task.workdir）  // 既有格
  3. main agent 构造：
       model_id   = task.model ?? 服务器默认模型        // §4.3.1
       thinking   = task.effort ?? 模型目录默认档        // §4.3.1
       permission  = MODE_TO_POSTURE 的四键消费（readonly→ReadOnly 新增，其余不动）
  ...(既有步骤 4-5)
}
```

**4.3.1 model/effort 的解析与失败口径**：`task.model` 存**模型别名**（与 `session.create` 的 `model_id` 同一格——两者本就是同一别名空间，session.create 参数面已收 `model_id`/`provider_type`）；`task.effort` 存 `ThinkingEffort`——**通道已存在**：`session.create` 参数面已有 `thinking_effort`（wire key `thinkingEffort`），且执行会话的 main agent 构造点（spawner 的 `AgentScopeConfig`）即 `session.create` 同族结构，`thinking_effort` 是其既有字段（`rust/crates/mirri-core/src/scope/config.rs`）。故 effort **无需新后端通道**——透传 = spawner 构造 agent 时把 `task.effort` 填进既有 `AgentScopeConfig.thinking_effort` 格。
**失败诚实口径（VU14「选了 A 跑了 B」的守门）**：运行时模型别名解析失败（模型已删/目录变动）→ 该次运行 RunRecord 记 Failed（skip_reason 指明「任务模型 X 不可用」），**不静默回退服务器默认**——回退即「选了 A 跑了 B」；用户从运行记录看到失败与原因，修任务（编辑面属后续批次）或重建。该口径与既有「创建失败即 run Failed，不留 wedged Running」的收敛纪律同型。

**ADR-019（字段透传链；2026-11-01 评审修订——B1/M3 补 ends_at/interval 与权威校验）**：环境五项与日程扩展字段经 `cron.add` 可选键 → `CronTask` 可缺省字段 → 运行时消费的单链。修订补充：`ends_at`（Option<Date>，wire 键 `endsAt`）与 `interval`（Option<IntervalSpec>）同为该链成员——`ends_at` 的消费点在调度器 check_due（含当日边界收尾，与 max_fires 计满收尾同族判定形态，收尾原因枚举追加「到期」）；`interval` 的消费点在求值内核间隔分支（与 preview_fires 同一实现）。服务端 create 权威校验：ends_at 早于首触发日拒绝、cron 与 interval 并存拒绝（双端校的后端权威侧）。否决备选：a) 任务只存环境预设名（如 profile），运行时再解析——引入第二间接层且预设语义归 agent-profile 域，跨界；b) 执行会话创建时从「创建者会话」继承环境——定时任务到点新建会话，彼时无创建者会话（表单需求 SDR-109 已否决同款）；c)（修订补）ends_at 仅前端持有、提交时折算进 cron/一次性参数——结束日期是**存储字段 + 调度执法**（B1），非前端展示约定；绕过表单直调 RPC 时无执法即成僵尸任务。选择直存直传的理由：字段即承诺（任务携带什么就跑什么、跑到何时就停到何时），缺省解释集中在单点。

## 5. 与主文档机制的关系（本文不重定义清单）

| 主文档机制 | 本文的使用方式 |
|---|---|
| cron 表达式语义（求值/时区/jitter/coalesce） | 只消费不重定义（subagent 域 SDR-005/006 裁决沿用，主文档 §5 引用）；preview 属显示投影非调度行为 |
| CronService.create / 校验族 | 参数扩展（§4.2），语义单点不变 |
| RunSpawner 五步管线 | 步骤 2-3 的字段消费扩展（§4.3），管线结构不动 |
| 无人值守三档（D-M2）与 ADR-015 工具类别 | 映射真值坐落后端常量（§3.3/ADR-018），档位与执法不动 |
| 提议通道（ADR-013/014） | 不触碰；草案字段扩展登记开放项（§8 O-1） |
| MIRRICODE_DISABLE_CRON（ADR-006） | 创建拒绝沿用；preview 不拦截（显示投影） |
| TaskView / 列表聚合 | 追加三时刻投影（§2.2），版式归阶段二 Epic C/E |

## 6. 测试策略（设计级）

- **`cronSchedule.ts` 纯函数**：每档 (schedule, 表达式, 描述) 三元组表驱动；档位切换重置；ends 边界三态（早于/恰等于/晚于首触发日 → 非法/一次性句式/正常句式）；raw-cron 直通。命名 Given-When-Then。
- **后端 `preview_fires`**：与 `compute_next_fire` 一致性（对同一表达式连续 preview 相邻两时刻 = 单时刻步进）；interval 变体（every×unit 步进、anchor 语义、DST 日历加法——`*/2` 伪表达式不等价「每 2 天」的反例用例）；不可用表达式/时区 → 空 Vec；DST 边界跨日（核心库既有测试族追加用例）。
- **RunSpawner 透传**：任务带 model/effort/workdir 时执行会话的 agent 配置与 metadata 如实携带（VU14 机制面）；模型解析失败 → RunRecord Failed 且 skip_reason 含模型名。
- **映射常量**：MODE_TO_POSTURE 四键全覆盖（无选择器名落到未知档；readonly 档断言映射到 ReadOnly）。
- 前端组件测试遵循「不新增测试文件优先加既有文件」惯例，落在 cron 面板既有测试处。

## 7. 机制与验收映射（VU1-18 → 本文机制）

| # | 验收（表单需求 §5） | 承载机制（本文） |
|---|---|---|
| VU1 | 三区块齐备、选择器同 Session 面 | §3.1 CronCreateForm 三区块 + §3.2 复用清单（ModelPicker/modelThinking/PERM_MODES 形态） |
| VU2 | 空态仅添加按钮、六档下拉、无表达式输入框 | §1.1 SchedulePicker 空态/六档枚举；主路径无表达式输入（§1.5 高级折叠区仅 Custom 内） |
| VU3 | 每小时 :03 预览与实际一致 | §1.3 compileSchedule 直译 + §1.4 同源描述（单次编译双消费） |
| VU4 | 各档预览如实重述、实际一致 | §1.4 describeSchedule 档位直述 + §6 三元组测试；提交走既有求值通道（显示=实际的结构保证） |
| VU5 | Custom 弹窗（流逝间隔+结束方式+取消确认/含当日边界） | §1.1 Custom 弹窗（Dialog 基建）+ §1.2 interval/ends 结构（SDR-113）+ §2.4 首时刻比较（「最后运行在 X 日（含当日）」句式） |
| VU6 | 移除按钮清空回空态 | §1.1 垃圾桶清空（schedule → null 即空态重置） |
| VU7 | 预览与实际触发逐字对应（R4） | §1.3/§1.4 单源编译 + 核心库唯一求值（ADR-016/017）；jitter 边界见 VU16 |
| VU8 | 时区缺省可见可改、改后重述 | 时区为 §1.4 describe 参数（预览前缀）+ 点击改值 → 描述与三时刻随 §2 preview 重述；wire 沿用既有 timezone 键 |
| VU9 | 空标题入库缺省名；标签用「指示」 | §3.4 缺省串折叠入库 + §4.1 title 字段（服务端兜底同缺省）；标签为表单文案项 |
| VU10 | 环境五项缺省 = Session 缺省 | §3.2 各项缺省来源（默认模型/目录默认档/工作区默认目录/AskDeny）+ §4.1 wire 缺键 = 服务端缺省解释（与 session.create 同缺省口径） |
| VU11 | 不支持思考深度的模型如实禁用 | §3.2 `modelThinkingAvailability` === 'unsupported' → 禁用态（R5，非隐藏） |
| VU12 | 非法项就地报错；结束日恰为首日 → 一次性句式 | §3.4 前端纯校验 + §2.4 后端 t0 基准比较；一次性 = §1.2 推导属性（无第三态） |
| VU13 | 提交即建、列表如实呈现 | 沿用 CronPanel submitForm + 既有 list 通道；§4.1 TaskView 扩展 title/model/effort/workdir 如实呈现 |
| VU14 | 执行会话按所选环境运行（选 A 不跑 B） | §4.3 透传链（work_dir/model/effort 直达 agent 构造）+ 解析失败记 Failed 不静默回退 |
| VU15 | 四选项（Session 三档 + 只读观察）；执法为三档之一；默认问询一律拒 | §3.2 呈现层复用 + §3.3 MODE_TO_POSTURE 后端常量（ADR-018 修订：readonly→ReadOnly）；默认 AskDeny 沿用 |
| VU16 | 预览为计划时刻 + 一行轻量 jitter 说明 | §2.3 jitter 文案（前端纯文案）+ preview 为 pre-jitter 理想值（核心库既有口径，主文档引用 SDR-111） |
| VU17 | 改完即见三时刻、无需提交 | §2.3 debounce + preview RPC + 序号丢弃；上次 =「尚未运行」诚实恒显 |
| VU18 | 常驻态三时刻、无历史如实显示 | §2.2 常驻态投影（preview_fires + 运行记录投影），与表单态同函数同口径 |

**映射自查**：VU1-18 全部落位，无遗漏、无双 VU 共享同一机制的歧义（VU3/VU4/VU7 共用「单源编译」但验收面不同——VU7 是原则验收，VU3/VU4 是档位用例）；每个 VU 的机制至少一个可测点（§6）。

## 8. 开放项

- **O-1 提议草案字段扩展**：ScheduledTaskPropose（原 CronPropose）草案是否同步携带 title/model/effort/workdir（表单需求 §0 排除提议面；§4.1 登记待该面深化时裁决）。
- **O-2 常驻态投影的 wire 形态**：`cron.list` 视图直接带 `nextTwoFires`，还是前端以 `nextFireAt` + `cron.preview` 补齐（§2.2 两案口径等价、成本不同——列表页多一次 RPC vs 视图字段膨胀；实施时按列表刷新频率定）。
- **O-3 编辑面**：表单需求明确排除；本文字段扩展（§4.1）天然为编辑面预留同构形态（draft 结构可直接反向初始化——CronSchedule 从表达式重建仅在六档可表达子集内成立，raw/无法归档表达式回退 raw 档）。
- **O-4 模型侧（CronCreate 工具面）不扩 max_fires/ends_at**：本批是用户直建路径的 UI 落地，`ends_at`/interval 只进入 RPC 创建面；模型侧的预算门槛字段（max_fires）已存在，`ends_at` 对模型侧的暴露属另一裁决（涉及模型代建的任务自限语义扩展），待提议/工具面深化时定，不扩本批 scope。

## 9. 对主文档的回写清单

| 主文档位置 | 回写内容 |
|---|---|
| §1 数据模型 | 追加 title/model/effort/workdir/ends_at/interval 字段段（本文 §4.1） |
| §2 CronService | create 参数 opts 扩展 + 权威校验两支（本文 §4.2/§4.1）；cron.preview RPC 面（本文 §2.2） |
| §8 RunSpawner | 步骤 2-3 环境消费点 + 解析失败口径（本文 §4.3） |
| §9 三档映射 | MODE_TO_POSTURE 常量注记（本文 §3.3，含 readonly→ReadOnly 修订） |
| §10 映射表 | 追加「表单需求 VU1-18 → 表单专项文档」行 |
| §11 ADR 索引 | ADR-016~019 四条（本文 §10 全文收录，主文档可只留索引行） |

## 10. ADR 记录（本文档）

- **ADR-016（2026-11-01；同日评审修订——SDR-113）**：结构化日程的中间表示 = 判别联合 `CronSchedule`，编译为唯一消费点；**interval 为独立变体、不编译为 cron 表达式**。背景：六档 + Custom 需要一个表单状态模型；直接拼 cron 字符串会让「档位切换重置」「一次性判定（SDR-110）」「描述与所选一致（R4）」三件事都退化为 UI 约定与字符串反解析。修订背景：cron 5 字段的 dom/dow 是字段枚举语义，表达不了「每 N 天」的流逝间隔——issue #181 用户原文点名要该类规则，缩窄 Custom 违背用户输入。决策：draft 持结构化判别联合（六档独立；custom 含 field / interval / raw 三模式与 ends 结构）；`compileSchedule` 纯函数**分流**——cron 档位产表达式、interval 档产结构化 `IntervalSpec`（提交值非表达式）；任务模型上 `cron` 与 `interval` 互斥二选一（服务端拒绝并存）；求值内核加间隔分支（从 anchor 按 every×unit 步进，日历加法），`preview_fires` 与 `check_due` 同一实现两处消费；`describeSchedule` 消费同一结构生成描述——「所选 → 提交值 → 描述」单源；raw 仅 field 模式可用（间隔不可为表达式表达，如实提示边界）。被否决备选：a) cron 字符串为 draft（三件事均需字符串层反推，同信息双表示）；b) 各档独立小表单组件各持各的提交逻辑（校验与一次性判定无单点，漂移起点）；c)（修订补）缩窄 Custom 到 cron 可表达场景（直接砍掉 issue #181 点名规则）；d)（修订补）间隔编译为「最接近的 cron」伪表达（语义失真，违反 R4）。影响范围：§1、VU2-VU7/VU12。
- **ADR-017（2026-11-01；同日评审修订——M2）**：三时刻预览的计算位置 = 核心库后端，扩展 `next_fire_for` 为 `preview_fires` + 新只读 RPC `cron.preview`；**不做 last_occurrence 回溯**。背景：cron 求值（时区/DST/occurrence 步进）在核心库是唯一实现（`rust/crates/mirri-core/src/cron.rs`）；前端任何形式的重复实现（自写或引库）都与「显示=实际」（R4）冲突——两实现必然在时区数据版本与边界规则上漂移。修订背景：原设计的 `last_occurrence_before` 回看支**无消费点**——表单态恒「尚未运行」（无历史可显），常驻态用的是 `last_fired` 真值（实际注入时刻），回溯推算「假如之前按日程跑会是什么时刻」没有任何呈现面消费它。决策：核心库新增 `preview_fires`（同一 `compute_next_fire` 内核、只读、无副作用；入参为 `ScheduleSpec = Cron | Interval`——interval 走间隔分支，与 check_due 同一实现），wire 面为独立 `cron.preview` RPC（不挂在 `cron.add` 上——预览发生在提交前，无任务可挂靠）；表单 debounce 调用、常驻态复用同一投影；「上次运行」表单态恒为「尚未运行」（无历史不倒填）、常驻态用运行记录投影（最近一次非跳过运行的开始时刻；**不用调度游标**——游标在到期检测时即推进、与运行是否被跳过无关，承载显示会把跳过伪装成运行过；游标专职调度基准，两真值各司其职）。被否决备选：a) 前端本地求值（漂移即 R4 违背，且新增依赖）；b) 复用 `next_fire_for` 现状单时刻签名而不扩展（三时刻需连续两时刻，逐次以 anchor 逼近会在时区换日边界上算错一次——求值器的 occurrence 步进必须同一次时区解析内完成）；c)（修订补）保留 `last_occurrence` 回溯支（无消费点的推测性设计，且与常驻态「实际注入」口径分裂——回溯解出的是计划时刻而非实际运行时刻，两口径并存即歧义）。影响范围：§2、VU16-VU18。
- **ADR-018（2026-11-01；同日评审修订——M1）**：Mode 选择器 → D-M2 运行姿态的映射真值 = 后端核心库常量 `MODE_TO_POSTURE`。背景：表单呈现层用 Session 模式名（SDR-107 零学习成本），执法在后端三档（D-M2）——映射表放前端则「呈现名」与「执法档」两处维护，第二消费方（TUI/提议面）各抄一份必然分裂，错位即 VU15 违背。修订背景：初版映射只有 manual→AskDeny / yolo、auto→Auto 三键——**ReadOnly 档在 UI 上不可达**（Session 无对应名可映射），D-M2 三档之二悬空。决策：映射为 mirri-core 常量（manual→AskDeny、yolo→Auto、auto→Auto——yolo 的「有人在场」语义在无人值守执行语境不存在，对齐 Auto 档；**readonly→ReadOnly**——不映射 Session 名，选择器为**四选项**：Session 三档零成本复用 + 「只读观察」以本名暴露为第四档、带用途说明，使三档执法全部可达）；wire 恒传后端档位（既有 `permissionPosture` 键），前端的 Session 名只是选择器标签。被否决备选：a) 前端持映射、wire 传 Session 名（后端需理解 Session 命名空间 + 多消费方各抄一份）；b) wire 新增 Session 名键与后端档位键并存（双键即双真值）；c)（修订补）保持三键、放弃 ReadOnly 档的 UI 可达性（阶段二已裁决三档，UI 上选不到即功能悬空）；d)（修订补）把 ReadOnly 硬映射到某 Session 名（Session 无此档——制造「Session 有这档」的错觉，比直陈本名更误导）。影响范围：§3.2/§3.3、VU15。
- **ADR-019（2026-11-01；同日评审修订——B1/M3）**：环境五项与日程扩展字段的透传链 = `cron.add` 可选键 → `CronTask` 可缺省字段 → 运行时消费的单链；解析失败诚实记 Failed 不静默回退。背景：SDR-109「任务携带环境并在运行时如实生效」需要一条无中转的存储-消费链；现状 RunSpawner 恒用服务器默认模型、无工作目录（`rust/crates/mirri-embedded/src/embedded_server/cron_run_spawner.rs`——阶段二批 4b 落地现状，先于 context 快照）。决策：model 存模型别名（与 `session.create.model_id` 同一格）、effort 存 ThinkingEffort（填 spawner 既有 `AgentScopeConfig.thinking_effort` 格——通道已存在，无新后端机制）、workdir 填 `SessionMetadata.work_dir`（既有格）；全部缺省可读（wire 缺键 = 服务端缺省，旧记录零迁移）；运行时模型解析失败 → RunRecord Failed + skip_reason 含模型名，不回退默认（回退即「选 A 跑 B」，VU14 违背）。修订补充（B1/M3）：`ends_at`（Option<Date>）与 `interval`（Option<IntervalSpec>）入链——ends_at 消费点在 check_due（含当日边界收尾，与 max_fires 计满同族，收尾枚举追加「到期」），interval 消费点在求值内核间隔分支；服务端 create 权威校验 ends_at 早于首触发日拒绝、cron 与 interval 并存拒绝。被否决备选：a) 任务存环境预设名运行时解析（第二间接层 + 跨 agent-profile 域）；b) 从创建者会话继承（定时任务到点新建会话，创建者已不在场——表单需求 SDR-109 已否决）；c) 解析失败静默回退服务器默认（呈现撒谎）；d)（修订补）ends_at 仅前端持有、提交时折算——结束日期是存储字段 + 调度执法（B1），非前端展示约定。影响范围：§4、VU10/VU13/VU14。
