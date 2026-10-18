# 工具调用域 — 内建工具详细规格（设计文档）

> 状态：v1 · 2026-09-10 ·
> **定位**：本文件是 tool-calling 域的补充设计文档——逐工具给出 27 个内建工具（注册名口径）的**接口契约**（面向模型的描述原文、参数契约、输出格式、错误文案、非功能上限、界面呈现），作为 **Rust 内核对齐的权威规格源**。这是 Rust 需要对齐的**底线**，不是天花板；Rust 可以做得更好。
> 与域内其他文档的关系：产品语义（用户故事）见 [tool-calling-requirements.md](tool-calling-requirements.md) E4；跨工具机制（注册/执行管线/截断/策略）见 [tool-calling-architecture.md](tool-calling-architecture.md)；代码定位（文件:行号、内部符号）见 [tool-calling-context.md](tool-calling-context.md) §1.8，本文不重复。
> **约定**：① 面向模型的 description 与错误文案均为**契约原文照录**（英文，含标点与格式）——它们是模型可见接口的一部分，Rust 侧应逐字对齐或刻意改进（改进须披露）；② 数值上限为基线值；③ 叙述用中文，专名/参数/文案保留英文；④ 各工具"界面呈现"小节为**消费端参考**（终端现行行为），不构成引擎对齐项——引擎对齐以参数/输出/错误/上限为准。

## 0. 通用约定（全部内建工具共享）

### 0.1 注册与激活

- 全部内建工具经**贡献声明**自注册进装配表，激活遍历按 角色工具面 → 工作区否决 → 生效谓词 判定后才构造注册（机制见 architecture §3）。
- `source` 一律缺省为 `builtin`；`disclosure` 一律未设（即 inline 直接可见；MCP/媒体面另有安排）。
- 带**生效谓词（when）**的工具只有两族：`WebSearch`（搜索 provider 已注入）与 goal 四件套（仅 main agent，子代理不可见）。`ReadMediaFile` 另有安排——不走贡献表，由媒体注册器按模型能力门控动态注册/撤销（模型切换时重建），门控即注册本身，与 when 谓词是两种机制。
- **profile 差异**（激活的模型可见投影）：默认 `agent` 主代理与 `coder` 子代理 profile 均含任务三件套、cron 三件套、TodoList、Plan 两件套；`explore` 子代理 profile 不含它们。`Agent` 工具描述中的 `Tools:` 行即该裁决的投影。

### 0.2 参数契约渲染

- 参数 schema 一律渲染为 JSON Schema draft-7（input 视图）：所有对象节点强制 `additionalProperties: false`（未声明字段拒绝）；带默认值的字段保持**可选**。
- 参数名遵循领域直觉命名规约（architecture §1.1）；数值形态宽容见 requirements US2.10。
- **别名列**：带「alias（Rust 静默别名）」列的参数表 = 该参数有已裁决的引擎内部静默别名（ADR-1 封闭清单，当前仅 Read 两例）；无该列的表即无别名。别名永不进入 schema/描述，与权威名并传即拒，命中可观测（ADR-1 六条护栏）。

### 0.3 执行元数据（每个工具都携带）

| 项 | 语义 |
|---|---|
| `description` | 执行描述（卡片头部动词短语来源，如 `Reading {path}`） |
| `display` | 结构化输入展示判别（协议级共享，审批面板与卡片渲染共用；不驱动卡片本体） |
| `approvalRule` | 审批刻面（字面/通配规则串，供权限规则匹配；审批 UX 归 approval 域） |
| `accesses` | 资源访问声明（并发调度与冲突判定依据，architecture §4） |

### 0.4 结果构建

- 输出经缓冲式构建：总量上限 50,000 字符、单行上限 2,000 字符（Bash 用默认；FetchURL/WebSearch 关闭单行上限）；超限逐行追加 `[...truncated]`，成功/失败结果在截断时追加 `Output is truncated to fit in the message.`。
- 执行链末端超大纯文本另有**全文落盘**两级截断（预览 2,000 字符 + 唯一输出路径，requirements US3.3）。（注：Read 的单行截断标记 `...` 是工具自身的行级截断，与结果构建器的 `[...truncated]` 分属两层。）

### 0.5 路径守卫（文件类工具共用）

先敏感文件判定（`.env` 家族、凭据、SSH 私钥命中即拒；`.env.example` 族与公钥豁免），再路径规范化（`~` 展开、纯词法归一、不解析符号链接），最后工作区边界判定（相对越界拒绝并要求绝对路径；绝对越界放行但标记供审批；守卫可配置关闭且不影响敏感检查）。共享文案模板：

- 越界相对路径（读/写/搜各一词）：`"${path}" is not an absolute path. You must provide an absolute path to ${read a file|write or edit a file|search} outside the working directory.`
- 敏感命中：`"${path}" matches a sensitive-file pattern (env / credential / SSH key). Access is blocked to protect secrets.`

### 0.6 工具索引（27 个）

| # | 工具 | 一句话 | 关键上限 |
|---|---|---|---|
| 1 | `Read` | 读文本文件（行号视图） | 1,000 行 / 100 KB / 行 2,000 字符 |
| 2 | `Write` | 整体创建/追加/覆写文件 | 无内容上限（大内容分块追加） |
| 3 | `Edit` | 精确字符串替换 | old_string 须唯一或 replace_all |
| 4 | `Glob` | 按文件名模式找文件 | 100 个文件 / 20s 超时 |
| 5 | `Grep` | 按正则搜内容 | 默认 250 行 / 20s 超时 |
| 6 | `Bash` | 执行命令 | 前台 60s/300s；后台 600s/24h |
| 7 | `FetchURL` | 抓取公开网页 | 响应体 10 MB / 重定向 10 跳 |
| 8 | `WebSearch` | 网页搜索 | provider 门控 |
| 9 | `ReadMediaFile` | 读图片/视频 | 文件 100 MB / 解码 64 MB / 压缩 256KB·2000px |
| 10 | `Agent` | 派发子代理 | 超时 2h（可配置） |
| 11 | `AgentSwarm` | 模板批量派发 | ≤128 且 ≥2 |
| 12 | `AskUserQuestion` | 结构化提问 | 1–4 问 × 2–4 选项 |
| 13 | `Skill` | 调用技能 | 嵌套深度 ≤3 |
| 14 | `select_tools` | 显式加载动态工具 | 实验开关门控 |
| 15–17 | `TaskList`/`TaskOutput`/`TaskStop` | 后台任务查询/读取/停止 | 预览 32 KiB / limit 1–100 |
| 18 | `TodoList` | 任务清单（整体替换） | 无 schema 上限 |
| 19–22 | `CreateGoal`/`GetGoal`/`UpdateGoal`/`SetGoalBudget` | 目标管理 | 时间预算 1s–24h；仅 main agent |
| 23–24 | `EnterPlanMode`/`ExitPlanMode` | 计划模式 | options 1–3 |
| 25–27 | `CronCreate`/`CronList`/`CronDelete` | 定时任务 | 每会话 ≤50 / prompt 8 KiB |

（共 27 个注册工具：#1–14 为单体工具，#15–27 为 13 个工具、4 个族。历史文档曾用「22 工具」口径（goal/cron 族各计 1），本文以 27 个注册名为准。）

**后台能力分布**：仅 `Bash`、`Agent`、`AskUserQuestion` 三个工具支持**模型主动后台化**（返回任务号、终态经自动通知送达，requirements US2.6/US2.11）；`AgentSwarm` 固定前台（内部子代理并行、结果聚合返回）；其余工具均为同步执行（同步工具在同批内仍可与不冲突调用并行，见架构 §4 调度）。补充通道两条：Bash 前台超时**自动**转后台（可配置为杀灭）；用户可随时把前台执行转后台（终端快捷键——用户侧操作，非模型能力）。后台化的前提是任务三件套（TaskList/TaskOutput/TaskStop）在当前 agent 工具面内启用，否则后台调用在预检被拒并给出明确原因。

---

## 第一部分 文件类工具

### 1.1 Read

- **注册**：名称 `Read`；目录描述 `Read a file from the local filesystem.`；domain `os/backends`；capabilities `['code.read']`；无 when。
- **执行元数据**：描述 `Reading ${path}`；展示 `file_io/read`；审批刻面 `Read` + 路径主体；资源访问 读单文件。

**面向模型的描述（原文，经模板渲染注入 1000/100/2000 三个上限值）**：

```
Read a text file from the local filesystem.

If the user provides a concrete file path to a text file, call Read directly. Do not `Glob`, `ls`, or otherwise pre-check known text file paths; missing or invalid file paths return errors you can handle. Do not use Read for directories; use `ls` via Bash for a known directory, or Glob when you need files matching a name pattern (Glob lists files only, never directories). Use `Grep` only when the task is to search for unknown content or locations.

When you need several files, prefer to read them in parallel: emit multiple `Read` calls in a single response instead of reading one file per turn.

- Relative paths resolve against the working directory; a path outside the working directory must be absolute.
- Returns up to 1000 lines or 100 KB per call, whichever comes first; lines longer than 2000 chars are truncated mid-line.
- Page larger files with `line_offset` (1-based start line) and `n_lines`. Omit `n_lines` to read up to the 1000-line cap.
- Sensitive files (`.env` files, credential stores, SSH private keys, and similar secrets) are refused to protect secrets; do not attempt to read them. Templates and public keys are exempt: `.env.example` / `.env.sample` / `.env.template` and public SSH keys such as `id_rsa.pub` read normally.
- Only UTF-8 text files can be read. Non-UTF-8 encodings, binary files, and files containing NUL bytes are refused; use `ReadMediaFile` for images or video, and Bash or an MCP tool for other binary formats.
- Negative line_offset reads from the end of the file (for example, -100 reads the last 100 lines); the absolute value cannot exceed 1000.
- Output format: `<line-number>\t<content>` per line.
- A `<system>...</system>` status block is appended after the file content; it summarizes how much was read (line and byte counts, truncation, line-ending notes) and is not part of the file itself.
- Pure CRLF files are displayed with LF line endings; `Edit` matches this output and preserves CRLF when writing back.
- Mixed or lone carriage-return line endings are shown as `\r` and require exact `Edit.old_string` escapes.
- After a successful `Edit`/`Write`, do not re-read solely to prove the write landed. When the task depends on an exact file, API, or output shape, inspect the final external contract before finishing.
```

**参数契约**：

| name | alias（Rust 静默别名） | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|---|
| `path` | — | string | ✅ | — | `Path to a text file. Relative paths resolve against the working directory; a path outside the working directory must be absolute. Directories are not supported; use `ls` via Bash for a known directory, or Glob for pattern search.` |
| `line_offset` | `offset` | integer（正数 ≥1，或负数 −1..−1000） | 可选 | 1 | `The line number to start reading from. Omit to start at line 1. Negative values read from the end of the file; the absolute value cannot exceed 1000.` |
| `limit`（Rust 目标名） | `n_lines` | integer（>0） | 可选 | 1000 | `The number of lines to read; the tool also applies its internal cap. Omit to read up to the internal cap of 1000 lines.` |

（TS 基线契约为 `line_offset` + `n_lines`；Rust 目标命名 `limit` 与两条静默别名均为已裁决增强，机制见 architecture ADR-1 / requirements P-2：`offset`→`line_offset` 为**实证幻觉类**——`line_offset` 保持权威名（"line" 提供单位语义）；`n_lines`→`limit` 为**改名过渡类**——与 TaskList 统一"取多少"语义。）

**输出契约**：
- 成功输出 = 逐行 `<lineNo>\t<content>`（行号 1 起）；超长行截断为前 1997 字符 + `...`（总长恰 2000）。
- 行尾归一化：纯 CRLF 文件显示为 LF；mixed/lone-CR 显示字面 `\r`。
- **note 侧信道**（仅模型可见，不进 UI），`<system>` 包裹，语句空格连接：有行 `${lineCount} lines read from file starting from line ${startLine}.`；空 `No lines read from file.`；`Total lines in file: ${totalLines}.`；上限命中三选一 `Max 1000 lines reached.` / `Max 102400 bytes reached.` / `End of file reached.`；截断行 `Lines [${numbers}] were truncated.`；mixed 行尾提示 `Mixed or lone carriage-return line endings are shown as \r. Use exact \r\n or \r escapes in Edit.old_string for those lines.`。
- 尾部模式（负偏移）：读最后 `|line_offset|` 行构成尾部窗口，再收缩——`n_lines` 保留窗口内**靠前**的 n_lines 行（例：`line_offset:-100, n_lines:50` 返回倒数第 100~51 行），100 KB 从末尾向前保留能装下的行。

**错误面**：

| 条件 | 文案 |
|---|---|
| 文件不存在 | `"${args.path}" does not exist.` |
| 非普通文件 | `"${args.path}" is not a file.` |
| 图片/视频文件 | `"${args.path}" is a ${kind} file. Use ReadMediaFile to read image or video files.` |
| 二进制/NUL/非 UTF-8 | `"${path}" is not readable as UTF-8 text. If it is an image or video, use ReadMediaFile. For other binary formats, use Bash or an MCP tool if available.` |
| 路径守卫拒绝 | 守卫文案（§0.5） |

**非功能**：上限 1,000 行、单行 2,000 字符、100 KB（102,400 字节，按渲染后行含行号与 tab 计）；媒体头嗅探 512 字节；无超时。

**界面呈现**：chip = 非空行数（`42 lines` / `1 line`）；渲染器折叠态仅 chip，展开显原输出（暗色）；错误走通用截断渲染（error 色，前 3 行）。

### 1.2 Write

- **注册**：名称 `Write`；目录描述 `Create, append to, or replace a file entirely.`；domain `os/backends`；无 when、无 capabilities。
- **执行元数据**：描述 `Writing ${path}`；展示 `file_io/write`（携带 content）；审批刻面 `Write` + 路径主体；资源访问 写单文件。

**面向模型的描述（原文）**：

```
Create, append to, or replace a file entirely.

- Missing parent directories are created automatically (like `mkdir(parents=True, exist_ok=True)`).
- Mode defaults to overwrite; append adds content at EOF without adding a newline.
- Write is NOT ALLOWED for incremental changes to existing files, including trivial, one-line, quick, or cosmetic edits. Use Edit instead.
- Use Write only when the file does not exist, you intend a complete replacement, or the new contents have little continuity with the old contents.
- Do not create unsolicited documentation files (`*.md` write-ups, `README`s, summaries) just because a task finished — write one only when the user asks for it, or when a task or project instruction requires it (e.g. the plan-mode plan file, created with Write when plan mode directs you to, or a changeset the repo mandates).
- Read before overwriting an existing file.
- Write ignores the Read/Edit line-number view. NEVER include line prefixes.
- Write outputs content literally, including supplied line endings: \n stays LF, \r\n stays CRLF.
- For new content too large for one call, overwrite the first chunk, then append subsequent chunks. Never chunk Write to modify an existing file.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `path` | string | ✅ | — | `Path to the file to create, append to, or completely overwrite. Relative paths resolve against the working directory; a path outside the working directory must be absolute. Missing parent directories are created automatically.` |
| `content` | string | ✅ | — | `Raw full file content to write exactly as provided. This does not use the Read/Edit text view.` |
| `mode` | enum `['overwrite','append']` | 可选 | `overwrite` | `Write mode. Defaults to overwrite. append adds content to the end exactly as provided and does not add a newline.` |

**输出契约**：成功 `Wrote ${bytes} bytes to ${path}`；append 时 `Appended ${bytes} bytes to ${path}`（bytes = content 的 UTF-8 字节数）。无 note。

**错误面**：

| 条件 | 文案 |
|---|---|
| 父路径不是目录 | `Parent path is not a directory: ${parent}.` |
| 父目录创建失败 | mkdir 的 `error.message` |
| 写入 ENOENT | `Failed to write ${args.path}: parent directory does not exist.` |
| 其他/路径守卫 | `error.message`（守卫文案见 §0.5） |

**非功能**：无内容上限（大内容：首块 overwrite + 后续 append）；append 走原生追加语义（不读不重写旧内容）；无超时。

**界面呈现**：chip = **参数** content 的行数（去单个尾换行后计，`5 lines`）——基于参数而非结果。

### 1.3 Edit

- **注册**：名称 `Edit`；目录描述 `Perform exact replacements in an existing file.`；domain `edit`；无 when、无 capabilities。
- **执行元数据**：描述 `Editing ${path}`；展示 `file_io/edit`（携带 before/after）；审批刻面 `Edit` + 路径主体；资源访问 读写单文件。

**面向模型的描述（原文）**：

```
Perform exact replacements in existing files.

- Edit is mandatory for every incremental change, especially small edits. DO NOT use Write or Bash `sed`.
- Read the target file before every Edit. DO NOT call Edit from memory, stale context, or a guessed `old_string`.
- Take `old_string` and `new_string` from the Read output view.
- Drop the line-number prefix and tab; match only file content.
- `old_string` must be unique unless `replace_all` is set.
- If `old_string` is ambiguous, add surrounding context. Use `replace_all` only when every occurrence should change — for example, renaming a symbol throughout the file.
- Multiple Edit calls may run in one response only when they do not target the same file.
- DO NOT issue consecutive Edit calls on the same file. A previous Edit can invalidate a later Edit's `old_string`, causing `old_string not found`. Read the file again before the next Edit.
- A write lock serializes same-file edits in response order, but serialization does not make stale `old_string` valid.
- For pure CRLF files, Read shows LF; use LF in `old_string` and `new_string`, and Edit writes CRLF back.
- For mixed endings or lone carriage returns, Read shows carriage returns as \r; include actual \r escapes in those positions.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `path` | string | ✅ | — | `Path to the text file to edit. Relative paths resolve against the working directory; a path outside the working directory must be absolute.` |
| `old_string` | string（≥1） | ✅ | — | `Exact content to replace from the Read output view, without the line-number prefix. Use LF for pure CRLF files; use actual \r escapes where Read shows \r.` |
| `new_string` | string | ✅ | — | `Replacement text in the same Read output view. LF is written back as CRLF only for pure CRLF files.` |
| `replace_all` | boolean | 可选 | `false` | `Set true only when every occurrence of old_string should be replaced.` |

**输出契约**：成功 `Replaced ${count} occurrence(s) in ${path}`（1 时单数）。

**错误面**：

| 条件 | 文案 |
|---|---|
| old_string 未找到 | `old_string not found in ${path}, the file contents may be out of date. Please use the Read Tool to reload the content.` |
| 不唯一且未 replace_all | `old_string is not unique in ${path} (found ${count} occurrences). To replace every occurrence, set replace_all=true. To replace only one occurrence, include more surrounding context in old_string.` |
| old == new | `No changes to make: old_string and new_string are exactly the same.` |
| 目标是目录 | `${displayPath} is not a file.` |
| 其他/守卫 | `error.message` |

**非功能**：无上限；行尾语义（LF 匹配、CRLF 回写、mixed 精确 `\r`）；同文件并发由写锁按响应序串行。

**界面呈现**：chip 基于**参数** diff——`+${added} -${removed}`（如 `+3 -1`；0/0 时无 chip）。

### 1.4 Glob

- **注册**：名称 `Glob`；目录描述 `Find files by glob pattern, sorted by modification time.`；domain `os/backends`；capabilities `['code.explore']`；无 when。
- **执行元数据**：描述 `Searching ${pattern}`；展示 `file_io/glob`；审批刻面 `Glob` + **pattern** 通配主体（区别于路径匹配）；资源访问 搜索目录树。

**面向模型的描述（原文；Windows 环境追加 Windows 路径说明段）**：

```
Find files by glob pattern, sorted by modification time (most recent first).

Powered by ripgrep. Respects `.gitignore`, `.ignore`, and `.rgignore` by default — set `include_ignored` to also match ignored files (e.g. build outputs, `node_modules`). Sensitive files (such as `.env`) are always filtered out. Matches are files only — directories themselves are never listed; to find a directory, glob for a file inside it (e.g. `**/fixtures/**`).

Good patterns:
- `*.ts` — all files matching an extension, at any depth below the search root (a bare pattern without `/` matches recursively)
- `src/*.ts` — files directly inside `src/` (one level, not recursive)
- `src/**/*.ts` — recursive walk with a subdirectory anchor and extension
- `**/*.py` — recursive walk from the search root for an extension
- `*.{ts,tsx}` — brace expansion is supported
- `{src,test}/**/*.ts` — cartesian brace expansion is supported too

Results are capped at the first 100 matching paths. If a search would return more, a truncation marker is appended. Refine the pattern (extension, subdirectory) when 100 is not enough, or call again with a narrower anchor.

Large-directory caveat — avoid recursing into dependency / build output even with an anchor, especially when `include_ignored` is set:
- `node_modules/**/*.js`, `.venv/**/*.py`, `__pycache__/**`, `target/**` can produce thousands of results that truncate at the match cap and waste context. Prefer specific subpaths like `node_modules/react/src/**/*.js`.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `pattern` | string | ✅ | — | `Glob pattern to match files.` |
| `path` | string | 可选 | 工作目录 | `Directory to search. Accepts an absolute path, or a path relative to the current working directory. Defaults to the current working directory.` |
| `include_ignored` | boolean | 可选 | `false` | `Also match files excluded by ignore files such as `.gitignore`, `.ignore`, and `.rgignore` (for example `node_modules` or build outputs). Sensitive files (such as `.env`) remain filtered out for safety. VCS metadata directories (`.git` and similar) are always skipped, even when this is true. Defaults to false.` |
| `include_dirs` | boolean | 可选 | —（忽略） | `Deprecated and ignored. Results are always files-only — directories are never listed. Accepted only so older calls that still pass this flag are not rejected by parameter validation.` |

**输出契约**：路径按 mtime 降序，每行一个（搜索根在工作区内时相对化，外部根保持绝对）；前缀提示行按序：超时 `Glob timed out after 20s; partial results returned.`、stdout 截断 `[stdout truncated at 10485760 bytes; results may be incomplete — use a more specific pattern]`、遍历告警 `Glob completed with warnings; some directories could not be read: ${stderr}`、超上限 `[Truncated at 100 matches — use a more specific pattern]` + `Only the first 100 matches are returned.`；末尾行 `Filtered ${N} sensitive file(s).` / 恰满 100 未截断 `Found ${N} matches`；空结果 `No matches found`（命中全为敏感时 `No non-sensitive matches found (${N} sensitive file(s) filtered).`）。

**错误面**：

| 条件 | 文案 |
|---|---|
| 根不存在 | `${searchRoot} does not exist` |
| 根不是目录 | `${searchRoot} is not a directory` |
| 中止 | `Glob aborted` |
| rg 不可用 | `ripgrep (rg) is not available and the automatic bootstrap failed.\n\nError: ${detail}\n\nFix options:\n  macOS:   brew install ripgrep\n  Ubuntu:  sudo apt-get install ripgrep\n  Other:   https://github.com/BurntSushi/ripgrep#installation\n\nAlternatively, drop a static rg binary at ${shareBin}` |
| rg 非常规退出 | `Glob failed: ${stderr}` 或 `Glob failed` |

**非功能**：上限 100 个匹配；超时 20s（超时返回部分结果）；输出 10 MiB 封顶；终止宽限 5s 后强杀；系统资源紧张时自动降为单线程重试一次；敏感过滤 = rg 层预滤 + 返回后权威后滤（计入 filtered 计数）。

**界面呈现**：chip = 非空行数（`N files` / `no files`）；折叠态 glance 显示前 3 条路径 + `, +N more`；VCS 目录恒排除。

### 1.5 Grep

- **注册**：名称 `Grep`；目录描述 `Search file contents using regular expressions.`；domain `os/backends`；capabilities `['code.explore']`；无 when。
- **执行元数据**：描述 `Searching for '${pattern}' in ${searchPath}`；展示 `file_io/grep`；审批刻面 `Grep` + **pattern** 通配主体；资源访问 搜索目录树。

**面向模型的描述（原文）**：

```
Search file contents using regular expressions (powered by ripgrep).

Use Grep when the task is to find unknown content or unknown file locations. Do not use shell `grep` or `rg` directly; this tool applies workspace path policy, output limits, and sensitive-file filtering.
ALWAYS use Grep tool instead of running `grep` or `rg` from a shell — direct shell calls bypass workspace policy, output limits, and sensitive-file filtering.
If you already know a concrete file path and need to inspect its contents, use Read directly instead.

Write patterns in ripgrep regex syntax, which differs from POSIX `grep` syntax. For example, braces are special, so escape them as `\{` to match a literal `{`.

Hidden files (dotfiles such as `.gitlab-ci.yml` or `.eslintrc.json`) are searched by default. To also search files excluded by `.gitignore` (such as `node_modules` or build outputs), set `include_ignored` to `true`. Sensitive files (such as `.env`) are always skipped for safety, even when `include_ignored` is `true`.
```

**参数契约**（13 参数）：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `pattern` | string | ✅ | — | `Regular expression to search for.` |
| `path` | string | 可选 | 工作目录 | `File or directory to search. Accepts an absolute path, or a path relative to the current working directory. Omit to search the current working directory. Use Read instead when you already know a concrete file path and need its contents.` |
| `glob` | string | 可选 | — | `Optional glob filter for which files to search, e.g. `*.ts`. Matched against each file's full absolute path, so a path-anchored pattern like `src/**/*.ts` silently matches nothing — use a basename pattern (`*.ts`), or anchor with `**/` (`**/src/**/*.ts`). To scope the search to a directory, use `path` instead.` |
| `type` | string | 可选 | — | `Optional ripgrep file type filter, such as ts or py. Prefer this over `glob` when filtering by language or file kind: it is more efficient and less error-prone than an equivalent glob pattern.` |
| `output_mode` | enum `['content','files_with_matches','count_matches']` | 可选 | `files_with_matches` | `Shape of the result. `content` shows matching lines (honors `-A`, `-B`, `-C`, `-n`, and `head_limit`); `files_with_matches` shows only the paths of files that contain a match, most-recently-modified first (honors `head_limit`); `count_matches` shows per-file match counts as `path:count` lines, preceded by an aggregate total line. Defaults to `files_with_matches`.` |
| `-i` | boolean | 可选 | `false` | `Perform a case-insensitive search. Defaults to false.` |
| `-n` | boolean | 可选 | `true` | `Prefix each matching line with its line number. Applies only when `output_mode` is `content`. Defaults to true.` |
| `-A` | integer（≥0） | 可选 | — | `Number of lines to show after each match. Applies only when `output_mode` is `content`.` |
| `-B` | integer（≥0） | 可选 | — | `Number of lines to show before each match. Applies only when `output_mode` is `content`.` |
| `-C` | integer（≥0） | 可选 | — | `Number of lines to show before and after each match. Applies only when `output_mode` is `content`; takes precedence over `-A` and `-B`.` |
| `head_limit` | integer（≥0） | 可选 | `250` | `Limit output to the first N lines/entries after offset. Defaults to 250. Pass 0 for unlimited.` |
| `offset` | integer（≥0） | 可选 | `0` | `Number of leading lines/entries to skip before applying `head_limit`. Use it together with `head_limit` to page through large result sets. Defaults to 0.` |
| `multiline` | boolean | 可选 | `false` | `Enable multiline matching, where the pattern can span line boundaries and `.` also matches newlines. Defaults to false.` |
| `include_ignored` | boolean | 可选 | `false` | 同 Glob。 |

**输出契约**：
- 空结果：`No non-sensitive matches found`。
- `files_with_matches`：每行一个路径（相对 workspaceDir；外部绝对），按 mtime 降序（并发 stat 加速；超时后跳过排序保持原始序）。
- `count_matches`：header `Found ${total} ${total|total non-sensitive} ${occurrence|occurrences} across ${files} ${file|files}.`（敏感文件被滤时作用域词为 total non-sensitive——模型区分滤前/滤后计数的信号）+ 逐行 `${displayPath}:${count}`。
- `content`：`${displayPath}:${lineNo}:${payload}`（`-n:false` 时无行号；上下文分组间 `--` 分隔行，首尾的会被消掉）。
- 末尾 messages 行（按序）：敏感过滤 `Filtered ${N} sensitive file(s): ${paths}`；分页截断 `Results truncated to ${headLimit} lines (total: ${total}). Use offset=${nextOffset} to see more.`；stdout 截断 `[stdout truncated at 10485760 bytes; incomplete trailing line omitted]`；超时 `Grep timed out after 20s; partial results returned`。

**错误面**：

| 条件 | 文案 |
|---|---|
| 搜索前中止 | `Aborted before search started` |
| 运行中中止 | `Grep aborted` |
| rg 不可用 | 同 Glob 的 rg 不可用文案 |
| 超时且无输出 | `Grep timed out after 20s. Try a more specific path or pattern.` |
| rg 非常规退出 | `Failed to grep: ${summary}\n\nripgrep stderr:\n${stderr}`（stderr 空时 `Failed to grep: ripgrep exited with code ${exitCode}`） |

**非功能**：超时 20s（部分结果返回）；输出 10 MiB；非 content 模式 rg 侧 `--max-columns 500`；默认 250 行（0=无限）；分页语义 `offset` 后取 `head_limit`；VCS 目录恒排除；敏感后滤同 Glob。

**界面呈现**：chip = 非空行数（`N matches` / `no matches`；header/提示行也计入）；glance 显示前 3 条剥离 `:line:content` 后的路径 + `, +N more`。

---

## 第二部分 命令与网络

### 2.1 Bash

- **注册**：名称 `Bash`；目录描述 `Execute a bash command on the system.`；domain `os/backends`；无 when、无 capabilities、无 accesses 声明（按最保守与一切冲突处置）。
- **执行元数据**：描述 `Running: {前 50 字符…}` / 后台 `Starting background: {preview}`；展示 `command`（command/cwd/description/language:bash）；审批刻面 `Bash` + 命令文本。
- **描述按条件改写**：任务三件套未激活时改写后台段落——后台段整段替换为 `Background execution is disabled for this agent. Do not set `run_in_background=true`.`（超时句改为 ` For possibly long-running commands, set the `timeout` argument in seconds. The default is 60s; foreground commands allow up to 300s; a foreground command that hits its timeout is killed.`，末条改为 `- Do not set `run_in_background=true`; background task management tools are not available.`）；`bashAutoBackgroundOnTimeout=false` 时超时句替换为 ` A foreground command that hits its timeout is killed.`。

**面向模型的描述（原文，模板注入 shell 名与超时数值）**：

```
Execute a `${SHELL_NAME}` command. Use this for shell semantics — pipes, env, processes, git, package managers, build/test runners, anything genuinely interactive or multi-step.

**Translate these to a dedicated tool instead:**
- `cat` / `head` / `tail` (known path) → `Read`
- `sed` / `awk` (in-place edit) → `Edit`
- `echo > file` / `cat <<EOF` → `Write`
- `find` / recursive `ls` to locate files by name pattern → `Glob` (plain `ls <known-directory>` is fine for listing a directory)
- `grep` / `rg` (search file contents) → `Grep`
- `echo` / `printf` (talk to the user) → just output text directly

The dedicated tools render in the per-tool permission UI and keep raw stdout out of the conversation; that is why they are worth reaching for whenever one fits.

**Output:**
The stdout and stderr will be combined and returned as a string. The output may be truncated if it is too long. If the command exits non-zero, the output ends with a `Command failed with exit code: N` line; a command killed by its timeout or interrupted by the user ends with its own message instead.

If `run_in_background=true`, the command will be started as a background task and this tool will return a task ID instead of waiting for command completion. When doing that, you must provide a short `description`. Background commands default to a 600s timeout and `timeout` is capped at 86400s; set `disable_timeout=true` only when the task should run without a timeout. You will be automatically notified when the task completes. After starting one, default to returning control to the user instead of immediately waiting on it. Use `TaskOutput` only for a non-blocking status/output snapshot — do not wait on a task you just launched, since its completion arrives automatically. Use `TaskStop` only if the task must be cancelled. If a human user wants to inspect background tasks themselves, point them to the `/tasks` command, which opens an interactive panel; it has no subcommands.

**Guidelines for safety and security:**
- Each shell tool call will be executed in a fresh shell environment. The shell variables, current working directory changes, and the shell history is not preserved between calls. To run a command in a particular directory, pass the `cwd` argument (or use absolute paths) rather than relying on a `cd` from an earlier call.
- The tool call will return after the command is finished. You shall not use this tool to execute an interactive command or a command that may run forever. For possibly long-running foreground commands, set the `timeout` argument in seconds. Foreground commands default to 60s and allow up to 300s. When a foreground command hits its timeout it is moved to the background instead of being killed, and you will be automatically notified when it completes.
- Avoid using `..` to access files or directories outside of the working directory.
- Avoid modifying files outside of the working directory unless explicitly instructed to do so.
- Never run commands that require superuser privileges unless explicitly instructed to do so.

**Guidelines for efficiency:**
- Use `&&` to chain commands that genuinely depend on each other, e.g. `npm install && npm test`. Independent read-only commands (separate `git show`, `ls`, or status checks) should be issued as separate parallel Bash calls in one response, not chained into a single call — chaining serializes their execution and mixes their output. Do not stitch outputs together with `echo` separators.
- Use `;` to run commands sequentially regardless of success/failure
- Use `||` for conditional execution (run second command only if first fails)
- Use pipe operations (`|`) and redirections (`>`, `>>`) to chain input and output between commands
- Always quote file paths containing spaces with double quotes (e.g., cd `"/path with spaces/"`)
- Compose multi-step logic in a single call with `if` / `case` / `for` / `while` control flows.
- Prefer `run_in_background=true` for long-running builds, tests, watchers, or servers when you need the conversation to continue before the command finishes.

**Commands available:**
The following common command categories are usually available. Availability still depends on the host, so when in doubt run `which <command>` first to confirm a command exists before relying on it.
- Navigation and inspection: `ls`, `pwd`, `cd`, `stat`, `file`, `du`, `df`, `tree`
- File and directory management: `cp`, `mv`, `rm`, `mkdir`, `touch`, `ln`, `chmod`, `chown`
- Text and data processing: `wc`, `sort`, `uniq`, `cut`, `tr`, `diff`, `xargs`
- Archives and compression: `tar`, `gzip`, `gunzip`, `zip`, `unzip`
- Networking and transfer: `curl`, `wget`, `ping`, `ssh`, `scp`
- Version control: `git`; for GitHub-hosted work (PRs, issues, CI runs, API queries) prefer the `gh` CLI when installed — it carries the user's GitHub auth and can return structured JSON
- Process and system: `ps`, `kill`, `top`, `env`, `date`, `uname`, `whoami`
- Language and package toolchains: `node`, `npm`, `pnpm`, `yarn`, `python`, `pip` (use whichever the project actually relies on)
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `command` | string（≥1） | ✅ | — | `The command to execute.` |
| `cwd` | string | 可选 | 会话目录 | `The working directory in which to run the command. When omitted, the command runs in the session's working directory.` |
| `timeout` | number（秒，>0） | 可选 | 前台 60 / 后台 600 | `Optional timeout in seconds for the command to execute. Foreground default 60s, max 300s. Background default 600s, max 86400s. Ignored for background commands when disable_timeout=true.` |
| `description` | string | 可选（后台必填，运行时校验） | — | `A short description for the background task. Required when run_in_background is true.` |
| `run_in_background` | boolean | 可选 | — | `Whether to run the command as a background task.` |
| `disable_timeout` | boolean | 可选 | — | `If true, do not apply a timeout to the command. Only applies when run_in_background is true.` |

schema 级校验：`timeout` 超上限 `timeout must be ≤ ${cap}s (background|foreground)`。

**输出契约**：
- 前台成功有输出：stdout 与 stderr 按到达顺序交错合并（同一缓冲流式追加，无分隔标记）；无输出时 `Command executed successfully.`。
- 前台非零退出：`Command failed with exit code: N.`（brief `Failed with exit code: N`）；此前无已捕获输出时先补一行 `Process exited with code N`。
- 前台截断（先落盘完整输出）：`[Full output saved]\ntask_id: {taskId}\noutput_path: {outputPath}\noutput_size_bytes: {n}\nnext_step: Use Read with output_path to page through the full log{, or TaskOutput(task_id="{taskId}")}.`
- 后台启动成功（元数据块）：`task_id`、`pid`、`description`、`status`、`automatic_notification: true`、`next_step: The completion arrives automatically in a later turn — do NOT wait, poll, or call TaskOutput on it; continue with your current work.`、`next_step: Use TaskStop only if the task must be cancelled.`、`human_shell_hint: Tell the human to run /tasks to open the interactive background-task panel.`（任务三件套不可用时 next_step 简化为 `You will be automatically notified when it completes.`。）
- 前台超时转后台（默认行为）：标题 `Task moved to background` / `Command timed out and moved to background`；`next_step: The task now runs in the background. You will be automatically notified when it completes — do NOT wait, poll, or call TaskOutput on it; continue with your current work.`；前台已捕获输出追加 `foreground_output:` 段。任务 id 前缀 `bash`。

**错误面**：

| 条件 | 文案 |
|---|---|
| 中止 | `Aborted before command started` |
| 空命令 | `Command cannot be empty.` |
| 后台不可用仍要求后台 | `Background execution is not available for this agent because TaskOutput and TaskStop are not enabled.`（照录原文；Agent 工具的对应文案枚举三个工具名——差异非笔误） |
| 后台缺 description | `description is required when run_in_background is true.` |
| 前台超时被杀灭（配置关闭转后台时） | `Command killed by timeout (${label})` |
| 用户取消 | `Interrupted by user` |
| spawn/注册失败 | `error.message` |

**非功能**：前台默认 60s、上限 300s；后台默认 600s、上限 86400s；`disable_timeout` 仅后台生效；前台超时默认转后台并重布防为后台超时；两阶段杀灭（SIGTERM → 5s 宽限 → SIGKILL）；输出缓冲 50,000 字符 / 行 2,000；每次调用全新 shell（变量/目录/历史不保留）；执行形态 `cd '{cwd}' && {command}`（cwd 单引号转义）；stdin 立即关闭（交互命令收 EOF）。

**界面呈现**：无 chip；卡片头部 `Running a command` / `Ran a command`（错误整体 error 色；参数截断 `Truncated Bash`）；命令预览 `$ ` 前缀 + 命令体（折叠态 ≤10 行）；结果折叠态前 3 行（成功暗色、错误 error 色），超出 `... (N more lines, ctrl+o to expand)`；运行超过短暂阈值后提示按快捷键转后台、用户可随时主动转后台（US2.6）。

### 2.2 FetchURL

- **注册**：名称 `FetchURL`；目录描述 `Fetch content from a URL.`；domain `web`；capabilities `['web.fetch']`；无 when（后端三级回落，始终可用）。
- **执行元数据**：描述 `Fetching: {url}`；展示 `url_fetch`；审批刻面 `FetchURL` + url；资源访问 无。
- **后端选择**（优先级）：① 云端抓取后端（宿主 API 配置或托管登录态两种凭证形态）→ ② 内建本地 provider（无配置可用）；云端失败回落本地。

**面向模型的描述（原文）**：

```
Fetch content from a URL. The content is returned either as the main text extracted from the page, or as the full response body verbatim; a note at the top of the result states which of the two you received, so you can judge how complete it is. Use this when you need to read a specific web page.

Only fully-formed public `http`/`https` URLs are supported; other schemes and private or loopback addresses are not fetched. Very large pages may be truncated or refused. The fetch carries no login or session for the target site, so pages behind authentication (private repositories, internal dashboards) return a login page or an error instead of the real content — if the text you get back looks like a generic landing or sign-in page, treat that as the login wall, not the answer, and reach the content through a credentialed route (an authenticated CLI or MCP tool) instead.
```

**参数契约**：仅 `url`（string，必填）——`The URL to fetch content from.`。

**输出契约**：
- 空 body：`The response body is empty.`。
- 成功：先一行说明（提取态 `The returned content is the main text extracted from the page.` / 透传态 `The returned content is the full response body, returned verbatim.`）+ 恒追加 `If you use it in your answer, cite this page as a markdown link, e.g. [title](url).`，空行后正文。
- 本地 provider 提取规则：HTTP ≥400 拒收；`text/plain` / `text/markdown` 原样透传；其余 HTML 走正文提取（成功 `# {title}\n\n{text}`，失败回退主体文本并同样带标题行）。
- 输出 50,000 字符（单行不限长），截断标记同 §0.4。

**错误面**（外层包装：HTTP 错误 `Failed to fetch URL. Status: {status}. {msg}`；网络错误 `Failed to fetch URL due to network error: {url}. {msg}`）：

| 条件 | 本地 provider 文案 |
|---|---|
| URL 非法 | `Invalid URL: "{url}"` |
| 非 http(s) | `Unsupported URL scheme "{protocol}" — only http(s) allowed.` |
| 私有 IP 字面量 | `Refusing to fetch private address: "{host}"` |
| 私有主机名 | `Refusing to fetch private host: "{host}"` |
| DNS 安全检查失败 | `Cannot resolve host "{host}" for the fetch safety check: {detail}` |
| 解析到私有地址 | `Refusing to fetch host "{host}": resolves to private address "{address}".` |
| 响应体超限 | `Response body too large: {bytes} bytes exceeds maxBytes ({10485760}).` |
| 重定向过多 | `Too many redirects while fetching "{url}" (limit 10).` |
| 正文提取失败 | `Failed to extract meaningful content from the page. The page may require JavaScript to render.` |

**非功能**：响应体上限 10 MB（可配置覆盖）；重定向 ≤10 跳（每跳重跑安全校验、DNS 结果 pinning 到连接）；SSRF 防护默认开（loopback/RFC1918/link-local/CGNAT/ULA 及 `localhost`/`*.localhost` 拒绝）；默认 UA 为 Chrome 91 样式；无工具自有超时（受取消信号控制）。

**界面呈现**：chip = 输出字节数（`1.2 MB`）；卡片头部 `Used FetchURL (url)`；成功折叠态空、展开显全量（暗色）；错误走通用截断渲染。

### 2.3 WebSearch

- **注册**：名称 `WebSearch`；目录描述 `Search the web for information.`；domain `auth`（provider 注入归认证域，非笔误）；capabilities `['web.search']`；**when**：搜索 provider 已注入（无本地后端，未配置则不激活）。
- **执行元数据**：描述 `Searching: {query}`（>40 字符截断）；展示 `search`；审批刻面 `WebSearch` + query；资源访问 无。
- **Provider 接口**：`search(query, {toolCallId?, signal?}) → {title, url, snippet, date?, siteName?}[]`；后端：云端搜索服务（宿主 API 配置或托管登录态），皆无则不激活。

**面向模型的描述（原文）**：

```
Search the web for information. Use this when you need up-to-date information from the internet.

Each result includes its title, its URL, and a snippet, plus its source site and publication date when available. Results are short summaries, not full pages — when a result looks relevant, call the FetchURL tool on its URL to read the full page content. Fetch only the few URLs you actually need. Prefer specific queries, and refine the query if the results don't contain what you need.

When you rely on a result in your answer, cite its source URL so the user can verify it.
```

**参数契约**：仅 `query`（string，必填）——`The query text to search for.`。

**输出契约**：
- 无结果：`No search results found.`（isError: false）。
- 有结果：逐条渲染（条间 `---` 分隔）：`Title: {title}` / `Site: {siteName}`（有则）/ `Date: {date}`（有则）/ `URL: {url}` / `Snippet: {snippet}`；末尾恒追加 `When you rely on a result in your answer, cite it inline as a markdown link, e.g. [title](url).`。
- 输出 50,000 字符（单行不限长）。

**错误面**（分类，均 isError: true）：

| 条件 | 文案 |
|---|---|
| 取消 | `Search cancelled: {message}` |
| 超时 | `Search timed out: {message}` |
| 认证失败 | `Search failed (authentication): {message}` |
| 网络失败 | `Search failed (network): {message}` |
| 兜底 | `Search failed: {message}` |

**非功能**：无工具自有超时；输出 50,000 字符；本地代码不截断结果条数，上限由远端服务决定。

**界面呈现**：chip = 结果条数（按列表行计数，`N results` / `no results` / `web result`）；卡片头部 `Used WebSearch (query)`；成功折叠态空、展开显全量。

### 2.4 ReadMediaFile

- **注册**：名称 `ReadMediaFile`；目录描述 `Read media content from a file.`；**不走贡献表**——由媒体注册器在模型状态更新时按能力门控注册/撤销：仅当模型 `image_in || video_in` 时注册；模型切换时重建实例。无 when/capabilities（门控即注册本身）。
- **执行元数据**：描述 `Reading media: {path}`；展示 `file_io/read`；审批刻面 `ReadMediaFile` + 路径；资源访问 读单文件；路径守卫同 Read。

**面向模型的描述（原文，模板注入 100 MB，能力行按模型动态附加）**：

```
Read media content from a file.

**Tips:**
- Make sure you follow the description of each tool parameter.
- A `<system>` tag accompanies the media content; it summarizes the mime type, byte size and, for images, the original pixel dimensions, and states how the image was delivered (untouched, downsampled, cropped, or native resolution). When outputting coordinates, give relative coordinates first and compute absolute coordinates from the original image size. After generating or editing media via commands or scripts, read the result back before continuing.
- Large images are downsampled by default when automatic compression can safely fit them within model limits, which can blur fine detail (small text, dense UI). Compute absolute coordinates from the original dimensions reported in the `<system>` block, never by measuring the displayed copy. When the `<system>` tag reports downsampling and you need that detail, call this tool again with the `region` parameter (original-image pixel coordinates) to view a crop at full fidelity, or set `full_resolution` to true when the whole file fits the per-image byte limit. Re-reading the same file without these parameters just reproduces the same downsampled image.
- If automatic compression cannot safely produce an image within model limits, the tool returns an error and does not send the original image. Follow the error: use Bash or an available image-processing tool to create a smaller copy, then read that copy. Do not retry the unchanged file.
- The system will notify you when there is anything wrong when reading the file.
- This tool is a tool that you typically want to use in parallel. Always read multiple files in one response when possible.
- This tool can only read image or video files. To read text files, use the Read tool. To list directories, use `ls` via Bash for a known directory, or Glob for pattern search.
- If the file doesn't exist or path is invalid, an error will be returned.
- The maximum size that can be read is 100MB. An error will be returned if the file is larger than this limit.
- The media content will be returned in a form that you can directly view and understand.

**Capabilities**
- This tool supports image and video files for the current model.    ← 能力行按 image_in/video_in 命中拼接
```

**参数契约**：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `path` | string | ✅ | `Path to an image or video file. Relative paths resolve against the working directory; a path outside the working directory must be absolute. Directories and text files are not supported.` |
| `region` | object `{x,y,width,height}`（int，x/y≥0，w/h≥1） | 可选 | `Images only: view just this rectangle of the image (original-image pixel coordinates). Use after a downsampled full view to inspect fine detail — a region within the size limits is delivered at full fidelity.`（子字段：`Left edge…` / `Top edge…` / `Crop width…` / `Crop height…, in original-image pixels.`） |
| `full_resolution` | boolean | 可选 | `Images only: skip the default downscaling and view at native resolution. Fails with an explicit error when the payload would exceed the per-image byte limit; use region for files that large.` |

**输出契约**（媒体内容块，非纯文本）：
- 成功 = 三段包裹：文本锚 `<image path="{safePath}">` → 媒体块（图片 `image_url` data-URL base64：region 裁剪 / full_resolution 原样 / 默认压缩三种；视频有 uploader 时上传 URL，否则 inline `video_url` data-URL）→ 闭合锚 `</image>`。
- **note 侧信道**（`<system>` 包裹，仅模型可见）：`Read {image|video} file.` `Mime type: {mime}.` `Size: {n} bytes.` 图片附 `Original dimensions: {W}x{H} pixels.`；downsampled 时附 `The attached image was downsampled to {W}x{H} pixels ({mime}, {size}) to fit model limits; fine detail may be lost.` + region 指引；crop 时附 `Showing region (x={x}, y={y}, width={w}, height={h}) of the original image…`；full 时 `Shown at native resolution; no downscaling applied.`；恒有 `If you generate or edit images or videos via commands or scripts, read the result back immediately before continuing.`。

**错误面**（isError: true，输出为纯文本无媒体块）：

| 条件 | 文案 |
|---|---|
| 空路径 | `File path cannot be empty.` |
| 文本文件 | `"{path}" is a text file. Use Read to read text files.` |
| 未知类型 | `"{path}" is not a supported image or video file. Use Read for text files, or Bash or an MCP tool for other binary formats.` |
| 模型无图片能力 | `The current model does not support image input. Dispatch a subagent whose model supports image input (see the "Available models" list in the Agent tool description) to analyze this image.` |
| 模型无视频能力 | 同上（video 版） |
| MIME 不受支持（仅 PNG/JPEG/GIF/WebP） | `"{path}" is an {mime} image, which the provider does not accept. Convert it to JPEG first, then read the converted file. {按操作系统的转换命令提示}` |
| 文件为空 | `"{path}" is empty.` |
| 超 100 MB | `"{path}" is {n} bytes, which exceeds the maximum 100MB for media files.` |
| region/full 用于视频 | `region and full_resolution apply only to image files.` |
| region/full 但图片 >64 MB 解码上限 | `Image is too large to process safely for region or full_resolution ({n} bytes; safe decode limit {67108864} bytes). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy or crop the needed region into a separate image, then call ReadMediaFile on the resulting file.` |
| full_resolution 但 >3.75 MB 每图限额 | `"{path}" is {n} bytes ({size}), over the {3932160}-byte ({3.8 MB}) per-image limit, so full_resolution cannot be honored. Use region to view a crop at full fidelity instead.` |
| 默认压缩后仍超限（fail-closed） | `Image is too large to send safely after compression ({n} bytes; limit {budget} bytes and {2000}px on the longest edge). The original image was not sent to the model. Do not retry the same file unchanged. Use Bash or an available image-processing tool to create a smaller copy within both limits, then call ReadMediaFile on the smaller copy.` |
| region 裁剪失败 | `Cannot read region from "{path}": {error}` |
| 兜底 | `Failed to read {path}: {error.message}` |

**非功能**：文件 ≤100 MB；图片安全解码 ≤64 MB；full_resolution 每图 ≤3.75 MB；默认压缩目标 256 KB + 最长边 2000px（可配置覆盖）；媒体头嗅探 512 字节；视频上传 auth 错误不回落 inline（直接上抛）。

**界面呈现**：chip = `image (image/jpeg, 1.2 MB)` 形态（解析输出信封的 meta）；卡片头部 `Used ReadMediaFile (path)`；成功折叠态空、展开两行 meta（路径 + kind·mime·size）；**永不渲染 base64**。

---

## 第三部分 协作类

### 3.1 Agent

- **注册**：名称 `Agent`；目录描述 `Launch a subagent to handle a task.`；domain `subagent`；无 when、无 capabilities；资源访问 无。
- **执行元数据**：审批刻面 `Agent` + **profile 名** 通配主体（区别于其他工具）；展示 `agent_call`。
- **描述为动态拼接**：基础段 + 后台段（按任务三件套是否全启用二选一）+ 可用子代理类型清单（按 profile catalog 逐条）+ 可用模型清单（curated 别名逐条）。

**基础描述（原文）**：

```
Launch a subagent to handle a task. The subagent runs as a same-process loop instance with its own context and wire file. Delegating also keeps the bulk of intermediate file contents out of your own context — you get a conclusion back instead of a pile of dumps.

Writing the prompt:
- The subagent starts with zero context — it has not seen this conversation. Brief it like a colleague who just walked into the room: state the goal, list what you already know, hand over the specifics.
- Lookups (read this file, run that test): put the exact path or command in the prompt. The subagent should not have to search for things you already know.
- Investigations (figure out X, find why Y): give the question, not prescribed steps — fixed steps become dead weight when the premise is wrong.
- Do not delegate understanding. If the task hinges on a file path or line number, find it yourself first and write it into the prompt.

Usage notes:
- When the task continues earlier work a subagent already did, prefer resuming that agent (pass its `resume` id) over spawning a fresh instance — the resumed agent keeps its prior context.
- A subagent's result is only visible to you, not to the user. When the user needs to see what a subagent produced, summarize the relevant parts yourself in your own reply.
- Subagents use a fixed 2-hour timeout. If one times out, resume the same agent instead of starting over.

When NOT to use Agent: skip delegation for trivial work you can do directly — reading a file whose path you already know, searching a small known set of files, or any task that takes only a step or two. Delegation has a context-handoff cost; it pays off only when the task is substantial enough to outweigh it.

Once a subagent is running, leave that scope to it: do not redo its searches or reads in parallel, and do not abandon it midway and finish the job manually. Both undo the context savings the delegation was meant to buy.
```

**后台段（启用时追加，原文）**：

```
When `run_in_background=true`, the subagent runs detached from this turn. The completion arrives in a later turn as a synthetic user-role message containing its result — you do not need to poll, sleep, or check on its progress. Continue with other work or respond to the user. Never fabricate or predict what the result will say.

Default to a foreground subagent (omit `run_in_background`) when your next step needs its result — foreground hands the result straight back. Reach for `run_in_background=true` only when you have other work to do while it runs and do not need its result to proceed. Never launch in the background and then immediately wait on it (by polling `TaskOutput`, sleeping, or otherwise): that just blocks the turn for no benefit — run it in the foreground instead.
```

**后台段（未启用时追加，原文）**：

```
Background agent execution is disabled for this agent. Do not set `run_in_background=true` — any call that sets it is rejected before the subagent launches. Run every subagent in the foreground and wait for its result.
```

**动态注入结构**：
```
Available agent types (pass via subagent_type):
- <profile.name>: <profile.description> <profile.whenToUse>
  Default model: <defaultModel> (supports vision, video, audio, tool-use, thinking)   ← 能力标签按命中拼接
  Capabilities: <capabilitiesRequired>
  Tools: <有效工具列表> | all | all except <disallowed> | none

Available models (pass via model):
- <alias> — <description>, <ctxKb>k context, supports <caps>
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `prompt` | string | ✅ | — | `Full task prompt for the subagent` |
| `description` | string | ✅ | — | `Short task description (3-5 words) for UI display` |
| `subagent_type` | string | 可选 | `coder`（无 resume 时注入） | `One of the available agent types (see "Available agent types" in this tool description). Defaults to "coder" when omitted.` |
| `resume` | string | 可选 | — | `Optional agent ID to resume instead of creating a new instance. When set, do not also pass subagent_type — the resumed agent keeps its own type, and supplying both is rejected.` |
| `run_in_background` | boolean | 可选 | `false` | `If true, return immediately without waiting for completion. Prefer false unless the task can run independently and there is a clear benefit to not waiting.` |
| `model` | string | 可选 | — | `Model alias to override the subagent default: it names a configured [models] entry or curated alias directly, and leaving it empty inherits the caller's model. Most agents already have a sensible default model — leave empty unless the task difficulty clearly differs from the agent purpose. Valid aliases are listed in the "Available models" section of this tool description when applicable. Ignored when resuming — resumed subagents keep their own model.` |

规范化：`resume` 与 `subagent_type` 同时出现即拒（见错误面）；两者皆缺时注入 `subagent_type="coder"`。

**输出契约**：
- 前台成功（元数据块）：`agent_id` / `actual_subagent_type` / `resolved_model`（有则）/ `status: completed` + 空行 + `[summary]` 段（子代理汇总文本）。
- 前台失败：`status: failed` + `subagent error: <message>`；超时另附 `resume_hint: Continue with Agent(resume="<agentId>", prompt="continue"). Use agent_id only; do not set subagent_type. The subagent retains its prior context; redo any unfinished tool call if its result was lost.`。
- 后台启动：`task_id` / `status: running` / `agent_id` / `actual_subagent_type` / `resolved_model` / `automatic_notification: true` / `description` + `next_step`（勿轮询指引）+ `resume_hint`（恢复说明：参数是 agent_id 而非 task_id；任务丢失/失败/被杀时用 `<notification>` 里的 agent_id 恢复）。
- 内部结构化结果（供任务输出/镜像）：`{ result: string, usage: { input, output, cache_read?, cache_write? } }`。

**错误面**（isError: true）：

| 条件 | 文案 |
|---|---|
| resume 与 subagent_type 并传 | `Cannot set subagent_type when resuming an existing agent. Resume by agent id only.` |
| 后台不可用 | `Background agent execution is not available for this agent because TaskList, TaskOutput, and TaskStop are not enabled.` |
| model 别名未配置 | `Model "<alias>" is not configured.`（+ curated 列表时 ` Valid options: <a, b>.` + ` Leave the model parameter empty to use the default.`） |
| subagent_type 不在允许清单 | `Subagent type "<name>" is not allowed for this agent. Allowed subagent types: <none|a, b>.` |
| 用户打断 | `The subagent was stopped before it finished by user.` |
| 其他中止 | `The subagent was stopped before it finished.`（有 reason 时 + ` Reason: <reason>`） |
| 超时 | `Agent timed out after 2 hours.`（按配置渲染 `2 hours` / `N minutes` / `N seconds` / `N ms`） |
| 后台任务过多 | `Too many background tasks are already running.`（上限为可配置项、缺省不限，仅约束后台任务） |
| 其他启动错误 | `subagent error: <message>` |

**非功能**：超时默认 2h（`[subagent] timeout_ms` / `MIRRI_SUBAGENT_TIMEOUT_MS` 可配置）；后台前提 = 任务三件套全启用；模型解析优先级 显式 model → profile 默认 → 继承调用者；resume 要求目标为本人派生、且当前空闲的子代理。

**界面呈现**：专用子代理卡——头 `✓ Coder Agent Completed (description) · N tools · 1m 2s · 8.4k tok`（状态词 Completed/Failed/Running/Backgrounded/Queued/Starting；标记 `✓`/`✗`/`◐`/spinner；description 截 60 字符）；卡体一行最新活动 + 固定两行内容窗（运行中显示 thinking/活动，结束显示结果尾 2 行或错误全文）；最多展示 4 个工具调用 + `N more tool calls ...`；phase chip `◐ backgrounded` / `✓ done · N tools · 8.4k tok`。同批 ≥2 个 Agent 聚合为组：头 `N agents finished` 或 `Running N agents (x done, y failed, z backgrounded, w running, v waiting, k starting)`，每成员一行 `├─ <AgentName> · desc · N tools · 1m · 8.4k tok · ✓ Completed`；运行中显示 `Press Ctrl+B to run in background`。

### 3.2 AgentSwarm

- **注册**：名称 `AgentSwarm`；目录描述 `Launch multiple subagents from one prompt template.`；domain `swarm`；资源访问 **all**（与一切冲突，串行化保证 swarm 单发）。
- **执行元数据**：展示 `agent_call`（`swarm ({N} subagents)` + description）；审批刻面 `AgentSwarm`。
- **描述动态拼接**：模型目录非空时追加与 Agent 相同结构的 `Available models` 清单段。

**面向模型的描述（原文）**：

```
Launch multiple subagents from one prompt template, existing agent resumes, or both.

Use AgentSwarm when many subagents should run the same kind of task over different inputs. The placeholder is exactly `{{item}}`. For example, with `prompt_template` set to `Review {{item}} for likely regressions.` and `items` set to `["src/a.ts", "src/b.ts"]`, AgentSwarm launches two new subagents with those two concrete prompts. For a few differently-shaped tasks, make separate `Agent` calls in one message instead.

Use `resume_agent_ids` to continue subagents that already exist from earlier work, such as ones that failed or timed out: map each agent id to the prompt for that resumed subagent (usually `continue` if no extra information is needed). You may combine `resume_agent_ids` with `items` in the same call to resume existing subagents and launch new ones. Do not duplicate resumed work in `items`.

Each of these is enforced — a violation is rejected before any subagent starts: provide at least 2 `items` unless you pass `resume_agent_ids`; whenever `items` are present, `prompt_template` is required and must contain `{{item}}`; and the filled-in prompts must be distinct (two items that expand to the same prompt are rejected).

Use enough subagents to keep the work focused and parallel. AgentSwarm supports up to 128 subagents, and launches are queued automatically, so it is safe to split large tasks into many clear, independent items.

If `AgentSwarm` is called, that call must be the only tool call in the response.
```

**参数契约**（strict）：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `description` | string（trim ≥1） | ✅ | `Short description for the whole swarm.` |
| `subagent_type` | string（trim ≥1） | 可选，默认 `coder` | `Subagent type used for every new subagent spawned from items; defaults to coder when omitted. Resumed subagents always keep their original type, so passing subagent_type together with resume_agent_ids is allowed — it only affects the item-based spawns.` |
| `prompt_template` | string（trim ≥1） | 可选（有 items 时必填） | `Prompt template for each subagent. The {{item}} placeholder is replaced with each item value.` |
| `items` | string[]（每项 trim ≥1，≤128） | 可选（无 resume 时 ≥2） | `Values used to fill {{item}}. Each item launches one new subagent.` |
| `resume_agent_ids` | record<string,string>（键值 trim ≥1） | 可选 | `Map of existing subagent agent_id to the prompt used to resume that subagent. These resumed subagents are launched before new item-based subagents.` |
| `model` | string | 可选 | `Model alias applied to all item-spawned subagents: it names a configured [models] entry or curated alias directly, and leaving it empty inherits the caller's model. Most agents already have a sensible default — leave empty unless the task difficulty clearly differs. Valid aliases are listed in the "Available models" section of this tool description when applicable. Resumed subagents always keep their own model.` |

**输出契约**（聚合 XML）：

```
<agent_swarm_result>
<summary>completed: 3, failed: 1</summary>
<resume_hint>Call AgentSwarm with resume_agent_ids using the agent_id values in this result to continue unfinished work.</resume_hint>   ← 存在未完成且有 agent_id 时
<subagent mode="resume" agent_id="..." item="..." state="started|not_started" outcome="completed|failed|aborted">result body</subagent>
...
</agent_swarm_result>
```

**错误面**（启动前校验，全部拒绝且不启动任何子代理）：

| 条件 | 文案 |
|---|---|
| items < 2 且无 resume | `AgentSwarm requires at least 2 items unless resume_agent_ids is provided.` |
| 总量 > 128 | `AgentSwarm supports at most 128 subagents.` |
| 有 items 无 template | `prompt_template is required when items are provided.` |
| template 无占位符 | `prompt_template must include the {{item}} placeholder.` |
| 展开出重复 prompt | `Duplicate subagent prompts from items <i> and <j>. AgentSwarm requires distinct subagents.` |
| subagent_type 不允许 | 同 Agent 的 allowlist 文案 |
| model 未配置 | 同 Agent 的 model 文案 |

（注：subagent_type 与 model 两项校验仅在 `items` 非空时执行；纯 `resume_agent_ids` 调用跳过这两项。）

**非功能**：上限 128（schema 与运行时双重限制）；子任务描述 `{swarm 描述} #{index} ({type|resume})`；resume 条目先于 spawn；固定前台（结果聚合返回）；每子代理超时同 Agent。描述声明的"该调用必须是响应中唯一调用"由资源声明与一切冲突的串行化调度保证，非预检拒绝项。

**界面呈现**：运行期专用面板 `Agent Swarm ─ <description>`，状态标签 Orchestrating.../Prompting.../Working.../Completed./Failed./Aborted./Cancelled./Queued.../Rate limited...，resume 条目带 `(resumed)`；网格单元格 3 位序号 + spinner；结果摘要行 `Agent swarm: ✓ N completed · ✗ N failed · ⊘ N aborted`。

### 3.3 AskUserQuestion

- **注册**：名称 `AskUserQuestion`；目录描述 `Ask the user questions with structured options during execution.`；domain `questionTools`；无 when、无 capabilities；资源访问 无。
- **执行元数据**：审批刻面 `AskUserQuestion`。

**面向模型的描述（原文 + 后台变体段直接拼接）**：

```
Use this tool when you need to ask the user questions with structured options during execution. This allows you to:
1. Collect user preferences or requirements before proceeding
2. Resolve ambiguous or underspecified instructions
3. Let the user decide between implementation approaches as you work
4. Present concrete options when multiple valid directions exist

**When NOT to use:**
- When you can infer the answer from context — be decisive and proceed
- Trivial decisions that don't materially affect the outcome

Overusing this tool interrupts the user's flow. Only use it when the user's input genuinely changes your next action.

**Usage notes:**
- Users always have an "Other" option for custom input — don't create one yourself
- Use multi_select to allow multiple answers to be selected for a question
- Keep option labels concise (1-5 words), use descriptions for trade-offs and details
- Each question should have 2-4 meaningful, distinct options
- Question texts must be unique across the call, and option labels must be unique within each question
- You can ask 1-4 questions at a time; group related questions to minimize interruptions
- If you recommend a specific option, list it first and append "(Recommended)" to its label
- The result is JSON with an `answers` object keyed by question text; each value is the chosen option's label (comma-separated labels for multi_select, or the user's own words if they picked "Other"); if `answers` is empty and a `note` says the user dismissed it, they explicitly declined to answer — the engine ends the turn and returns control to the user; do NOT proceed on assumptions, do NOT treat this as selecting the recommended option, and do NOT re-ask the same question
- Set background=true when you can keep working without the answer. This starts a background question task and returns a task_id immediately. The answer arrives automatically in a later turn — you do not need to poll, sleep, or check on it. Continue with other work; never fabricate or predict the answer.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `questions` | array（1–4） | ✅ | — | `The questions to ask the user (1-4 questions).` |
| └ `question` | string（≥1） | ✅ | — | `A specific, actionable question. End with '?'.` |
| └ `header` | string | 可选 | `''` | `Short category tag (max 12 chars, e.g. 'Auth', 'Style').` |
| └ `options` | array（2–4） | ✅ | — | `2-4 meaningful, distinct options. Do NOT include an 'Other' option — the system adds one automatically.` |
| ── `label` | string（≥1） | ✅ | — | `Concise display text (1-5 words). If recommended, append '(Recommended)'.` |
| ── `description` | string | 可选 | `''` | `Brief explanation of trade-offs or implications.` |
| └ `multi_select` | boolean | 可选 | `false` | `Whether the user can select multiple options.` |
| `background` | boolean | 可选 | `false` | `Set true to ask in the background and return immediately with a background task_id; you are notified automatically when the user answers — do not poll with TaskOutput while the question is pending.` |

**输出契约**：
- 已作答：`JSON.stringify({ answers })` —— 以问题文本为 key、所选 label（或 "Other" 原文；多选逗号分隔）为 value。
- 用户放弃（isError: false）：`{"answers":{},"note":"User dismissed the question without answering."}`。
- 后台变体（任务 id 前缀 `question`）：`task_id` / `description`（首个问题文本，>1 时 ` (+N more)`）/ `status` / `automatic_notification: true` / `next_step: Continue your current work; the answer will arrive automatically when the user responds.` / `next_step: Use TaskOutput with this task_id for a non-blocking status/answer snapshot.` / `next_step: Use TaskStop only if the question should be cancelled.` / `human_shell_hint: The pending question is also visible in /tasks.`。

**错误面**：

| 条件 | 文案 |
|---|---|
| 重复问题文本 | `Invalid questions: duplicate question text "<text>". Question texts must be unique across questions, and option labels must be unique within each question. Rephrase the duplicates and call the tool again.` |
| 题内重复选项 | `Invalid questions: duplicate option label "<label>" in question "<question>". …同上。` |
| 客户端不支持交互提问 | `The connected client does not support interactive questions. Do NOT call this tool again. Ask the user directly in your text response instead.` |
| 其他未知异常 | 静默转为"放弃"结果（isError: false） |
| 后台任务注册失败 | 注册错误的 `error.message`（isError: true；**无** Bash/Agent 式的后台不可用前置文案——该工具不做此前置检查） |

**非功能**：1–4 问 × 2–4 选项；"Other" 由系统自动补；无超时（挂起直到作答或取消）。

**界面呈现**：卡片头 `Waiting for your input` → `Collected your answers` / `Could not collect your input`（后台变体 `Starting background question` / `Started background question`）；结果体逐题两行 `  Q  <question>` + `  →  <answer>`（accent 箭头；无 answers 灰字显示 note）；提问对话框 tab 式、数字键直达、多选 `[✓]`/`[ ]`、每选项下缩进渲染 description、超屏 `showing X-Y of Z`、"Other" 自由输入、提交页 `Ready to submit your answers?` + 未答全警告。

### 3.4 Skill

- **注册**：名称 `Skill`；目录描述 `Invoke a registered skill from the current skill listing.`；domain `skill`；无 when、无 capabilities。
- **执行元数据**：展示 `skill_call`（skill_name + args）；审批刻面 `Skill`（glob 匹配 skill 名）。

**面向模型的描述（原文）**：

```
Invoke a registered skill from the current skill listing. BLOCKING REQUIREMENT: when a skill from the listing matches the user's request, you MUST call this tool (not free-form text). Do not re-invoke a skill to repeat work already done: if a `<kimi-skill-loaded>` block for it with the same `args` is already present in the conversation, follow those instructions directly instead of calling the tool again. Do call the tool again when you need the skill with different arguments — the loaded block was expanded with the earlier `args` and will not reflect new inputs.
```

**参数契约**：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `skill` | string | ✅ | `The exact name of the skill to invoke, spelled as it appears in the current skill listing (e.g. "commit", "pdf").` |
| `args` | string | 可选 | `Optional argument string for the skill, written like a command line (e.g. `-m "fix bug"`, `123`, a file path). It is split on whitespace (quotes group a token) and expanded into the skill's placeholders ($NAME, $1, $ARGUMENTS); if the skill body has no placeholders, the whole string is still appended as a trailing `ARGUMENTS:` line. Omit it only when there is nothing to pass.` |

**名称解析**：先按清单裸名精确解析（不区分大小写）；未命中且名称含 `:` 时按「插件id:技能名」解析插件技能——两种形态等效，激活呈现回显正式裸名，错误路径回显原始输入名（机制见 skills 域 ADR-001）。

**输出契约**：成功 = `Skill "<skill.name>" loaded inline. Follow its instructions.` + **旁路投递**——向当前回合注入一条 user 角色消息（技能全文渲染，含技能名/参数/来源/触发方式），即"成功是导流而非文本结果"（投递机制归会话管理域 G4）。

**错误面**（isError: true）：

| 条件 | 文案 |
|---|---|
| 嵌套深度 ≥3 | `Nested skill invocation "<skill>" exceeded the maximum depth of 3 — refusing to recurse further.` |
| 技能不存在 | `Skill "<args.skill>" not found in the current skill listing.` |
| 禁止模型调用 | `Skill "<args.skill>" can only be triggered by the user (model invocation is disabled).` |
| 非 inline 类型 | `Skill "<skill.name>" is not an inline skill and cannot be invoked by the model in v1.` |

（"in v1" 字样为现行文案的历史版本标记——Rust 复刻时建议中性化为版本无关措辞，属刻意改进须披露。）

**非功能**：嵌套深度上限 3（Skill→Skill 递归，调用时深度达 3 即拒绝——即最多两层嵌套）；需技能为 inline 类型且未禁用模型调用。

**界面呈现**：展示 `skill_call`；专用技能激活渲染器；描述行 `Invoke skill <args.skill>`。

### 3.5 select_tools

- **注册**：名称 `select_tools`（全系统唯一 snake_case 名工具）；目录描述 `Select which tools are available to the agent.`；domain `toolSelect`；审批刻面 `select_tools`。
- **披露门**：仅当渐进披露启用（模型能力 `dynamically_loaded_tools` + 实验旗标，默认关）时出现在模型视野（机制见 architecture §7）。

**面向模型的描述（原文）**：

```
Load one or more tools by name so you can call them. All available tool names are listed in the <tools_added>/<tools_removed> announcements in the system context — fold them in order to get the current list. Pass the exact name(s) you need; their full definitions become available immediately, so you can call them directly in your next tool call.
```

**参数契约**（strict）：仅 `names`（string[]，≥1，必填）——`Exact tool names to load, taken from the latest announced tool list.`。

**输出契约**（三段反馈行，空格连接）：
- `Loaded: <toLoad.join(', ')>`（排序后注入完整工具定义到系统上下文）；
- `Already available: <alreadyAvailable.join(', ')>`（已在集合，去重后视为已可用）；
- `Unknown tool: <name>. Pick from the latest announced tools list.`（逐名一行）。
- isError 当且仅当 toLoad 与 alreadyAvailable 均为空（全 unknown）；混合时 isError: false。

**错误面**：

| 条件 | 文案 |
|---|---|
| 服务未启用 | `select_tools is not available for the current model.` |
| 全 unknown | 同输出契约（isError: true） |

**同域关联拦截文案**（非本工具输出，但属披露面契约）：

| 条件 | 文案 |
|---|---|
| 已公告未加载的工具被调用 | `Tool "<name>" is available but not loaded. Call select_tools with ["<name>"] first, then call the tool.` |
| 已加载但不再活跃 | `Tool "<name>" was loaded but is no longer active. Ask the user to enable it before calling it again.` |
| 已加载但 MCP 断连 | `Tool "<name>" was loaded but its MCP server is currently disconnected. It may become available again when the server reconnects; do not retry immediately.` |
| 已加载但不再注册 | `Tool "<name>" was loaded but is no longer registered. Do not retry it unless it becomes available again.` |

**非功能**："可动态加载"判定 = MCP 来源或 deferred 披露；压缩后未落地的加载状态清空；names 无数量上限（待复核，见 §6）。

**界面呈现**：通用卡片 `Used select_tools (a, b)`；无专用渲染器。

---

## 第四部分 任务 / 目标 / 计划 / 定时

### 4.1 TaskList

- **注册**：名称 `TaskList`；目录描述 `List background tasks and their current status.`；domain `agentTask`；无 when（profile 差异见 §0.1）；审批刻面 `TaskList`。

**面向模型的描述（原文）**：

```
List background tasks and their current status.

Use this tool to discover which background tasks exist and where each one
stands. It is the entry point for inspecting background work: it returns a
task ID, status, and description for every task it reports, plus the command,
PID, and (once finished) exit code for shell tasks, and a stop reason for any
task that ended early.

Guidelines:

- After a context compaction, or whenever you are unsure which background
  tasks are running or what their task IDs are, call this tool to
  re-enumerate them instead of guessing a task ID.
- Prefer the default `active_only=true`, which lists only non-terminal tasks.
  Pass `active_only=false` only when you specifically need to see tasks that
  have already finished. With `active_only=false` the result may also include
  `lost` tasks — tasks left over from a previous process that can no longer be
  inspected or controlled; treat them as already terminated.
- `limit` caps how many tasks are returned. It accepts a value between 1 and
  100 and defaults to 20 when omitted.
- This tool only lists tasks; it does not return their output. Use it first
  to locate the task ID you need, then call `TaskOutput` with that ID to read
  the task's output and details.
- This tool is read-only and does not change any state, so it is always safe
  to call, including in plan mode.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `active_only` | boolean | 可选 | `true` | `Whether to list only non-terminal background tasks.` |
| `limit` | integer（1–100） | 可选 | `20` | `Maximum number of tasks to return.` |

**输出契约**：空表 `active_background_tasks: 0\nNo background tasks found.`（全量时 header `background_tasks: N`）；非空 header 后逐任务 `key: value` 行（snake_case 序列化），记录间 `---` 分隔。任务通用字段：`task_id` / `description` / `status`（`running|completed|failed|timed_out|killed|lost`）/ `detached` / `started_at` / `ended_at` / `stop_reason` / `terminal_notification_suppressed` / `timeout_ms`；按 kind 追加——process：`kind: process` `command` `pid` `exit_code`；agent：`kind: agent` `agent_id` `subagent_type` `model`；question：`kind: question` `question_count` `tool_call_id`。

**错误面**：无（只读）。

**非功能**：limit 1–100 默认 20；read-only（plan mode 安全）。

### 4.2 TaskOutput

- **注册**：名称 `TaskOutput`；目录描述 `Retrieve a snapshot of a running or completed background task.`；domain `agentTask`；审批刻面 `TaskOutput`。

**面向模型的描述（原文）**：

```
Retrieve a snapshot of a running or completed background task.

Use this after `Bash(run_in_background=true)`, `Agent(run_in_background=true)`, or `AskUserQuestion(background=true)` to check progress, or to read the output of a task that has already completed.

Guidelines:
- Prefer relying on automatic completion notifications. Use this tool only when you need task output before the automatic notification arrives.
- This tool is always non-blocking: it returns the current status/output snapshot immediately and never waits for the task to finish.
- Do not use TaskOutput to wait for a result you need before continuing — if your next step depends on the task's result, run that task in the foreground instead. TaskOutput is for a deliberate progress check you will act on without blocking, not a way to sit and wait on a background task you just launched.
- This tool returns structured task metadata, a fixed-size output preview, and an output_path for the full log.
- For a terminal task, the metadata also explains why it ended. A shell command that runs to completion reports `status: completed` on a zero exit, or `status: failed` with its non-zero `exit_code` — judge that failure from the `exit_code`, because a plain command failure carries no `stop_reason` and no `terminal_reason`. `terminal_reason` is a categorical label emitted only when the end is not an ordinary exit: `timed_out` when the deadline aborted it, `stopped` when it was explicitly stopped, or `failed` when it errored without producing an exit code; the `stopped` and `failed` cases also carry a human-readable `stop_reason`. A task that finished on its own with a clean exit carries neither `stop_reason` nor `terminal_reason`.
- The full, never-truncated log is always available at output_path; use the `Read` tool with that path to page through it, whether or not the preview was truncated.
- This tool works with the generic background task system and should remain the primary read path for future task types, not just bash.
```

**参数契约**：仅 `task_id`（string，必填）——`The background task ID to inspect.`。

**输出契约**（`key: value` 行序列）：`retrieval_status`（终态 `success` / 非终态 `not_ready`）→ 任务元数据全字段（同 TaskList）→ `output_path` → `terminal_reason`（`timed_out|stopped|failed`，普通退出缺省）→ `output_size_bytes` / `output_preview_bytes` / `output_truncated` / `full_output_available` / `full_output_tool`（`Read`）→ `full_output_hint`（截断时 `Only the last {32768} bytes are shown above. Use the Read tool with the output_path to page through the full log (parameters: path, line_offset, n_lines; read about {300} lines per page).`；未截断 `The preview above is the complete output. …`）→ 空行 → 截断提示 `[Truncated. Full output: {output_path}]` / 无落盘 `[Truncated. No persisted full log is available for this task.]` → `[output]` 行 + 预览（无输出 `[no output available]`）。

**错误面**：任务不存在 `Task not found: {task_id}`（isError: true）。

**非功能**：预览 = 输出**尾部** 32 KiB；非阻塞；完整日志永在 `output_path` 落盘。

### 4.3 TaskStop

- **注册**：名称 `TaskStop`；目录描述 `Stop a running background task.`；domain `agentTask`；审批刻面 `TaskStop`。

**面向模型的描述（原文）**：

```
Stop a running background task.

Only use this when a task must genuinely be cancelled — for a task that is
finishing normally, wait for its completion notification or inspect it with
`TaskOutput` instead of stopping it.

Guidelines:
- This is a general-purpose stop capability for any background task. It is not
  a bash-specific kill.
- Stopping a task is destructive: it may leave partial side effects behind.
  Use it with care.
- If the task has already finished, this tool simply returns its current
  status.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `task_id` | string | ✅ | — | `The background task ID to stop.` |
| `reason` | string | 可选 | `'Stopped by TaskStop'`（空亦回落） | `Short reason recorded when the task is stopped.` |

**输出契约**：终态任务（不操作）：`task_id` / `status` / `reason`（记录的 stop_reason 为空时 `Task already in terminal state`）。存活任务：先**抑制终态通知**（使本结果成为模型唯一应答），再停止：`task_id` / `status` / `reason`。

**错误面**：

| 条件 | 文案 |
|---|---|
| 任务不存在 | `Task not found: {task_id}` |
| 停止失败 | `Failed to stop task: {task_id}` |

**非功能**：停止为破坏性操作（可能留下部分副作用）；终态任务为只读返回。

**界面呈现（任务三件套共用）**：无专用结果渲染器（回落通用截断渲染：前 3 行 + `... (N more lines, ctrl+o to expand)`，错误 error 色）；无 chip。

### 4.4 TodoList

- **注册**：名称 `TodoList`；目录描述 `Maintain a structured TODO list as you work.`；domain `todo`；审批刻面 `TodoList`；无 when。
- **执行描述**随形态：`Reading todo list` / `Clearing todo list` / `Updating todo list`。

**面向模型的描述（原文）**：

```
Use this tool to maintain a structured TODO list as you work through a multi-step task. Use it proactively and often when progress tracking helps the current work. This is especially useful in long-running investigations and implementation tasks with several tool calls; in plan mode, write the plan to the plan file rather than tracking it here.

**When to use:**
- Multi-step tasks that span several tool calls
- Tracking investigation progress across a large codebase search
- Planning a sequence of edits before making them
- After receiving new multi-step instructions, capture the requirements as todos
- Before starting a tracked task, mark exactly one item as `in_progress`
- Immediately after finishing a tracked task, mark it `done`; do not batch completions at the end

**When NOT to use:**
- Single-shot answers that complete in one or two tool calls
- Trivial requests where tracking adds no clarity
- Purely conversational or informational replies

**Avoid churn:**
- Do not re-call this tool when nothing meaningful has changed since the last call — update the list only after real progress.
- When unsure of the current state, call query mode first (omit `todos`) to check the list before deciding what to update.
- If no available tool can move any task forward, tell the user where you are stuck instead of repeatedly re-ordering the same todos.

**How to use:**
- Call with `todos: [...]` to replace the full list. Statuses: pending / in_progress / done.
- Call with no `todos` argument to retrieve the current list without changing it.
- Call with `todos: []` to clear the list.
- Keep titles short and actionable (e.g. "Read session-control.ts", "Add planMode flag to TurnManager").
- Update statuses as you make progress.
- When work is underway, keep exactly one task `in_progress`.
- Only mark a task `done` when it is fully accomplished.
- Never mark a task `done` if tests are failing, implementation is partial, unresolved errors remain, or required files/dependencies could not be found.
- If you encounter a blocker, keep the blocked task `in_progress` or add a new pending task describing what must be resolved.
```

**参数契约**：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `todos` | array<{title, status}> | 可选 | `The updated todo list. Omit to read the current todo list without making changes. Pass an empty array to clear the list.` |

子结构：`title`（string ≥1，`Short, actionable title for the todo.`）；`status`（enum `pending|in_progress|done`，`Current status of the todo.`）。

三形态：`{todos:[…]}` 整体替换 / `{todos:[]}` 清空 / `{}` 查询。

**输出契约**：查询 `Current todo list:` + 逐行 `  [pending] {title}` 等（空表 `Todo list is empty.`）；写入后空表 `Todo list cleared.`，非空 `Todo list updated.\n{清单渲染}\n\n{reminder}`，reminder 原文 `Ensure that you continue to use the todo list to track progress. Mark tasks done immediately after finishing them, and keep exactly one item in_progress while work is underway.`。

**错误面**：无显式运行时错误。

**非功能**：清单无数量上限；会话共享存储（跨轮持久化、重放水合）。

**界面呈现**：非工具卡——输入区上方**常驻面板**：标记 in_progress=`●`（主色）/ done=`✓`（success 色带删除线）/ pending=`○`（dim）；折叠上限 5 条（优先全部 in_progress、再补最早 pending、保留最新 done 一条），溢出 `… +{N} more ({x} done · {y} in progress · {z} pending) · ctrl+t to expand`；全 done 自动隐藏；工具卡片只留标题。

### 4.5 Goal 四件套（CreateGoal / GetGoal / UpdateGoal / SetGoalBudget）

**公共约定**：
- 注册：四件套目录描述依次为 `Create a durable, structured goal that the runtime will pursue.` / `Read the current goal: its objective, completion criterion, and status.` / `Set the status of the current goal.` / `Set a hard budget limit for the current goal.`；domain `goal`；审批刻面各为工具名。
- 全部仅 main agent 可用（生效谓词限定 main agent；子代理不可见）。
- 快照序列化剥离 `goalId`；输出统一 `{ "goal": {…}|null }` JSON（两格缩进）。快照字段：`objective` / `completionCriterion?` / `status`（`active|paused|blocked|complete`）/ `turnsUsed` / `tokensUsed` / `wallClockMs` / `budget`（`tokenBudget/turnBudget/wallClockBudgetMs` 各 number|null、`remaining*` 各 number|null、`*BudgetReached` 与 `overBudget` 均 boolean）/ `terminalReason?`。
- **文案式失败**（isError: false 的业务拒绝，如 "Goal not completed: no active goal."）与工具性失败（isError: true）并存——Rust 对齐时需区分两种形态。

#### CreateGoal

**面向模型的描述（原文）**：

```
Create a durable, structured goal that the runtime will pursue across multiple turns.

Call `CreateGoal` only when:

- the user explicitly asks you to start a goal or work autonomously toward an outcome, or
- a host goal-intake prompt asks you to create one.

Do NOT create a goal for greetings, ordinary questions, or vague requests that lack a
verifiable completion condition. A goal needs a checkable end state.

When the request is vague, ask the user for the missing completion criterion before creating
the goal. If the user clearly insists after you warn them that the wording is vague or risky,
respect that and create the goal.

Include a `completionCriterion` when the user provides one, or when it can be stated without
inventing new requirements. Keep `objective` concise; reference long task descriptions by file
path rather than pasting them.

Creating a goal fails if one already exists, so use `replace: true` only when the user explicitly
wants to abandon the current goal and start a new one.
```

**参数契约**（strict）：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `objective` | string（≥1） | ✅ | `The objective to pursue. Must have a verifiable end state.` |
| `completionCriterion` | string | 可选 | `How to verify the goal is complete. Include when the user provides one.` |
| `replace` | boolean | 可选 | `Replace an existing active, paused, or blocked goal instead of failing.` |

**输出契约**：成功 → goal 快照 JSON；非 auto 权限模式附 `goal_start` 结构化展示（objective/completionCriterion/mode）。
**错误面**：执行间 goal 变化 `Goal not created: the current goal changed.`（isError: false）。

#### GetGoal

**面向模型的描述（原文）**：

```
Read the current goal: its objective, completion criterion, status, and budgets (turns, tokens,
time, and how much of each remains). When the goal has stopped, it also reports the terminal reason.

Use `GetGoal` before deciding whether to continue working, report completion, report a blocker,
or respect a pause. It returns `{ "goal": null }` when there is no current goal.
```

**参数契约**：无参数（strict 空对象）。
**输出契约**：goal 快照 JSON 或 `{ "goal": null }`。
**错误面**：无。

#### UpdateGoal

**面向模型的描述（原文）**：

```
Set the status of the current goal. This is how you resume, complete, or block an autonomous goal.

- `active` — resume a paused or blocked goal when the user explicitly asks you to work on that goal.
- `complete` — the objective is satisfied and any stated validation has passed. The goal ends and a completion summary is recorded. Before using this, verify the current state against the actual objective and every explicit requirement. Treat weak or indirect evidence as not complete. Do not use `complete` merely because a budget is nearly exhausted or you want to stop.
- `blocked` — a genuine impasse prevents useful progress: an external condition, required user input, missing credentials or permissions, a persistent technical failure, or an impossible, unsafe, or contradictory objective. For non-terminal blockers, do not use `blocked` the first time you hit the blocker. The same blocking condition must repeat for at least 3 consecutive goal turns before you call `blocked`, counting the original/user-triggered turn and automatic continuations. If a previously blocked goal is resumed, treat the resumed run as a fresh blocked audit. If the objective itself is impossible, unsafe, or contradictory, call `blocked` in the same turn instead of running more goal turns. Do not use `blocked` because the work is large, hard, slow, uncertain, incomplete, still needs validation, would benefit from clarification, or needs more goal turns. Once the 3-turn threshold is met and you cannot make meaningful progress without user input or an external-state change, call `blocked` instead of leaving the goal active.

Most active goal turns should not call this tool. If you complete one useful slice of work and material work remains, end the turn normally without calling UpdateGoal; the runtime will prompt you to continue in the next goal turn. Call `complete` only when all required work is done, any stated validation has passed, and there is no useful next action. Do not call `complete` after only producing a plan, summary, first pass, or partial result. Call `blocked` only after the blocked audit threshold is met. If you call `blocked`, you will be prompted to explain the blocker in your next message. Setting the status is the machine-readable signal; the completion summary or blocker explanation is yours to write in the following message.
```

**参数契约**（strict）：仅 `status`（enum `active|complete|blocked`，必填）——`The lifecycle status to set for the current goal. Use \`blocked\` for impossible, unsafe, or contradictory objectives, or after the same non-terminal blocking condition repeats for at least 3 consecutive goal turns.`（schema 无 reason 字段——模型在后续消息中自行解释）。

**输出契约**：
- `active` → `Goal resumed.`
- `complete` → 完成摘要提示词（`Goal completed successfully{reason}.\nWorked {N} turns over {elapsed}, using {tokens} tokens.\n\nWrite a concise final message for the user. State that the goal is complete, summarize the main work completed, and mention any validation you ran. Do not call more goal tools.`；elapsed 格式 `Xs`/`XmYYs`/`XhYYm`、tokens `N`/`N.Nk`/`N.NM`）+ **stopTurn: true**。
- `blocked` → `Goal blocked.\nWorked {N} turns over {elapsed}, using {tokens} tokens.\n\nWrite a concise final message for the user. State that the goal is blocked, explain the concrete blocker, and say what input or change is needed before work can continue. Do not call more goal tools.` + stopTurn: true。
- **stopBatchAfterThis**：status 非 active 且当前 goal 活跃时。

**错误面**：非法 status（防御）`Invalid goal status. Use \`active\`, \`complete\`, or \`blocked\`.`（isError: true）；无当前 goal / goal 变化：`Goal not {resumed|completed|blocked}: {no current goal|no active goal|the current goal changed}.`（isError: false）。

#### SetGoalBudget

**面向模型的描述（原文）**：

```
Set a hard budget limit for the current goal.

Use this only when the user clearly gives a runtime limit, such as:

- "stop after 20 turns"
- "use no more than 500k tokens"
- "finish within 30 minutes"

Do not invent limits. Do not call this for vague wording such as "spend some time" or
"try to be quick".

If the user gives a compound time, convert it to one supported unit before calling this tool.
For example, "2 hours and 3 minutes" can be set as `value: 123, unit: "minutes"`.

A time budget must be between 1 second and 24 hours — the tool rejects anything shorter or
longer, telling the user it is not a reasonable goal budget. Turn and token budgets are not
bounded this way; they must be positive and are rounded to the nearest whole number (minimum 1).

Supported units:

- `turns`
- `tokens`
- `milliseconds`
- `seconds`
- `minutes`
- `hours`
```

**参数契约**（strict）：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `value` | number（>0） | ✅ | `The positive numeric budget value.` |
| `unit` | enum `turns\|tokens\|milliseconds\|seconds\|minutes\|hours` | ✅ | （无描述） |

**归一化**：turns/tokens → `max(1, round(value))`；时间单位不取整。**输出契约**：正常 `Goal budget set: {value} {unit}.`（单数化 `1 turn`）；已超新预算 `Goal budget set: {value} {unit}. The goal has already reached this budget and will stop now.` + stopTurn: true；解析期判定 stopBatchAfterThis（将超预算时）。

**错误面**（isError: false 文案式）：无当前 goal `Goal budget not set: no current goal.`；goal 变化 `Goal budget not set: the current goal changed.`；时间预算超 1s–24h `Goal budget not set: {value} {unit} is not a reasonable goal budget.`。

**界面呈现（goal 族共用）**：头部动词对（CreateGoal `Starting goal`/`Started goal`/`Could not start goal` 等，附参数后缀）；chip 仅 CreateGoal/GetGoal（解析 goal.status，无 goal → `no goal`）；快照卡 `  Goal {status}: {objective}`（单行截 96 字符）+ `{N} turns · {tokens} tokens · {elapsed}`；`goal: null` → `  No current goal.`；SetGoalBudget/UpdateGoal 无结果卡（仅头部）。

### 4.6 Plan 两件套（EnterPlanMode / ExitPlanMode）

#### EnterPlanMode

- **注册**：名称 `EnterPlanMode`；目录描述 `Enter plan mode for read-only investigation.`；domain `plan`；审批刻面 `EnterPlanMode`。

**面向模型的描述（原文）**：

```
Use this tool proactively when you're about to start a non-trivial implementation task.
Getting user sign-off on your approach via ExitPlanMode before writing code prevents wasted effort.

Use it when ANY of these conditions apply:

1. New Feature Implementation - e.g. "Add a caching layer to the API"
2. Multiple Valid Approaches - e.g. "Optimize database queries" (indexing vs rewrite vs caching)
3. Code Modifications - e.g. "Refactor auth module to support OAuth"
4. Architectural Decisions - e.g. "Add WebSocket support"
5. Multi-File Changes - involves more than 2-3 files
6. Unclear Requirements - need exploration to understand scope
7. User Preferences Matter - if user input would materially change the implementation approach, use EnterPlanMode to structure the decision

Permission mode notes:
- EnterPlanMode enters plan mode automatically without an approval prompt in all permission modes.
- In yolo and manual modes, ExitPlanMode still presents the plan to the user for approval.
- In auto permission mode, do not use AskUserQuestion; make the best decision from available context.
- In auto permission mode, ExitPlanMode exits plan mode without asking the user.
- Use EnterPlanMode only when planning itself adds value.

When NOT to use:
- Single-line or few-line fixes (typos, obvious bugs, small tweaks)
- User gave very specific, detailed instructions
- Pure research/exploration tasks

Once you are in plan mode, a reminder walks you through the workflow (explore → design → write the plan file → `ExitPlanMode`) and enforces read-only access. For non-trivial tasks where you are unsure of the codebase structure or relevant code paths, use `Agent(subagent_type="explore")` to investigate first when the `Agent` tool is available.
```

**参数契约**：无参数（strict 空对象）。
**输出契约**：成功消息按宿主是否提供 plan file path 二选一——无 path：`Plan mode is now active. Your workflow:\n\n1. Use read-only tools (Read, Grep, Glob) to investigate the codebase. Use Bash only when needed.\n2. Design a concrete, step-by-step plan.\n3. Wait for the host to provide a plan file path before calling ExitPlanMode.\n\nDo NOT use Write or Edit while plan mode is active in this host; no plan file path is available.\nUse Bash only when needed; Bash follows the normal permission mode and rules.`；有 path：第 3 步改为写计划文件、第 4 步调用 ExitPlanMode，尾行 `Do NOT edit files other than the plan file while plan mode is active.`。进入 plan mode 不需任何权限审批（所有模式自动）。
**错误面**：已在 plan mode `Plan mode is already active. Use ExitPlanMode when the plan is ready.`（isError: true）；进入失败 `Failed to enter plan mode: {message}`。

#### ExitPlanMode

- **注册**：名称 `ExitPlanMode`；目录描述 `Exit plan mode and present a plan for approval.`；domain `plan`；审批刻面 `ExitPlanMode`。

**面向模型的描述（原文）**：

```
Use this tool when you are in plan mode and have finished writing your plan to the plan file and are ready for user approval.

## How This Tool Works
- You should have already written your plan to the plan file specified in the plan mode reminder.
- This tool does NOT take the plan content as a parameter - it reads the plan from the file you wrote.
- The user will see the contents of your plan file when they review it. In auto permission mode, the tool reads the file and exits plan mode without asking the user.

## When to Use
Only use this tool for tasks that require planning implementation steps. For research tasks (searching files, reading code, understanding the codebase), do NOT use this tool.

## What a good plan contains
List specific, verifiable steps grounded in the actual codebase — real files, functions, and commands, in a sensible order. Each step should be concrete enough to act on and to check. Avoid vague filler like "improve performance" or "add tests"; say what to change and where.

## Multiple Approaches
If your plan offers multiple alternative approaches, pass them via the `options` parameter so the user can choose which one to execute — see the `options` parameter for the format, count, and reserved labels. In yolo and manual modes the user sees all options alongside the host's Reject and Revise controls.

## Before Using
- In auto permission mode, do NOT use AskUserQuestion; make the best decision from available context.
- In auto permission mode, this tool exits plan mode without asking the user.
- In yolo and manual modes, this tool still presents the plan to the user for approval.
- If auto permission mode is not active and you have unresolved questions, use AskUserQuestion first.
- If auto permission mode is not active and you have multiple approaches and haven't narrowed down yet, consider using AskUserQuestion first to let the user choose, then write a plan for the chosen approach only.
- Once your plan is finalized, use THIS tool to request approval.
- Do NOT use AskUserQuestion to ask "Is this plan OK?" or "Should I proceed?" - that is exactly what ExitPlanMode does.
- If rejected, revise based on feedback and call ExitPlanMode again.
```

**参数契约**（strict）：

| name | 类型 | 必填 | 描述原文 |
|---|---|---|---|
| `options` | array<{label, description}>（1–3） | 可选 | `When the plan contains multiple alternative approaches, list them here so the user can choose which one to execute. Provide up to 3 options; 2-3 distinct approaches work best when the plan offers a real choice. Passing a single option is allowed and is equivalent to a plain plan approval. Each option represents a distinct approach from the plan. Do not use "Reject", "Revise", "Approve", or "Reject and Exit" as labels.` |

子结构：`label`（1–80 字符，`Short name for this option (1-8 words). Append "(Recommended)" if you recommend this option.`）；`description`（默认 `''`，`Brief summary of this approach and its trade-offs.`）。保留标签（trim+小写归一比对）：`Approve` / `Reject` / `Reject and Exit` / `Revise`；refine 文案 `Option labels must be unique.` / `Option labels must not use reserved approval labels.`。

**输出契约**：
- auto 模式：`Exited plan mode. Plan mode deactivated. All tools are now available.\nNote: this plan was auto-approved without user review — the user has NOT explicitly approved it. Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize after planning, do not start executing.\n{Plan saved to: {path}}\n\n## Plan (auto-approved, not user-reviewed):\n{plan}`。
- manual/yolo：`Exited plan mode. Plan mode deactivated. All tools are now available.\n{Plan saved to: {path}}\n\n## Approved Plan:\n{plan}`。
- 展示 `plan_review`（plan 全文 + path + options，≥2 时）；每次提交前记录一个 plan 修订版本。

**错误面**（isError: true）：非 plan mode `ExitPlanMode can only be called while plan mode is active. Use EnterPlanMode (or /plan) first.`；读计划失败 `Failed to read plan file: {message}`；无计划 `No plan file found. Write the plan to the current plan file first, then call ExitPlanMode.`（有 path 时点名路径）；退出失败 `Failed to exit plan mode: {message}`。

### 4.7 Cron 三件套（CronCreate / CronList / CronDelete）

#### CronCreate

- **注册**：名称 `CronCreate`；目录描述 `Schedule a prompt to be enqueued at a future time.`；domain `cron`。

**面向模型的描述（原文）**：

```
Schedule a prompt to be enqueued at a future time. Use for both recurring schedules and one-shot reminders.

Uses standard 5-field cron in the user's local timezone: minute hour day-of-month month day-of-week. `0 9 * * *` means 9am local — no timezone conversion needed.

## One-shot tasks (recurring: false)

For "remind me at X" or "at <time>, do Y" requests — fire once then auto-delete.
Pin minute/hour/day-of-month/month to specific values:
  "remind me at 2:30pm today to check the deploy" → cron: "30 14 <today_dom> <today_month> *", recurring: false
  "tomorrow morning, run the smoke test" → "57 8 <tomorrow_dom> <tomorrow_month> *", recurring: false

One-shots are best for near-term reminders. A task only fires while its session is still alive (see Session lifetime below), so favor near times — within hours or a few days — rather than scheduling weeks or months ahead.

## Recurring jobs (recurring: true, the default)

For "every N minutes" / "every hour" / "weekdays at 9am" requests:
  "*/5 * * * *" (every 5 min), "0 * * * *" (hourly), "0 9 * * 1-5" (weekdays at 9am local)

## Avoid the :00 and :30 minute marks when the task allows it

Every user who asks for "9am" gets `0 9`, and every user who asks for "hourly" gets `0 *` — which means requests from across the planet land on the API at the same instant. When the user's request is approximate, pick a minute that is NOT 0 or 30:
  "every morning around 9" → "57 8 * * *" or "3 9 * * *" (not "0 9 * * *")
  "hourly" → "7 * * * *" (not "0 * * * *")
  "in an hour or so, remind me to..." → pick whatever minute you land on, don't round

Only use minute 0 or 30 when the user names that exact time and clearly means it ("at 9:00 sharp", "at half past", coordinating with a meeting). When in doubt, nudge a few minutes early or late — the user will not notice, and the fleet will.

## Coalesce semantics

Fires are delivered only while the session is idle: a fire that comes due during an active turn is held and delivered at the next idle moment, never injected mid-turn.

If the scheduler slept past multiple ideal fire times (laptop closed, long-running turn, etc.), only **one** fire is delivered when it wakes up. The origin carries `coalescedCount` showing how many ideal fires were collapsed into this single delivery. You should treat `coalescedCount > 1` as "I missed some checks; only the latest state matters" rather than running the prompt that many times.

## Cron-fire envelope

When a cron task fires, the prompt you scheduled is re-injected wrapped in an XML envelope that exposes the fire context:

```
<cron-fire jobId="..." cron="..." recurring="true|false" coalescedCount="N" stale="true|false">
<prompt>
your original prompt text, verbatim
</prompt>
</cron-fire>
```

The envelope is parseable. Use `coalescedCount > 1` to know multiple ideal fires were collapsed into a single delivery (treat as "only the latest state matters"), and `stale="true"` as a cue that the task is past its 7-day threshold.

## 7-day stale behavior

Recurring tasks that have been alive for more than 7 days fire one
final time with `stale: true` on the envelope, and the system then
auto-deletes the task. The flag is the model's notice that this is
the last delivery. If the schedule is still wanted, call `CronCreate`
again with the same `cron` and `prompt` — that resets `createdAt` and
starts a fresh 7-day window. One-shot tasks are never marked stale.

## Jitter behavior

Anti-herd jitter is applied deterministically per task id:
  - Recurring: ideal fire time is shifted **forward** by an offset ≤ min(10% of the cron period, 15 minutes). A `*/5 * * * *` task can drift up to 30s; a `0 9 * * *` task can drift up to 15 minutes.
  - One-shot: only when the ideal fire lands on `:00` or `:30` of the hour, the fire is pulled **earlier** by ≤ 90 seconds. Other minutes pass through unchanged.

## One-shot vs recurring — when to pick which

Use `recurring: false` for "remind me at X" style requests, single deadlines, "in N minutes do Y", and any task that should not repeat. Use `recurring: true` for periodic polling (CI status, build watchers, scheduled reports), workday rituals, and anything the user explicitly described as recurring.

## Session lifetime

Cron tasks live in the current kimi CLI session. When you exit, they
are persisted under the session homedir; resuming the same session
reloads them and the scheduler resumes from each task's `createdAt`. Fire times that fell during the offline window are
collapsed into a single delivery via `coalescedCount` (and recurring
tasks past their 7-day window arrive with `stale: true` as their final
delivery).

Tasks do **not** carry over into a brand-new session — they are scoped
to the resumed session id, not to the working directory.

## Limits

A session holds at most 50 live cron tasks; creating one beyond that is rejected. (The `prompt` body is also capped — see its parameter description.) Expressions that never fire within the next 5 years (e.g. `0 0 31 2 *`, an impossible date) are rejected at create time.

## Returned fields

`id` (ULID), `cron` (the normalized expression), `humanSchedule` (English summary), `recurring`,
`nextFireAt` (local ISO timestamp with numeric offset, or null). `id` is needed by `CronDelete`.

## Tell the user how to cancel or modify

After successfully creating a task, proactively tell the user how they can cancel or modify it later. Users have no direct `/cron` command or self-service UI to manage reminders themselves; they must ask the model to make changes (e.g. "cancel my 9am reminder" or "change my daily check to 10am"). Include the task `id` in your message so the user can reference it.
```

**参数契约**：

| name | 类型 | 必填 | 默认 | 描述原文 |
|---|---|---|---|---|
| `cron` | string | ✅ | — | `5-field cron expression in local time: "M H DoM Mon DoW" (e.g. "*/5 * * * *" = every 5 minutes; "30 14 28 2 *" = Feb 28 at 2:30pm local — a pinned date like this repeats yearly unless you also pass recurring: false).` |
| `prompt` | string（schema 层 ≤8,192 码元；运行时另按 UTF-8 字节校验 ≤8 KiB） | ✅ | — | `The prompt to enqueue at each fire time. Limited to 8 KiB (UTF-8).` |
| `recurring` | boolean | 可选 | `true` | `true (default) = fire on every cron match until deleted or auto-expired after 7 days. false = fire once at the next match, then auto-delete. Use false for "remind me at X" one-shot requests with pinned minute/hour/dom/month.` |

**审批刻面**：字面量 JSON 规则（cron/prompt/recurring 参数快照）——区别于其他工具的简单名。**执行描述**：`Scheduling cron {normalized}` / `Scheduling one-shot {normalized}`。

**输出契约**：五字段 `id: {ULID}` / `cron: {归一化表达式}` / `humanSchedule: {英文摘要}` / `recurring` / `nextFireAt: {本地 ISO 带偏移}|null`（post-jitter 值）。

**错误面**（isError: true）：

| 条件 | 文案 |
|---|---|
| 全局开关关闭 | `Cron scheduling is disabled (MIRRI_DISABLE_CRON=1).` |
| cron 解析失败 | `Invalid cron expression: {detail}`（底层如 `cron expression must have exactly 5 fields (minute hour day-of-month month day-of-week); got {n}`、`cron {name} value {v} out of range {min}..{max}` 等） |
| 5 年内无触发 | `Cron expression {expr} has no fire within 5 years; refusing to schedule.` |
| 会话上限 | `Cron job cap reached (max {50} per session).`（解析期+执行期双检） |
| prompt 超限 | `Prompt exceeds {8192} bytes (got {byteLen}).` |
| one-shot 太远（>350 天） | `One-shot cron {expr} would not fire until {date} (more than a year out). If you meant "today" or a near date, the pinned day/month has already passed this year — pick a future date or use wildcards.` |

**非功能**：每会话 ≤50；prompt ≤8 KiB；5 年触发窗口；one-shot ≤350 天；recurring 7 天自动过期；确定性抖动（见描述）；只注入空闲会话；离线错过合并一次投递（`coalescedCount`）；任务随会话持久化、不跨新会话。

#### CronList

- **注册**：名称 `CronList`；目录描述 `List all cron jobs currently scheduled in this session.`；domain `cron`。

**面向模型的描述（原文）**：

```
List all cron jobs currently scheduled in this session.

Use this tool to see every pending cron task — both recurring jobs and
one-shot reminders — that you (or the user) have scheduled with
`CronCreate`. The output is the entry point for inspecting scheduled
work: it returns a stable id, the original cron expression, a human
rendering, the next post-jitter fire time, the recurring flag, the
task's age in days, and a stale indicator.

Each record carries:

- `id` — the task id (a ULID). Pass this to `CronDelete` to remove the
  task, or quote it in user-facing messages when asking for
  confirmation.
- `cron` — the verbatim 5-field cron expression as scheduled.
- `humanSchedule` — plain-English rendering (e.g. `every 5 minutes`).
- `prompt` — the scheduled prompt text, JSON-encoded so embedded
  newlines stay on one line. Truncated to 200 UTF-8 bytes with
  `…(truncated)` if longer. Use this to recall what a task is for
  after a context compaction, and as the source for the
  `CronCreate` refresh ritual.
- `nextFireAt` — local ISO timestamp with an explicit numeric offset
  for the next fire **after jitter has been applied**. The actual fire
  may land slightly before or after a round `:00` / `:30` minute mark
  due to herd-avoidance jitter; this is the value the scheduler will
  compare against, so it reflects what will really happen. `null` if
  the expression has no fire in the next 5 years (should not happen
  for tasks created through `CronCreate`, which validates).
- `recurring` — `true` for cadenced jobs, `false` for one-shots.
- `ageDays` — `(now - createdAt) / day`, two decimal places. Useful
  when deciding whether a long-running cron is still relevant.
- `stale` — `true` when a recurring task is older than 7 days. The
  system **auto-deletes the task after this fire** to bound session
  lifetime; the `stale: true` flag is the model's notice that this is
  the final delivery. To resume the same schedule, call `CronCreate`
  again with the original `cron` and `prompt` (the `prompt` row above
  carries it for exactly this purpose). One-shots are never marked
  stale — they fire at most once by construction.

Guidelines:

- This tool is read-only and never mutates state, so it is always
  safe to call (including in plan mode).
- Users cannot directly manage cron tasks themselves; if they want to
  cancel or modify a schedule, route the request through the model
  (i.e. call `CronDelete` or `CronCreate` on their behalf).
- The empty case returns `cron_jobs: 0\nNo cron jobs scheduled.`. Cron
  tasks survive a resume of the same session but do not bleed into new
  sessions.
- After a context compaction, or whenever you are unsure which cron
  jobs are live, call this tool to re-enumerate them rather than
  guessing ids from earlier in the conversation.
- Records are separated by a line containing just `---`, in the
  insertion order they were scheduled.
```

**参数契约**：无参数（strict 空对象）。
**输出契约**：header `cron_jobs: {N}`；空 `No cron jobs scheduled.`；非空逐条八字段（id/cron/humanSchedule/prompt[200 字节 JSON 预览]/nextFireAt/recurring/ageDays[两位小数]/stale），记录间 `---`，按调度顺序；`cron` 字段为调度时刻的归一化形态（列表时不二次归一化）；防御性容错（解析失败时 humanSchedule 回落原文、nextFireAt=null，不抛错）。
**错误面**：无。

#### CronDelete

- **注册**：名称 `CronDelete`；目录描述 `Cancel a scheduled cron job by id.`；domain `cron`。

**面向模型的描述（原文）**：

```
Cancel a scheduled cron job by id.

Use this tool to remove a cron task previously scheduled with
`CronCreate`. The `id` is the ULID value returned by `CronCreate`, or
shown in the `id:` column of `CronList` — quote it verbatim, no
prefix.

Behaviour by task kind:

- **Recurring task** (`recurring: true`): stops all future fires
  immediately. The scheduler picks up the deletion on its next tick.
- **One-shot task** (`recurring: false`): cancels the pending fire if
  it has not happened yet. One-shots that have already fired
  auto-delete themselves, so calling `CronDelete` on a fired one-shot
  returns "no cron job with id ...".

Not-found is reported as an error (not a silent no-op) so you can
correct yourself — typically by calling `CronList` to see which ids
are actually live, rather than re-trying with the same stale id.

Refresh pattern (use when you want a stale recurring schedule to
continue):

Stale recurring tasks are auto-deleted by the system after their final
fire — there is nothing for `CronDelete` to remove at that point. To
keep the schedule running, just call `CronCreate` with the same `cron`
and `prompt`. Use `CronList`'s `prompt` field to recall the original
text after a context compaction.

`CronDelete` remains the right call when you want to cancel a task
that is still live (recurring not yet stale, or a one-shot still
pending).

Guidelines:

- Users have no direct `/cron` command or self-service UI to delete
  tasks themselves; they must ask the model to cancel a reminder.
  When deleting on behalf of a user, confirm the action and report
  the result plainly.
- Cron deletion is irreversible — there is no undo. If you delete the
  wrong task, you must re-create it with `CronCreate`.
- If the model is unsure which id is current (e.g. after a context
  compaction), call `CronList` first rather than guessing.
```

**参数契约**：仅 `id`（string，必填）——`The cron job id (ULID) returned by CronCreate / CronList.`。
**输出契约**：成功 `Deleted cron job {id}.`。
**错误面**（isError: true）：id 形状非法（8 位 hex 或 26 位类 Crockford ULID 之外的形态）`Invalid cron job id {JSON.stringify(args.id)} — must be a ULID.`；未找到 `No cron job with id {args.id}.`（有意报错而非幂等 no-op）。

**界面呈现（cron 族）**：无工具卡专用渲染；已触发提醒的消息卡 `Scheduled reminder fired` / `Missed scheduled reminders`，detail 行（| 分隔）`{cron}` / `job {jobId}` / `one-shot` / `{N} fires coalesced` / `{N} missed` / `final delivery`。

---

## 5. 跨工具实现细节备忘（Rust 对齐注意）

1. **key:value 序列化**（任务族输出基础）：对象键 camelCase→snake_case、过滤 null/undefined、按插入序输出——Rust 端需复刻插入序与键名转换。
2. **两种失败形态并存**：工具性失败（isError: true）与文案式业务拒绝（isError: false，如 goal 族 "Goal not …"）——契约上必须区分。
3. **stopTurn / stopBatchAfterThis** 为内部循环控制提示（持久化前剥离）：UpdateGoal complete/blocked、SetGoalBudget 已达预算时以 stopTurn 收尾；goal 更新非 active、预算将超时 stopBatchAfterThis。
4. **goal 快照输出剔除 goalId**；完成/阻塞摘要是纯文本提示词（含 usage 格式化 `Xs`/`XmYYs`/`XhYYm`、`N`/`N.Nk`/`N.NM`）。
5. **CronCreate 的审批刻面是字面量 JSON 规则**（含参数快照），非简单工具名；cron 输入先归一化（trim + 连续空白折叠单空格）再解析/存储/输出。
6. **ExitPlanMode 每次提交（plan_review 展示读取即）记录一个修订版本**；auto 模式文案明示"未经用户审阅"。
7. **TodoList 写入走共享存储**（key `'todo'`），UI 面板与重放均从该 store 水合；写入后输出末尾恒附 write-reminder。
8. **Read 的 chip 基于结果**（输出行数）；**Write/Edit 的 chip 基于参数**（content 行数 / 参数 diff）——呈现层契约需注意数据来源差异。
9. **Glob 相对化基 = 搜索根，Grep 相对化基 = 工作目录**——两工具显示路径的基准不同。
10. **Read 的字节上限按渲染后文本计**（含行号、tab、截断记号 `...`）。
11. **契约原文中的上游字样**：CronCreate 描述含 "kimi CLI session"、Skill 描述含 `<kimi-skill-loaded>`、Skill 错误面含 "in v1"——均为照录的现行文案；Rust 复刻时建议中性化为品牌/版本无关措辞（属刻意改进须披露）。

## 6. 待复核

三项待复核内容统一登记于 [tool-calling-ROADMAP.md](tool-calling-ROADMAP.md) F-12：FetchURL 外层网络超时、ReadMediaFile 压缩配置键名、select_tools 的 names 数量上限。

## 7. 关联文档

- 产品语义（用户故事）：[tool-calling-requirements.md](tool-calling-requirements.md) E4（US4.1–4.20）
- 跨工具机制：[tool-calling-architecture.md](tool-calling-architecture.md)
- 代码定位与现状：[tool-calling-context.md](tool-calling-context.md) §1.8
- 执行状态与开放项：[tool-calling-ROADMAP.md](tool-calling-ROADMAP.md)
