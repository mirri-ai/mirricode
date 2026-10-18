# Rust v3 内核会话审计报告：session-6b03e323-2b33-422c-941d-8e5eda775228

> **处置状态(2026-09-13 更新)**:本报告所列 11 个问题已全部处置完毕——9 个代码修复、2 个按设计记录(P1-5 双索引、P2-3 三层冗余,均有 ADR)。修复批次:本地 git 提交 `69f474e4..a7994244`(12 个提交);设计决策见 `docs/specs/session-persistence-architecture.md` ADR-009~015;实施计划见 `plan/rust-v3-session-audit-fixes.md`。验收:全 workspace cargo test 全绿 + 真实进程 API 验收 16/16 + 浏览器 web UI 完整体验(含审批流)通过。逐条处置见文末「六、处置结果」。

- **审计日期**：2026-09-13
- **审计对象**：Rust 新内核（v3_sessions 布局）下的一次真实会话，及其与 TS 内核（v2 sessions 布局）行为/格式的对照
- **数据来源**：
  - 会话目录：`~/.mirri-code/v3_sessions/wd_ai_0df641d34911/session-6b03e323-2b33-422c-941d-8e5eda775228/`（state.json、wire.jsonl、tasks/）
  - 索引：`~/.mirri-code/session-index.db`（SQLite）、`~/.mirri-code/session_index.jsonl`
  - TS 对照样本：`~/.mirri-code/sessions/wd_mirricode_963bbfd6e198/session_76463b05-…/`（TS 内核活跃会话）
  - Rust 源码：`rust/crates/mirri-core/src/session/{manager/mod.rs, persistence.rs}`、`rust/crates/mirri-cli/src/migrate_sessions.rs`
  - 文件系统实况：`/Users/chengri.a.qi/workspace/ai/`、`/Users/chengri.a.qi/workspace/opensource/mirricode/rust/`

## 一、会话概况

| 项 | 值 |
|---|---|
| workDir（声明值） | `/Users/chengri.a.qi/workspace/ai` |
| 用户提问 | "show me the file list you see" |
| 模型 | alias `fast` → `DeepSeek-V4-Flash` |
| 执行过程 | 1 次 `Bash ls -la`（权限已批准，exit 0），2 个 loop step |
| 结束状态 | `turn.ended reason=completed durationMs=11945`，无任何错误 |

会话运行本身无报错，但对照磁盘数据与源码后共发现 **11 个确认问题**，按严重度分为 P0（1 个）、P1（5 个）、P2（5 个）。

---

## 二、问题清单

### P0-1 Bash 工具未在会话 workDir 下执行（严重：正确性 + 安全）

**现象**：会话声明的 workDir 是 `/Users/chengri.a.qi/workspace/ai`（`state.json:10` 与 SQLite `sessions.cwd` 一致），但 `ls -la` 的返回内容与 `/Users/chengri.a.qi/workspace/opensource/mirricode/rust`（Rust 引擎自身源码目录，也是服务器进程的启动目录）逐字节一致——相同的 `total 288`、相同的每行大小与时间戳（`.` Sep 13 14:19、`..` Sep 13 14:37、`.codegraph` 16:10、`tools/` Sep 11 10:32 等），父目录 48 个条目亦吻合。

**反证**：`workspace/ai` 目录当前仅有 `.DS_Store` 与 `skills/`，目录 mtime 为 9 月 5 日——会话运行时（9 月 13 日）该目录不可能包含 ls 输出中的 AGENTS.md、Cargo.toml 等文件，命令从未在此执行。

**推断根因**：Rust 引擎的 Bash 工具使用了进程自身的 cwd（服务器启动目录），没有把会话/workspace 的 workDir 传入工具执行环境。需进一步定位 `rust/crates` 中工具执行的 cwd 传递链路。

**影响**：
- **正确性**：agent 在错误目录下读写文件，用户以为它在自己的项目里干活，实际操作的是引擎源码目录或其他目录。
- **隐蔽性**：模型会忠实复述工具返回的内容，对话内完全无法察觉——本会话中模型说"我看到了当前工作目录的文件列表"，没有任何提示。
- **安全**：会话之间没有目录隔离；一个会话可能读写到另一个目录（含引擎自身、其他项目）的文件，权限体系若按 workDir 白名单设计则完全失效。

---

### P1-1 `updated_at` 在会话创建后从不刷新

**现象**：`state.json` 与 SQLite 索引的 `updated_at` 均停在 `2026-09-13T07:13:24.131915`（创建后 65ms），而本轮 turn 实际在 `07:13:36` 才结束（`wire.jsonl` 的 `turn.ended durationMs:11945`）。

**代码依据**：`save_state` 生产调用仅两处——会话创建（`rust/crates/mirri-core/src/session/manager/mod.rs:469`）与 lazy materialize（`mod.rs:915`）；`updated_at` 在核心代码中只有读取/拷贝，没有运行期写入。

**TS 对照**：TS 会话（session_76463b05）的 `updatedAt` 随对话实时推进（createdAt=1789283936693 → updatedAt=1789284718750，跨多轮）。

**影响**：会话列表"最近活跃时间"永远等于创建时刻；多轮会话无法体现最后活动；按 `updated_at DESC` 排序（SQLite 索引 `idx_sessions_ws_updated` 即此用途）的结果不反映真实活跃度。

### P1-2 `last_prompt` 两处不一致，且 Rust 写盘会抹掉

**现象**：SQLite 索引 `last_prompt = "show me the file list you see"`（有值），而 `state.json` `last_prompt: null`。

**代码依据**：`rust/crates/mirri-core/src/session/persistence.rs:125-129` 注释明确说明 `customTitle`/`lastPrompt` 等 TS-only 字段不在 scope 层元数据里，写盘时默认置空；一旦发生 load-mutate-save（archive、重命名等），这些字段会被清掉。`custom_title`、`custom`、`forked_from`、`agents` 同样如此。

**TS 对照**：TS 会话的 `lastPrompt` 随最新提问实时更新（当前值为最新一条用户消息）。

**影响**：会话列表"最后一条消息预览"丢失或不一致；`custom_title`（用户自定义标题）在任何 Rust 写盘操作后会被清空。

### P1-3 TS 老会话的 `cwd` 键 Rust 读不进来（迁移兼容 bug）

**现象**：线上 TS 会话的 `state.json` 使用 `"cwd"` 键（抽查 4 个活跃 TS 会话全部如此）；Rust `persistence.rs:105` 的 serde alias 只有 `workDir`，全代码库无 `cwd` alias。

**代码依据**：`persistence.rs:104` 注释声称 TS 字段名为 `workDir`，与线上实际数据不符——注释所依据的 TS 版本与当前 TS 内核的写盘格式已经脱节。

**影响**：TS 会话经 `mirri migrate-sessions` 迁入 `v3_sessions/` 后，Rust 引擎加载其 `state.json` 时 `work_dir` 为 `None`，workspace 归属丢失；依赖 state.json 内 workDir 的逻辑（如 `migrate_legacy_flat_sessions` 读取 workDir 决定归属）对 TS 会话失效。

### P1-4 `usage.record` 字段口径与 TS 完全不兼容

**现象**（同一次 Bash 调用，两种格式）：

- TS：`{"model":"fast","usage":{"inputOther":47025,"output":80,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":…}`
- Rust：`{"model":"DeepSeek-V4-Flash","usage":{"inputTokens":16167,"outputTokens":67,"cacheCreationTokens":0,"cacheReadTokens":0}}`

差异点：字段名（`inputOther` vs `inputTokens` 等）、`model` 记录 alias vs 实际模型名、有无 `usageScope`、有无 `time`。

**附带账目矛盾**：step 1 `inputTokens 16167, cacheCreationTokens 0`；step 2 `cacheReadTokens 16128 + inputTokens 642`。首次调用未记任何缓存创建，第二次却能命中 16128——要么首次的 cache creation 没记账，要么 hit/miss 映射（Anthropic 口径的 cacheCreationTokens vs DeepSeek 的 hit/miss tokens）没有对齐。

**影响**：任何按 TS 口径实现的用量统计/成本展示组件，对 Rust 会话直接失效；两套数字并存时计费口径不可信。

### P1-5 双索引并存，会话集合分裂

**现象**：`session_index.jsonl`（TS 侧，753 条，审计当日 16:18 仍在追加）与 `session-index.db`（SQLite，38 条）并存。本 Rust 会话只存在于 SQLite，不在 jsonl；TS 历史会话大体只在 jsonl。

**影响**：任何只读 jsonl 的入口（TS CLI 的 resume/会话列表等）看不到 Rust 会话，反之亦然——不同入口看到的"会话列表"内容不一致，用户在 A 端建的会话在 B 端凭空消失。长期双写还会漂移。

---

### P2-1 wire 事件全部缺少 `time` 字段

**现象**：TS wire 每个事件带毫秒时间戳（`"time":1789283936774`）；Rust v3 wire.jsonl 中除 metadata 的 `created_at` 外，所有事件（`turn.prompt`、`tool.result`、`turn.ended`…）均无 `time`。

**影响**：会话重放、vis 调试工具、审计排错失去事件级时序；只能靠 JSONL 行序近似推断。

### P2-2 `tool.result` 结构不兼容且嵌套 JSON

**现象**：TS 的 `result.output` 是纯文本字符串（换行真实存在，由 JSON 转义一层）；Rust 的 `result.output[]` 是 part 数组，`text` 里塞的是再转义一层的 JSON 字符串：`{"exitCode":0,"stdout":"total 288\\n…"}`。

**影响**：UI/重放工具需同时兼容两种形状；直接渲染 Rust 结果会显示 raw JSON（含 `\n` 转义）而非多行文本。同时数据被双重编码，可读性与解析成本都变差。

### P2-3 同一份工具输出存了三份

**现象**：本会话单条 `ls -la` 输出（约 1.4KB）同时存在于：`wire.jsonl`（JSON 嵌套形式）、`tasks/task-*.json` 的 `output` 字段（纯文本）、`tasks/task-*/output.log`（纯文本）。整个会话目录 20KB。

**影响**：存储膨胀（乘上历史会话数量后不可忽略）；三处副本存在不一致风险（如任务中断时）。

### P2-4 Rust `state.json` 缺 `id`/`version` 字段

**现象**：TS `state.json` 有 `"id"` 与 `"version":2`；Rust 只有目录名和 wire metadata 里的 `protocol_version`，单看 `state.json` 无法区分格式代际。

**影响**：格式演进时缺少文件内版本锚点，兼容层只能靠结构猜测（如"有无 state.json"、"是否 bucket 布局"），长期维护成本高。

### P2-5 模型回复质量问题（非引擎 bug，但值得记录）

**现象**：
- **漏项**：`ls -la` 输出 14 个条目，模型汇总表格只列了 13 行，漏掉 `tools/`。
- **语言不匹配**：用户用英文提问，模型用中文回答（取决于该会话 system prompt 的语言规则配置，若规则为"跟随用户语言"则未生效）。

**影响**：汇总类回复丢条目属于模型自身缺陷（DeepSeek-V4-Flash on alias `fast`），在文件清单、目录对比等场景会产生误导；语言不匹配影响交互体验。

---

## 三、已排查并排除的项（避免误报）

| 疑点 | 结论 |
|---|---|
| `config.update` 被写进会话历史流（wire.jsonl:2，`modelAlias:"fast"`） | **非问题**：TS 同样固化 `config.update` 进 wire（对照样本有 `{"type":"config.update","thinkingEffort":"max"}`），是两边一致的设计。 |
| wire 首个 content part 为空白文本 `"\n\n"` | 模型行为（工具调用前输出空白 token），非引擎问题。 |
| `protocol_version` 1.5 vs 2.0 | 设计如此（v3 会话启用 2.0，1.5/1.6 → 2.0 在 resume 时惰性迁移，见 `mirri-cli/src/migrate_sessions.rs` 文档注释）。 |
| 模型名 `DeepSeek-V4-Flash` | alias `fast` 的实际解析结果，非异常。 |
| turn 时长 11945ms（仅 ~340 output tokens） | 模型速度问题，非引擎问题。 |

## 四、优先级汇总与修复建议

| 编号 | 严重度 | 问题 | 一句话修复方向 |
|---|---|---|---|
| P0-1 | **致命** | Bash 工具在服务器进程 cwd 执行，而非会话 workDir | 工具执行层注入 session/workspace cwd，并对越界路径做隔离校验 |
| P1-1 | 高 | `updated_at` 创建后不刷新 | turn 结束钩子中 load-mutate-save 元数据（或独立轻量更新路径） |
| P1-2 | 高 | `last_prompt` 等字段写盘即抹除 | 扩充 scope 层元数据结构，把这些字段纳入持久化 |
| P1-3 | 高 | TS `cwd` 键读不到 | serde 增加 `alias = "cwd"`，并修正 persistence.rs 注释 |
| P1-4 | 高 | usage 口径不兼容且自相矛盾 | 统一字段名/口径（建议沿用 TS 名），补 `usageScope`/`time`，核对 provider 映射 |
| P1-5 | 高 | jsonl / SQLite 双索引分裂 | 明确单一索引来源，另一方只读或提供同步迁移 |
| P2-1 | 中 | wire 事件无 `time` | 协议 2.0 事件统一补时间戳 |
| P2-2 | 中 | tool.result 双重 JSON 编码 | 结构化结果走独立字段，`text` 保持纯文本（或对齐 TS 形状） |
| P2-3 | 中 | 工具输出三份冗余 | wire 只存引用，正文以 tasks/ 为准（或反之） |
| P2-4 | 低 | state.json 缺 id/version | 写盘补 `version` 字段 |
| P2-5 | 低 | 模型漏项/语言不匹配 | 记录观察；语言规则检查 system prompt 组装 |

**修复顺序建议**：P0-1 单独先行（基本正确性问题，且在对话内不可察觉）；P1-1 与 P1-2 同根（turn 后元数据刷新），可一并修；P1-3 是一行级修复但影响迁移用户，尽快修；P1-4/P1-5 涉及协议与索引口径，建议先出设计决策再动手。

## 五、复现与验证方式

- 本地复现 P0-1：在目录 A（内容可辨识）中新建 Rust 会话，让其执行 `pwd && ls`；若输出为服务器进程目录而非 A，即命中。
- 验证 P1-1/P1-2：跑一轮 turn 后 `cat v3_sessions/<ws>/<sid>/state.json`，对比 `updated_at`/`last_prompt` 与 SQLite 行。
- 验证 P1-3：`mirri migrate-sessions` 迁移任一 TS 会话后用 Rust 引擎 load，检查 `work_dir` 字段。

---

## 六、处置结果(2026-09-13)

| 编号 | 问题 | 处置 | 依据/提交 |
|---|---|---|---|
| P0-1 | Bash 工具在服务器进程 cwd 执行 | **已修复**:BashTool/GlobTool/GrepTool 注入会话 workDir 为默认 cwd/根(`BuiltinToolServices.workspace_paths`),模型显式入参仍可覆盖;含装配级测试 | ADR(rust-tool-calling 追加节);`eccddd9f`、`08c52207` |
| P1-1 | `updated_at` 创建后不刷新 | **已修复**:turn settle(成功/失败/abort,覆盖 send/steer/cron/task 驱动)统一 bump updated_at + 落盘 state.json + SQLite 写通;含 fire 模式时间敏感测试与 abort/cron 护栏测试 | ADR-009;`fb16e17c`、`a7994244` |
| P1-2 | `last_prompt` 写盘抹除 | **已修复**:`last_prompt` 纳入 scope 层 `SessionMetadata`,send 时写入,双向 From impl round-trip;fork 场景也继承 | ADR-009;`16e532ba`、`a7994244` |
| P1-3 | TS `cwd` 键读不到 | **已修复**:serde 增 `alias = "cwd"`,注释修正;配套 TS 形状 state.json 加载测试 | `8fdb0015` |
| P1-4 | usage 口径不兼容 | **已修复(按 ADR-012 分层)**:磁盘拼写保持 Rust 自有(版本链纪律);客户端事件投影已对齐 TS(补 pin 测试);`agent.getUsage` 改为裸 TS `UsageStatus`;`usageScope`/`time`/model-alias 澄清为 TS 磁盘行属性,响应面盘点登记于 ADR-012;"账目矛盾"确认为 DeepSeek 隐式缓存的预期语义(已显式化) | ADR-009~012;`7c78c386`、`fb16e17c` |
| P1-5 | 双索引分裂 | **按设计记录**:两引擎 SQLite 索引刻意分离至 TS 退役(spec 既定);修复了 reconcile 冷启动丢 `last_prompt` 的真 bug(迁移 TS 会话后列表可见性已验证);data-locations.md 双语更新 | ADR-015;`fb16e17c` |
| P2-1 | wire 事件缺 time | **已修复**:append 层统一注入 `time`(metadata 行除外,已有值保留),重放天然兼容 | ADR-011;`f696031d` |
| P2-2 | tool.result 嵌套 JSON | **已修复**:Bash 结果改为 TS 纯文本格式(镜像 `foregroundCompletionResult` 语义);消费者扫描确认无人解析旧 JSON 形状 | ADR-013(含版本裁决:记录形状未变,不递增 wire 版本);`b0488781` |
| P2-3 | 输出三份冗余 | **按设计记录**:三层各司其职(output.log 实时尾随 / task.json 恢复快照 / wire.jsonl 对话重放),冗余为有意设计 | ADR-014 |
| P2-4 | state.json 缺 version | **已修复**:Rust 写盘带 `version: 3`(v3 存储根纪元),读取容忍缺失 | ADR-010;`8fdb0015` |
| P2-5 | 模型回复质量(漏项/语言) | **部分修复**:语言跟随指令已 seed 进 Rust 默认 base system prompt(逐字镜像 TS `system.md`,caller 自带 prompt 时不注入);表格漏 `tools/` 一行属模型自身缺陷,记录为观察项不修 | `da02aa34` |

### 实施过程记录

- SDD:ADR-009~015 先行落盘(`69f474e4`),正文字段表/写通点清单同步更新;`plan/rust-tool-calling.md` 补默认工作目录设计节。
- TDD:全部 9 个代码修复均为"先红后绿",测试名 Given-When-Then;reviewer 独立复审结论"可合并",其 5 条建议(S1-S3/N1-N4)已全部处置(`a7994244`),含 fork 继承、abort/cron 护栏、byModel alias 键、预存 terminal_pool env 竞态修复。
- 验收:①rust workspace cargo test 全绿;②真实进程验收(`mirri serve` + 隔离 home + mock OpenAI):16 项断言含 P0-1 工作目录、P1-1/2 元数据、P2-1/2 格式、SQLite 写通,全过;③浏览器 web UI 完整体验:会话列表(lastPrompt/时间线)、对话渲染(纯文本工具结果)、权限审批流(Manual 模式 Approve → Bash 在会话 workDir 执行)、失败 turn 也正确 settle。
- 过程中额外发现并修复 1 个由 settle 引入的事件序回归(`09aa6c03`):ADR-009 的 settle await 插在终态事件发布与 RPC 响应之间,导致 `event.turn.ended` 先于 `message.send`/`message.abort` 的响应帧到达,被按 id 扫描响应的客户端丢弃。修复:settle 移到事件发布之前,恢复"发布→响应"零 await 的基线时序,由 mirri-server abort e2e 钉住(五连跑稳定通过)。
