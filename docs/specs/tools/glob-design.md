# Glob 工具设计文档

> 状态：v1.1 已裁决并入（2026-10-22）· 决策标号 GL-ADR-1~4 已并入 [tool-calling-requirements.md](../tool-calling-requirements.md)（US4.5 增强版）与 [tool-calling-architecture.md](../tool-calling-architecture.md)（ADR-6）；实现已落地（filter/limit/计数头/全量收集 + 双硬上限），描述文案（§3.1）属 Rollout 阶段 2 待单独落地
> 数据依据：[tool-calling-bash-competition-context.md](../tool-calling-bash-competition-context.md)（Glob 332 次 vs Bash 41,755 次的实测调研）
> 实现基线：`rust/crates/mirri-core/src/tools/glob.rs`（工具层）+ `rust/crates/mirri-os/src/glob.rs`（搜索层，ignore/globset 实现）；`include_dirs` 移除已由 commit `5ec0ba4b1`（2026-10-22）落地——本设计在其上叠加能力增强

## 1. 目的与需求（PM 可读）

### 1.1 这个工具为什么存在

Glob 回答"**文件在哪里**"：按名字模式找文件——`**/*.rs` 找所有 Rust 源码、`test-*.json` 找某类产物、`*.test.ts` 找测试文件。它是 Grep（按内容找）和 Read（读已知路径）之间的另一半拼图。

### 1.2 背景与痛点（真实会话数据）

| 事实 | 数字 | 业务含义 |
|------|------|---------|
| Glob 工具调用 | **332 次** | 在 41,755 次 Bash 面前近乎消失（0.8%） |
| Bash 里 `rg --files \| rg -i 'x'` 式过滤 | 118 次 | 模型想要"列文件 **+ 过滤**"，Glob 只会列 |
| Bash `find`/`ls`（按名找文件） | ~960 次 | Glob 的领地被 Bash 吃掉 |
| Glob 结果 100 条截断且无参数可调 | 现状 | 大目录直接不可用，模型弃用 |

**一句话痛点**：模型对文件发现的真实需求是"**列出 → 按我的条件筛 → 控制量**"，Bash 一条 `rg --files dir | rg -i 'skill' | head -40` 完成全部三步；Glob 只有第一步，且 100 条截断不可调，于是在文件发现场景模型也整体转向 Bash。

Glob 的处境比 Grep 更糟（0.8% vs 13%），因为它的能力面更窄：连 `-A` 类上下文能力都没有，纯靠"列名"单打一。

### 1.3 需求

- **R1**：支持对匹配结果做**二次过滤**（子模式/正则，如"路径里含 skill 的"）。
- **R2**：结果量**可控**：上限参数化，默认从 100 提到合理值。
- **R3**：模型感知"最新优先"排序语义（mtime 降序是 Glob 独有价值——找"最近生成的文件"是高频场景，Bash 需要额外 `sort`）。
- **R4**：保持安全与性能基线（敏感过滤、ignore 规则、20s 超时、mtime 排序）。

### 1.4 需求定义（US 条目，条件-结果式验收）

> 编号 `US-GL*` 为本工具域内编号，批准后并入 `tool-calling-requirements.md` E4 域（替代 US4.5 的增强版）。每条验收 = 一个可直写的测试断言。工具的直接用户是 LLM（agent），User Story 主语统一为"LLM"；参数名、输出文案为契约原文保留英文，叙述一律中文。验收句式统一"当……时，则……"。

**US-GL1 路径二次过滤**
LLM 应当能够对匹配到的路径按子串/正则二次过滤，从而替代 `rg --files | rg -i 'x'` 管道。
- 当 `filter` 为纯文本（不含正则元字符）时，则按不区分大小写的子串匹配**显示路径**（搜索根在工作区内为相对路径，根外为绝对路径——匹配对象即输出形态）
- 当 `filter` 含正则元字符（触发集合显式定义为：`\` `+` `*` `?` `(` `)` `[` `]` `{` `}` `|` `^` `$`——**刻意不含 `.`**，因为路径中扩展名点极常见，把 `.` 算元字符会使几乎所有 filter 走正则分支）时，则按不区分大小写的正则匹配路径
- 当 `filter` 带 `!` 前缀（如 `"!test"`）时，则语义取反——只保留路径**不含**该模式的结果（`!` 前缀在子串与正则两种模式下均生效；要匹配字面 `!` 开头的子串，用正则 `^!`）
- 当 `filter` 含正则元字符但编译失败时，则**按子串处理**（回退而非报错——与 Grep `exclude_pattern` 的报错语义**刻意不同**：过滤是收窄意图，误伤面小；排除是精确意图，静默回退危险。此差异为裁决项，记 GL-ADR-1 修订附注）
- 当 `filter` 过滤掉全部匹配时，则只输出计数头 `0 matches (of M matching before filtering, 0 after filter) — most recent first`（**替代**旧空结果文案 `No matches found`——被滤光的调用恒有计数头，与"模式无匹配"区分；模式本身无匹配时输出 `No matches found` 保持现状）

**US-GL2 结果量控制与计数头**
LLM 应当能够控制返回条数并知道被截断了多少，从而不盲目翻页。
- 当未传 `limit` 时，则默认上限 200（原硬编码 100）；`0` = 不限
- 当匹配数超过 limit 时，则首行计数头 + 尾部截断提示 `[Results truncated at N of K — refine pattern or filter, or pass limit]`
- 当 `limit: 0`（不限）时，则受**新增的字节硬上限**约束（本设计新工作，见 §4.2——现状 `LocalGlob` 无任何输出上限，全量收集进内存，`limit: 0` 在大目录会打爆）：匹配数超 10,000 或渲染字节超 1 MiB 时截断，尾部提示 `[Results truncated — limit: 0 with large result set; pass a smaller limit or refine pattern]`
- 当恰好 N 条匹配（未超 limit）时，则计数头照常输出（计数头**恒在**，非仅截断时出现——LLM 每次都知道总量基线）
- **计数头模板（唯一字面，全文引用此处）**：传了 `filter` 时 `N matches (of M matching before filtering, K after filter) — most recent first`；未传 `filter` 时退化为 `N matches — most recent first`。M/K 的口径为**敏感文件过滤后**的计数（防止计数头泄漏"存在 N 个被滤敏感文件"的存在性信号），现行 `Filtered N sensitive file(s)` 消息行保留

**US-GL3 排序语义显性化**
LLM 应当能够确定结果按修改时间从新到旧排序，从而"找最新的 X"一次完成。
- 当多个匹配且 mtime 各异时，则严格 mtime 降序（现状保持）
- 当多个匹配 mtime 相同（同秒常见）时，则次级排序键为**显示路径字典序**（保证测试可复现、翻页稳定）
- 当 `filter` 与排序并存时，则先过滤后排序——返回的是"过滤后的最新 N 条"，不是"最新 N 条再过滤"（语义正确性：过滤后取前 N，见 §4.2）

**US-GL4 废弃参数现状确认（方向对称）**
`include_dirs` 移除已由 commit `5ec0ba4b1` 落地（schema 无该属性 + `additionalProperties: false` 拒绝 + 测试 `should_not_expose_deprecated_include_dirs_in_schema` 锁定），本条为现状确认，无实现工作量。
- 当仍传 `include_dirs` 时，则参数校验拒绝（已生效行为）
- 当历史会话回放中含 `include_dirs` 调用帧、resume 重放时，则回放层不受影响（wire 记录照旧，仅新调用被拒）

**US-GL5 安全基线不回退**
- 当匹配包含敏感文件时，则任意 filter/limit 组合照旧过滤（filter 不能"捞回"被敏感过滤掉的文件）
- 当搜索根越出沙箱时，则沿用现有路径守卫拒绝文案

**US-GL6 界面呈现（消费端参考，非引擎对齐项）**
- 当输出含计数头与截断尾行时，则 chip 计数 = 实际返回的路径行数（**不含**计数头与截断行——修正现行"非空行数"取数规则对新格式的误计）；glance 展示前 3 条路径 + `, +N more`，N 取计数头的 N——具体由 TUI/web 消费端实现时对齐，本条为方向声明

## 2. 目标与非目标

### 目标

- 承接 Bash 中 `rg --files | rg <filter>` 和 `find -name` 的结构化部分。
- 让"找最近改过的 X 类文件"一次调用完成（mtime 排序 + 过滤 + 分页）。

### 非目标（明确不做）

- **不做**内容搜索——那是 Grep 的领地；过滤参数只作用于**路径字符串**。
- **不做**目录列举——files-only 是已裁决契约（deprecated `include_dirs` 已废弃）。
- **不做**跨根多路径数组——Glob 的 pattern 本身支持逗号分隔多模式（现状已有），够用。
- **不做**文件元信息返回（大小/mtime 数值）——保持纯路径列表输出；如需元信息用 Bash `ls -la` / `stat`。

## 3. 模型可见接口（LLM 描述设计）

### 3.1 面向模型的 description（设计稿）

```
Find files by name pattern, sorted by modification time (most recent first).

Respects `.gitignore`, `.ignore`, and `.rgignore` by default — set `include_ignored` to also match ignored files. Sensitive files (`.env` etc.) are always filtered out. Matches are files only; to find a directory, glob for a file inside it (e.g. `**/fixtures/**`).

Narrow results without a shell pipeline:
- `filter` narrows matched paths by substring or regex (`"skill"` or `"test-\\d+"`), case-insensitive — replaces `rg --files | rg -i 'x'`.
- `limit` controls how many paths are returned (default 200). When more match, the result says so and tells you the total, so you can refine `pattern` or `filter` instead of paging blindly.
- Sorting is most-recently-modified first — ideal for "the newest X": `{"pattern":"*.log","filter":"ws-"}` finds the latest ws-*.log without `ls -t | head`.

Good patterns:
- `*.ts` — extension match at any depth (a bare pattern without `/` matches recursively)
- `src/**/*.ts` — recursive walk anchored under src/
- `*.{ts,tsx}` — brace expansion; `{src,test}/**/*.ts` cartesian too

Avoid recursing into dependency/build dirs even with an anchor (`node_modules/**` can flood results): prefer specific subpaths like `node_modules/react/src/**/*.js`.
```

**设计意图**（GL-ADR-1）：与 Grep 的描述工程同一手法——第二段直接命名 "**Narrow results without a shell pipeline**"，逐条把 `rg --files | rg` 的 Bash 习惯映射到 `filter`/`limit` 参数，并示范"找最新文件"场景（mtime 排序是 Glob 相对 Bash 的独有优势，现状描述只字未提）。

### 3.2 参数契约（变化标 🆕）

| name | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `pattern` | string | ✅ | — | 不变（逗号分隔多模式=并集，现状已支持） |
| `path` | string | 可选 | 工作目录 | 不变 |
| `filter` | string 🆕 | 可选 | — | 对**匹配路径**做二次过滤：不区分大小写的子串或正则（含 `\\d`、`+`、`(` 等字符视作正则，否则子串）。可加 `!` 前缀取反（`"!test"` = 路径不含 test） |
| `limit` | integer 🆕 | 可选 | **200**（原硬编码 100） | 返回条数上限；`0` = 不限制（受双硬上限约束：10,000 条 / 1 MiB 字节，见 US-GL2） |
| `include_ignored` | boolean | 可选 | false | 不变 |
| `include_dirs` | — | — | — | **现状确认已移除**（`5ec0ba4b1` 落地；schema 无该属性，`additionalProperties: false` 拒绝） |

### 3.3 输出契约

**结构（核心变化 🆕）——带元信息的头 + 路径列表**：

```
4 matches (of 137 matching before filtering, 12 after filter) — most recent first
apps/mirri-web/src/api/daemon/ws-rpc-api.ts
apps/mirri-web/src/api/daemon/agentEventProjector.ts
packages/node-sdk/src/sdk-rpc-client-rust.ts
rust/crates/mirri-server/src/rpc.rs
[Results truncated at 4 of 12 — refine pattern or filter, or pass limit]
```

- 首行**计数头**：模板与退化形态以 US-GL2 的唯一字面为准（未传 `filter` 退化为 `N matches — most recent first`）。M/K 让模型知道"漏了多少"，替代盲目翻页（G-ADR 同款哲学：结果自带裁剪语义）。
- 截断行保留现有文案风格。

## 4. 内部设计（架构）

### 4.1 分层（现状沿用，改动标 🆕）

```
mirri-core/tools/glob.rs       工具层：schema、路径沙箱校验、
                               敏感文件过滤、输出渲染
   │                           🆕 filter 解析（子串/正则/!取反，元字符集合见 US-GL1）
   │                           🆕 limit 参数（替代硬编码 MAX_GLOB_MATCHES）
   │                           🆕 计数头渲染（after_filter 计数在此层算）
   ▼
mirri-os/glob.rs               搜索层：globset 编译 pattern +
                               ignore walker + mtime 排序 + 截断
   │                           🆕 GlobParams 增加 limit 字段（工具层 limit 下沉，
   │                              取代搜索层无条件 truncate(100)——否则工具层
   │                              永远只拿到 100 条，limit: 200 形同虚设）
   │                           🆕 GlobResult 增加 total_matched 字段（见下）
   ▼
ignore crate walker            （不变）
```

### 4.2 关键机制

**filter 实现（R1）**：在工具层（glob.rs 渲染前）对 `GlobResult.matches` 应用过滤。判断顺序：按 US-GL1 定义的元字符触发集合（不含 `.`）判定正则/子串，均不区分大小写；`!` 前缀取反。**过滤发生在 limit 截断之前**——先全量匹配、过滤、再排序、再截断，保证"过滤后取前 N 个最新"语义正确。`after_filter` 计数在工具层过滤时计算（评审核实：过滤在工具层，搜索层无从得知——`GlobResult` 只加 `total_matched`，不塞 `after_filter`）。

**总数报告（R2/R3）**：现状事实（评审核实修正）：`LocalGlob` **先全量收集进 `collected`、排序后才 `truncate(100)`**——total 本就免费可得，直接取截断前 `collected.len()`，无需"多走整棵树"的权衡。`GlobResult` 增加 `total_matched: usize`；M/K 口径为敏感过滤后（防止计数头泄漏被滤敏感文件的存在性，US-GL2）。

**limit 语义与搜索层联动**：`GlobParams` 增加 `limit` 字段，搜索层按它截断（取代无条件 `truncate(100)`）；工具层默认 200（GL-ADR-2），理由：调研中 Bash `rg --files | rg | head` 的 head 参数 p90 ≈ 40、最大 100；200 覆盖绝大多数意图，且计数头让"被截断"不再是黑洞。`0` = 不限，但受**新增双硬上限**约束（匹配数 10,000 / 渲染字节 1 MiB——现状无任何上限是评审 MUST-FIX 确认的缺口，此为本设计新增工作）。

**排序并列序**：mtime 相同时以显示路径字典序为次级键（US-GL3），现状 `sort_by_key(Reverse(mtime))` 需改为双键比较。

## 5. 决策记录（已裁决并入 tool-calling-architecture.md ADR-6；此处保留原始论证）

### GL-ADR-1：描述工程 + filter/limit 双参数，而非只改描述
备选：只做描述强化（教模型用现状 Glob）。拒绝：Glob 的硬伤是**能力**（无过滤、上限死），描述救不了——332 次调用里没有任何"参数可调空间"，模型没有别的用法可选。

**修订（2026-10-22 用户裁决，与 G-ADR-2 同批）**：描述里 "Powered by ripgrep's walker" 移除——该表述双重不当：① 实现指涉强化"直接用 Bash rg"联想（同 G-ADR-2 理由）；② 技术上**不准确**——Glob 底层是 `ignore`/`globset` crate 进程内运行（`rust/crates/mirri-os/src/glob.rs`），根本不依赖 rg 二进制。改为纯语义描述（只说 ignore 规则行为，不说实现引擎）。

### GL-ADR-2：limit 默认 200，而非 100 或无限
备选 A：保持 100。拒绝：100 是拍脑袋值，数据里模型在 Bash 里 head 的中位是 20~40、p90 100——200 是"默认宽松、报数不黑箱"。
备选 B：默认无限。拒绝：大目录直接打爆（现状 `LocalGlob` 无任何输出上限——本设计补双硬上限 10,000 条 / 1 MiB 字节后才敢谈"无限"）。

### GL-ADR-3：filter 做在结果层（工具层），而非下沉到 walker
备选：给 walker 加过滤回调（遍历时即丢）。拒绝：分层职责——walker 管"匹配文件"，过滤是"呈现意图"；结果层过滤让 filter 语义与 limit/计数头在同一层，测试与推理都简单；遍历性能差异在实测中可忽略（§4.2）。

### GL-ADR-4：`include_dirs` 移除——现状确认（已落地）
评审核实：早前计划 `plan/rust-builtin-tool-schema-naming.md` 的移除已由 commit `5ec0ba4b1` 完成（schema 无该属性 + `additionalProperties: false` + 测试锁定），无实现工作量。本 ADR 修订为现状确认；唯一保留的新增量是 US-GL4 第二条的回放不受影响验收。

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| filter 正则误用（模型传非法正则） | 含正则元字符但编译失败 → 按子串处理并在描述中说明该回退；错误文案给示例 |
| 全量计数在大目录的性能（include_ignored 全开） | 双硬上限（10,000 条 / 1 MiB 字节）兜底；walker 并行（ignore crate 默认并行遍历）现状保留 |
| 计数头增加每调用固定 token | 一行 ~15 token，对比它替代的"盲目重查/翻页"收益为正 |
| limit=0 无限 + 敏感文件大量命中 | 敏感过滤在渲染前仍然执行（不变）；双硬上限（10,000 条 / 1 MiB 字节）截断 |
| 与 Grep 描述风格耦合（两工具描述同时改） | 描述各自独立成文（§3.1），Rollout 分 PR，可单独回滚 |

## 7. 成功标准（可测量）

1. Glob 调用次数 ≥ 5 倍（332 → 1,600+）
2. Bash 中 `rg --files | rg` 式过滤命令归零或接近（118 → <20）
3. Glob 输出均值 ≤ 2,500B（带计数头的路径列表）
4. 截断时模型能感知总数（计数头存在率 100% 的截断调用）
5. 无新增错误率（filter 非法输入回退子串而非报错）

## 8. Rollout

1. **阶段 1**：filter + limit + 计数头 + `include_dirs` 移除——同一 PR，rust core（glob.rs 工具层 + mirri-os/glob.rs 搜索层），契约快照重生成。changeset：`minor`（新参数可选、默认值披露）+ `include_dirs` 移除按早前裁决记录破坏性说明。
2. **阶段 2**：描述替换（§3.1），单独 PR。
3. **阶段 3**：上线 2 周复测 §7；数据决定是否进入阶段 4（如路径元信息、按 mtime 过滤）。

## 9. 与 Grep 设计的关系

两文档同源于一次调研（[tool-calling-bash-competition-context.md](../tool-calling-bash-competition-context.md)），共享三个设计哲学：**结果自带裁剪语义**（计数头/分页提示）、**描述里显式映射 Bash 管道习惯**、**能力增强只做结构化子集**。实现落点同为 rust core 文件类工具族，建议同一里程碑交付、分 PR 落地。
