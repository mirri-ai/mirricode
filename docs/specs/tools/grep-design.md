# Grep 工具设计文档

> 状态：v1.1 已裁决并入（2026-10-22）· 决策标号 G-ADR-1~4 已并入 [tool-calling-requirements.md](../tool-calling-requirements.md)（US4.4 增强版）与 [tool-calling-architecture.md](../tool-calling-architecture.md)（ADR-5）；实现已落地（块状输出、多路径、exclude_pattern、字节预算、rg 缓存回退），描述文案（§3.1）属 Rollout 阶段 2 待单独落地
> 数据依据：[tool-calling-bash-competition-context.md](../tool-calling-bash-competition-context.md)（159 个 v3 会话、41,755 次 Bash、5,505 次 Grep 的实测调研）
> 实现基线：`rust/crates/mirri-core/src/tools/grep.rs`（工具层）+ `rust/crates/mirri-os/src/search.rs`（搜索层）；枚举值 `files`/`count` 与描述强化已由 commit `5ec0ba4b1`（2026-10-22）落地——本设计在其上叠加能力增强

## 1. 目的与需求（PM 可读）

### 1.1 这个工具为什么存在

Grep 是 agent 在陌生代码库里"找到内容在哪里"的第一入口：不知道符号定义在哪、不知道谁调用了它、不知道某配置散落在哪些文件——这些问题的答案都靠内容搜索。它是 **Read（读已知文件）和 Glob（按名字找文件）之间的拼图**：Read 需要先知道路径，Glob 只能按文件名找，Grep 按**内容**找。

### 1.2 背景与痛点（来自真实会话数据）

我们分析了 159 个真实工作会话（rust 引擎、2026-10-17~22）：

| 事实 | 数字 | 业务含义 |
|------|------|---------|
| agent 用 Bash 跑 `rg`/`grep` 的次数 | 22,347 | 搜索需求本身**极旺盛** |
| 其中用 Grep 工具完成的 | 5,505（13%） | 87% 的搜索绕过了我们的工具 |
| 搜索后 3 步内跟"读文件片段" | 95% | 搜索和阅读是**一个动作**，不是两个 |
| Grep 工具单次输出大小 | Bash 搜索的 4 倍 | 我们的工具反而更费 token |
| Bash 搜索命令带管道 `\|` | 93% | 模型在主动做"裁剪、过滤、组合" |

**一句话痛点**：agent 的真实检索单元是"**定位 → 顺带看清周边 → 只要我要的部分**"，一个 Bash 管道命令天然完成这个单元；Grep 工具目前只完成第一步（定位），第二、三步全缺，于是模型 87% 的时候直接用 Bash。

### 1.3 需求（本设计要满足什么）

- **R1**：一次 Grep 调用返回"可直接阅读的代码块"——命中行带行号上下文成块呈现，消灭"搜完还要再读一次"的链路。
- **R2**：支持一次搜索多个明确文件（"在这 3 个文件里找 X"）。
- **R3**：支持对结果做排除过滤（"找 X 但排除测试文件/排除含 Y 的行"）。
- **R4**：输出默认紧凑——模型不再需要"用完 Grep 再后悔输出太大"。
- **R5**：安全性不回退：敏感文件过滤、沙箱路径策略、输出上限照旧。

### 1.4 需求定义（US 条目，条件-结果式验收）

> 编号 `US-G*` 为本工具域内编号，批准后并入 `tool-calling-requirements.md` E4 域（替代 US4.4 的增强版）。每条验收 = 一个可直写的测试断言。工具的直接用户是 LLM（agent），User Story 主语统一为"LLM"；参数名、输出文案为契约原文保留英文，叙述一律中文。验收句式统一"当……时，则……"。

**US-G1 命中即代码块（content 模式块状输出）**
LLM 应当能够在 content 模式下一次拿到"命中行 + 周边代码块"，从而搜索一次即完成阅读，无需再调 Read/sed。
- 当 content 模式下相邻命中的上下文窗口重叠或相邻（无空行间隔）时，则合并为一个块，块头含 `path:startLine-endLine` 行号范围，命中行带 `← match` 标记
- 当单个命中且 `-C 3` 时，则块含命中行及其前后至多 3 行上下文，行号连续无空洞
- 当命中位于文件首行/末行时，则只输出文件内实际存在的行（不输出越界行号）
- 当 `-n: false` 时，则块内行不带行号前缀，但块头行号范围保留（定位能力不丢；实现注意：解析行号与显示行号需解耦，解析层始终保留真实行号供块头使用）
- 当 multiline 命中载荷内含换行时，则按多行命中渲染为块内连续行，行号按命中起始行计
- **计数口径**：本设计全文的"条"（entry）均指**命中数**（match），不含上下文行——现状 `parse_json_content` 把上下文行与命中同等计数，本设计修正为命中口径（G-ADR-4 附注）

**US-G2 多路径搜索**
LLM 应当能够在一次调用里指定多个明确文件或混合文件与目录，从而替代 `rg a.rs b.rs` 式的定向搜索。
- 当 `path` 为 2~32 个文件路径的数组时，则一次搜索覆盖全部指定文件，结果按文件分组（组序 = 调用数组的顺序，保证 offset 翻页可复现）
- 当 `path` 数组混入目录时，则目录按递归搜索语义处理，与文件混合返回
- 当 `path` 数组超过 32 项时，则参数校验拒绝，错误信息引导改用 `glob` 过滤
- 当 `path` 数组任一元素构成**词法逃逸**（escape，如 `..` 越界）时，则整体拒绝；当数组元素为工作区外的干净绝对路径（outside）时，则逐一进权限链裁决（N 条 access，任一被拒整体不执行）——与现行单路径语义对称（plan 2026-10-10-sandbox-path-permission D1）
- 当 `path` 数组与 `glob`（含 `/` 的模式）组合时，则取数组全部路径的公共祖先钉为 rg 的 cwd、argv 传相对该 cwd 的路径（沿用 F11 修复的 cwd-pinning 原理，消除"含 `/` 的 glob 匹配绝对路径而静默零命中"的回归）
- 当数组元素不存在时，则报错并指明该元素（不静默跳过——与"数组里有什么就搜什么"的显式契约一致）

**US-G3 结果排除过滤**
LLM 应当能够对命中行做二次排除，从而替代 `| rg -v` 管道，剔除测试/生成代码等噪音。
- 当 `exclude_pattern` 非空且某命中行匹配它时，则该命中被丢弃；过滤发生在**聚类之前**（按命中逐条丢弃后重新聚类，被滤命中独占的上下文行自然消失）
- 当 `exclude_pattern` 过滤掉全部命中时，则返回 `No non-sensitive matches found`——与"本来就没有命中"**同文案**（对齐现行空结果文案，不引入第二种空结果语义）
- 当 `exclude_pattern` 为非法正则时，则报错并指明该参数（不静默回退子串——排除是精确意图，静默回退会让 LLM 误信结果完整性）
- 当 `exclude_pattern` 与分页并存时，则过滤先于 offset/head_limit 应用（分页语义基于过滤后集合）
- 当 `exclude_pattern` 用于 `files`/`count` 模式时，则同样生效（语义：排除该文件内的命中——文件命中全被排除则文件不出现在结果）

**US-G4 输出量默认可控**
LLM 应当能够不传参数就得到紧凑结果，从而默认一次调用不爆上下文。
- 当 content 模式且未传 `head_limit` 时，则默认上限 30 个**命中**（原 250，修订依据见 G-ADR-4）；`0` 仍表示不限
- 当 content 模式输出累计字节超过 4 KB 软预算（统计范围：块内容 + 块头，不含尾部提示行）时，则在最后一个完整块边界截断（单块本身超预算仍完整输出——块是最小原子单位），尾部提示 `Results truncated at N of T entries (byte budget reached). Use offset=... to see more.`
- 当 `head_limit: 0`（不限）时，则 4 KB 软预算**不适用**（显式不限即显式要全量），由 10 MiB 硬上限兜底，截断提示沿用现行 `[stdout truncated at 10485760 bytes; ...]` 文案
- 当输出触发条数上限（显式或默认）时，则尾部提示 `Results truncated to N entries (total: T). Use offset=... to see more.`——与字节预算的提示为两个模板，触发原因可区分

**US-G5 安全基线不回退（方向对称）**
- 当命中文件包含敏感文件（`.env` 等）时，则任意模式照旧过滤（含 content 模式的块——敏感文件的块整块不出现）
- 当搜索路径构成词法逃逸时，则沿用现有路径守卫拒绝文案，无新绕行面；工作区外绝对路径沿权限链裁决（语义同 US-G2）
- 当传非法参数形态（如不存在的枚举值）时，则参数校验拒绝——本设计**不引入任何静默兼容**（枚举值 `content`/`files`/`count` 为现行已落地形态，见 G-ADR-1）

**US-G6 界面呈现（消费端参考，非引擎对齐项）**
- 当 content 模式输出为块状格式时，则 glance（折叠态摘要）从块头行提取路径（`path:start-end` 冒号前段），chip 计数 = 命中数（非块数、非行数）——具体取数规则由 TUI/web 消费端在实现时对齐，本条为方向声明

## 2. 目标与非目标

### 目标

- Grep 承接 Bash 搜索类命令中"可结构化"的部分：多文件定向搜索、带上下文阅读、排除过滤。
- 单次调用信息密度 ≥ 等效 Bash 管道命令（`rg -n -C 3 X file | head -20`）。

### 非目标（明确不做）

- **不做**通用管道/任意 shell 组合——那是 Bash 的领地，Grep 是结构化工具，安全与可预测性优先。
- **不做**跨仓库/多工作区聚合一级接口——多 `path` 指向同一批文件即可覆盖主场景；并行调用多个 Grep 已被系统提示鼓励。
- **不做**搜索结果语义排序（按相关性重排）——保持 mtime/出现序，可预期性优先。
- **不动**Bash 工具本身。

## 3. 模型可见接口（LLM 描述设计）

### 3.1 面向模型的 description（设计稿，实现时逐字对齐）

```
Search file contents using regular expressions.

Use Grep when the task is to find unknown content or unknown file locations. Do not use shell `grep` or `rg` directly; this tool applies workspace path policy, output limits, and sensitive-file filtering.

One call answers the whole question — combine these instead of a shell pipeline:
- Reading around a match: `output_mode: "content"` with `-C` returns the match with line-numbered context lines, grouped into blocks. This replaces `rg -n -C 3 <pattern> | head`.
- Narrowing to specific files: `path` accepts an array of file paths (`["a.rs","b.rs"]`) or one directory.
- Excluding noise: `exclude_pattern` drops matching lines (e.g. tests, generated code) — replaces `| rg -v`.
- Keeping output small: `head_limit` (default 30 in content mode, plus a byte budget) caps results; the result tells you the total so you can page with `offset`.

Write patterns in ripgrep regex syntax (braces are special: escape `\{` for a literal `{`).

Hidden files are searched by default; `include_ignored: true` also searches gitignored files. Sensitive files (`.env` etc.) are always skipped.
```

**设计意图**（G-ADR-2，详见 §5）：描述第一段就用"**One call answers the whole question**"直击调研发现的复合单元行为，随后**逐条把 Bash 管道习惯映射到 Grep 参数**。这是描述工程，不是参数堆砌。

### 3.2 参数契约（相对现状的变化标 🆕）

| name | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `pattern` | string | ✅ | — | 不变 |
| `path` | string **或 string[]** 🆕 | 可选 | 工作目录 | 数组时逐个校验沙箱边界；单文件直接搜该文件 |
| `glob` / `type` / `-i` / `-n` / `-A` / `-B` / `-C` / `multiline` / `include_ignored` / `offset` | — | — | — | 不变 |
| `exclude_pattern` | string 🆕 | 可选 | — | 对**命中行**做二次正则排除（ripgrep 语法）；命中行匹配它则丢弃（上下文行跟随其命中一起丢弃） |
| `head_limit` | integer | 可选 | content 模式 **30**（原 250）；其他模式 250 | content 模式默认收紧 + 4 KB 字节软预算（G-ADR-4 修订） |
| `output_mode` | enum | 可选 | `files`（现行） | `content` / `files` / `count`——现行已落地形态（`5ec0ba4b1`），本设计不改 |

### 3.3 输出契约

**content 模式（核心变化 🆕）——块状呈现**：

```
src/server/ws_rpc.rs:261-267
  261│ impl SessionEventChannel {
  262│     pub fn new() -> Self {
  264│         Self { journal: Vec::new() }   ← match
  266│     }
```

- 每个命中（或命中簇）一个块：头部 `path:startLine-endLine`，正文逐行 `行号│内容`，命中行尾部 `← match` 标记。
- 相邻命中（上下文窗口重叠）**合并为一个块**，避免重复行——这是 Bash `rg -C` 做不到结构化的关键改进。
- 块头部带行号范围，模型可直接引用 `path:261` 定位，后续 Edit/Read 无需再搜。

**其他模式不变**（files / count 形态保留）。

**分页提示（保留并强化）**：截断时尾部 `Results truncated to N entries (total: T). Use offset=... to see more.`

## 4. 内部设计（架构）

### 4.1 分层（现状沿用，改动点标 🆕）

```
mirri-core/tools/grep.rs      工具层：schema、参数解析、路径沙箱校验、
                              结果过滤（敏感文件）、输出渲染
   │                          🆕 path 数组解析 + escape/outside 逐路径裁决（N 条 access）
   │                          🆕 exclude_pattern 行级过滤（渲染前管线）
   │                          🆕 块状渲染（合并重叠上下文窗口）+ 字节预算
   ▼
mirri-os/search.rs            搜索层：SearchParams → rg argv → 子进程 → 解析
                              🆕 SearchParams.path → paths: Vec<PathBuf>（类型变更，见下）
                              🆕 SearchMatch 增 is_match 标记（SearchResult 结构不动）
   ▼
rg 子进程                     --json / -C / 排除 glob / 沙箱 path
```

**`SearchParams.path` 类型变更的连带面（评审核实，必须在 Rollout 披露）**：`path: PathBuf` → `paths: Vec<PathBuf>`（`resolve_execution` 层把单字符串归一为单元素数组，公共 API 单数糖保留）。受影响符号：`ContentSearch`/`DynContentSearch` 两个 trait 签名的消费面、`rg_invocation`（多路径分支重写）、`PathAccess::new`（多根敏感归一）、grep.rs 的 `display_path`（多根前缀裁剪）、`ExecutionPlan.accesses`（N 条 access）、mirri-os 两个测试文件的 `SearchParams` 字面量。

### 4.2 关键机制

**多路径（R2）**：rg 原生支持多路径 argv。关键分支：`rg_invocation` 现状在"glob 含 `/` 且 path 为目录"时把 cwd 钉在搜索根、传 `.` 作为搜索路径（F11 修复，测试 `should_run_rg_from_the_search_root_when_a_glob_filter_is_active` 锁定）。多路径时无单一根可钉——**公共祖先方案**：取全部路径的公共祖先为 cwd，argv 传相对该 cwd 的路径，`resolve_root` 回显按各路径分别归位（glob 无 `/` 时保持现状直拼）。沙箱校验逐路径执行：任一 escape 整体拒绝；outside 路径逐一生成 access 进权限链（与单路径语义对称，plan 2026-10-10-sandbox-path-permission D1）。

**管线顺序（评审 S1 披露）**：现状 offset/head_limit 在 `parse_json_content` 的解析循环内逐条施加（含上下文行同等计数）。本设计改为**后置管线**：解析全量收集 → exclude_pattern 过滤（按命中）→ 命中口径 offset/head_limit → 聚类 → 渲染。分页从解析循环迁出到后置 pass；全量收集的内存开销由既有 10 MiB stdout 上限兜底。**计数单位从"行条目"改为"命中数"**——`total_matches` 现状已只数 match（search.rs），head_limit/offset 改为对齐该口径。

**块状渲染（R1，G-ADR-3）**：rg `--json` 已输出 `match`/`context` 事件流。聚类放**工具层渲染前**（评审 S3 方案）：`SearchMatch` 增加 `is_match: bool` 字段（`SearchResult` 结构不动，files/count 模式与测试替底 `FixedSearch` 兼容），`parse_json_content` 停止把两者混排扁平化、保留事件序；grep.rs 按 path 分组 + 行号窗口聚类成块。**不依赖 rg 事件流的文件连续性**（rg 并行搜索不保证同文件事件相邻），仅以 path 为分组键。

**exclude_pattern（R3）**：rg 的 `-g` 是文件名 glob 不是行内容，不可用。正确做法：**结果层过滤**——解析后丢弃匹配 `exclude_pattern` 的命中（在聚类之前），再走分页。管线位置见上段。

**默认 head_limit 收紧 + 字节软预算（R4，G-ADR-4）**：content 模式默认 250 → **30 个命中**（口径变更见管线段）；4 KB 字节软预算在渲染循环逐块累计（统计范围：块内容 + 块头）。依据：Bash 搜索 65% 带 `| head -N`（p50=10、p90=40），实测 Grep 即便传 limit=30 输出仍是 Bash 的 ~3 倍（按条计数不限字节）。files/count 模式不变（现状输出已小）。

## 5. 决策记录（已裁决并入 tool-calling-architecture.md ADR-5；此处保留原始论证）

### G-ADR-1：output_mode 枚举值现状确认——`content`/`files`/`count` 已落地（`5ec0ba4b1`，2026-10-22）
- 本设计撰写期间（v1 草案）曾基于过时基线裁"不改名"；评审核实：早前计划 `plan/rust-builtin-tool-schema-naming.md` 的改名（`files_with_matches`→`files`、`count_matches`→`count`）、描述强化（"Pass exactly ONE of these string VALUES"）、`include_dirs` 移除**已由 commit `5ec0ba4b1` 全部落地**。
- 本 ADR 修订为**现状确认**：枚举值维持 `content`/`files`/`count`，无进一步改名；本设计的增量在其上叠加（块状输出、多路径、exclude_pattern、默认收紧、字节预算）。

### G-ADR-2：描述工程优先——把"管道习惯"显式映射为参数；实现指涉降级为方言指涉
备选：加更多参数（如 `sort`、`format`）。拒绝理由：参数越多选择负担越大；数据表明模型缺的是"知道怎么组合现有能力"，不是缺参数。描述里逐条给 `Bash 管道 → Grep 参数` 的映射。

**修订（2026-10-22 用户裁决）**：description 首句的 `(powered by ripgrep)` 移除——实现指涉有双刃效应：模型对 ripgrep 认知度高（方言提示价值），但把引擎名放进第一句会**强化"直接用 Bash rg 也一样"的联想**，与引导目标冲突。裁决取"说方言不说引擎"：全文只保留 `Write patterns in ripgrep regex syntax...` 这一句功能性方言提示（写 pattern 的准确率依赖），其余位置不再出现引擎名。

### G-ADR-3：content 模式块状输出（而非保持扁平行列表）
备选 A：保持扁平 `path:line:content`，仅靠 -A/-B/-C。拒绝：这正是现状，23% 用户用了上下文仍 95% 链式跟读——扁平列表缺"块"的视觉边界和行号范围头，模型仍需 Read 确认结构。
备选 B：返回 JSON 结构化结果。拒绝：工具输出是纯文本协议，JSON 增大 token。

### G-ADR-4：content 模式默认 head_limit 250→30，并增设字节软预算（2026-10-22 二次修订）
初版裁 250→50；二次实证（r8 调研：Bash 搜索 65% 带 `| head -N`，N 值 p10=3 / p50=10 / p90=40）后收紧为 **30**，并对齐模型真实心智加**字节维度**。
备选 A：保持 250 并靠模型自觉传小。拒绝：实测 58% 的 Grep 调用根本没传 `head_limit`，默认值就是模型拿到的值。
备选 B：默认 10（对齐 p50）。拒绝：p90=40，过紧会频繁触发"再查一次"，翻页成本反噬；30 覆盖 p90 且不浪费。
**字节软预算（block budget，4 KB）**：按条计数对长行无效——实测"传了 head_limit=30 的 Grep 调用"输出仍是 Bash head-10 的 ~3 倍，根因是 content 模式一条命中可能是长行，按条数不限字节。4 KB ≈ Bash 搜索 p99（5,921B）以内、均值（989B）的 4 倍，是"一次调用信息量"的合理上界；在**块边界**截断（不切半块），两条上限（条数 30 / 字节 4 KB）先到先生效。硬上限 10 MiB 不变（防 head_limit: 0 的滥用兜底）。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 块状输出比扁平列表更费 token（重复行号） | 行号用紧凑格式（`261│`）；合并簇消除重复上下文；head_limit 默认 30 + 4 KB 字节软预算双重对冲（G-ADR-4 修订）；上线后对比输出均值（Bash 搜索 989B 为标尺） |
| 字节软预算截断导致单块超预算本身（极长行） | 单块超 4 KB 时该块仍完整输出（块是最小原子单位），下一块起停止——预算控制"块数"不破坏"块完整性" |
| 多路径 argv 长度上限（极多文件） | schema 限制数组 maxItems 32；超出的引导用 glob |
| exclude_pattern 与 pattern 语义混淆（都作用在行上） | 参数描述明确"排除**命中行**"；错误示例给在描述里 |
| 迁移期模型不识块格式 | 格式自解释（行号+范围头），且 files/count 模式不受影响 |
| rg --json Context 事件在多文件混流时聚类错乱 | 聚类以 path+行号窗口为键，rg 事件流按文件连续（同文件 begin/end 包裹），实现时以 end 事件为界 flush |
| **rg 二进制分发兜底 rust 侧未对齐**（TS 有四级：PATH→vendor→缓存→自动下载；rust 仅 PATH，`mirri-os/src/search.rs:245`） | P1：本设计 Rollout 阶段 1 内附带（PATH 失败后查 `~/.mirri-code/bin/rg`，约十几行）；P2 完整下载兜底独立立项（已登记 [../rust-core-design/ROADMAP.md](../rust-core-design/ROADMAP.md) §4 工具/搜索基础设施） |

## 7. 成功标准（可测量）

以同源 v3 会话数据复测（对比基线 = 调研报告）：

1. Grep 调用次数提升 ≥ 2.5 倍（5,505 → 13,000+）
2. Bash 搜索类命令占比 54% → ≤ 35%
3. "搜索后 3 步内跟读"链式率 95% → ≤ 60%（块状输出直接可读）
4. Grep content 模式输出均值 ≤ 2,000B（对比 Bash 搜索 989B 的 2 倍以内）
5. Grep 工具错误率保持 0（不因新参数引入校验失败）

## 8. Rollout

1. **阶段 1（核心）**：多路径 + 块状输出 + exclude_pattern + head_limit 默认收紧——同一 PR，rust core 落地（grep.rs + search.rs），契约快照重生成，changeset `minor`（无破坏：新参数可选、默认值变化在描述中披露）。
2. **阶段 2（描述）**：§3.1 描述全文替换——单独 PR，便于 A/B 观察描述工程的效果。
3. **阶段 3（观察）**：上线 2 周后按 §7 复测，数据决定是否需要阶段 4（如 glob 语义增强、跨工作区 path 数组）。
