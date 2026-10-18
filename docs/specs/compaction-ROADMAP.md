# 上下文压缩 — ROADMAP

> 本文只做**状态与批次管理**，不承载需求或设计内容（那些见 [compaction-requirements.md](compaction-requirements.md) / [compaction-architecture.md](compaction-architecture.md)）。

> **治理规矩（2026-09-10 用户定规）**：任何待裁决项，必须先在本域 context/ROADMAP 中写清背景、痛点、解决思路选项与各优缺点，再交用户裁决；不允许裸问。

## 批次状态

| 批次 | 内容 | 状态 |
|---|---|---|
| 领域文档 | 需求（含 SDR-001~005，005 为收口批次补记的 P-E1 立项归属）/架构（含 ADR-001~007）/context 基线 + glossary 术语 C1–C4 | ✅ 本批 |
| M1 | **Rust 触发接线**：生产路径注入模型窗口能力（消除 `max_context_tokens=0`）、配置接线（`compactionTriggerRatio` / `reservedContextSize` 驱动 strategy、清理死字段 `compaction_threshold_tokens`） | ✅ 已落地（2026-09-10，d3b76c3..8bdf5de4）：溢出学习统一保守口径（×0.85）；`turn_flow_config_from` 从 `[loop_control]` + 模型目录窗口构造 strategy，接入全部生产流（message dispatch / cron / 子代理 host 两路径）；模型切换（agent.setModel / session.updateMeta）原地更新既有流窗口；死字段已清除 |
| M2 | **保留尾落地（Rust）**：切线算法（ADR-001）、`apply` 记录保留计数字段 + resume 保真修复（ADR-005，含 reducer 不再清空保留内容）、elision 标记接通、摘要位置统一前置（ADR-002） | ✅ 已落地（2026-09-10，68cd5a9d..27119f13 + 终审修复波 4f42f7bf..0fa38fd7）：`compaction/retention.rs` 切线；落位四段形态（头部精华整条粒度 + elision + 摘要前置 + 保留尾）；`apply` 记录携带 `retainedTailCount`/`keptHeadUserMessageCount`/`elisionMessage`（可选字段追加未递增版本，先例 planText/turn.steer）；reducer 按计数重建（`plan_materialization` 模拟 live 物质化：vacuous step 移除、deferred 延迟落槽、容量拒绝）；US5.4 往返保真闸门测试 |
| M3 | **TS v2 对齐**：保留尾落位（替换仅用户消息形态）、修剪优先（对齐 Rust P8a-2）、必须变小守护、`computeCompactCount` 死代码清理或改造为切线实现 | ✅ M3-lite 已落地（2026-09-10，d797954a..eeb592ee：Rust 收敛三修复——media-part vacuous、legacy toolCalls 边界偏移、恢复期容量上限接线；TS 引擎改动按用户裁决不做——rust-first，TS 回退面文档标注见 context §6/ADR-006） |
| M4 | **摘要盲区治理（方案 A：出处链 + 显式标注，SDR-004/ADR-007）**：会话累计逻辑编号（live `entry_seq` 计数器 + reducer fold 镜像 + undo 区间括号校正）、apply 记录携带被替换编号区间与丢弃清单、摘要指令注入编号引用约定（每次重试逐次重建、显式列出排除编号）、事件面 `compaction.completed` 携带区间与丢弃计数 | ✅ 已落地（2026-09-10，ADR-007：编号区间 + 丢弃清单 + 指令注入 + 事件面；UI 呈现归 P-E1） |
| M5 | **配置面扩展**：保留尾预算键（比例/绝对值互斥 + 加载校验）、溢出重试上限键；v1 ratio 接线或声明移除（US6.3） | ✅ 已落地（2026-09-10，4d07d0ee..2a5b06e6 + d003ca8b + 1be19b17 + 1fe1ae2a：保留尾预算键 ratio/tokens 互斥 + 溢出重试上限键 + v1 声明移除（含 node-sdk fixture 连带）；终审修复波 120f3cab——config.set 在 merge_wire_patch 前置互斥校验，杜绝毒化文件写穿） |

依赖：M1 ✅；M2 ✅；M3 与 M2 可并行（各自引擎）；M4/M5 依赖 M2（保留尾形态先定）✅ 已满足。

## 开放项（承接 context §2/§3）

| # | 项 | 备注 |
|---|---|---|
| 1 | 保留尾默认比例 0.16 是否合适（相对触发阈值 0.85 的间距） | 初始默认值；实现后按"压缩后仍频繁触发"遥测调参 |
| 2 | 头部精华保留（2k）与保留尾并存还是由"翻旧账"工具取代 | 可回溯压缩提案（P-E1）落地前的过渡设计；P-E1 立项时重审——**P-E1 已立项为 history-readback 域（proposal，2026-09-10），重审义务由其 ROADMAP 开放项 1 承接**（[history-readback-ROADMAP.md](history-readback-ROADMAP.md)） |
| 3 | 摘要指令风格（SDR-002 交接笔记）落地后按摘要质量遥测复评 | 与八栏结构化模板的对比基准：压缩后首回合的纠错率 |
| 4 | token 估算精度（ASCII 4字符/token 对中文偏低） | NFR4 已披露；若保留尾切线频繁落在错误位置再立项精确计量 |
| 5 | Rust cancel 契约返回 null vs TS 返回值的对齐 | **已解决**（2026-09-10 复核）：两侧契约已对齐——rust 返回 null（`compaction_handlers.rs:325`）、v2 kernel `void`（TSV row 93 `v2:void`），无对齐工作 |
| 6 | 可回溯压缩（P-E1：目录卡片 + 现状卡 + 读历史/搜历史工具） | **已立项（proposal，2026-09-10）→ [history-readback-ROADMAP.md](history-readback-ROADMAP.md)**（需求/架构/context 四件套已建；实现批待其开放项 1–5 裁决） |
| 7 | v2 `strategy.ts` 近端保留窗口死代码的处置 | M3 实现时决定改造为切线实现或删除（context §1.9）；随 TS 引擎退役处置，不在 rust-first 路线上处理（2026-09-10） |
| 8 | ~~Rust 有效窗口学习口径疑似过估~~ | **已解决**（M1）：统一为保守口径 `floor(X × 0.85)`，与 TS 同式 |
| 9 | 对话撤销（undo）的需求归属域 | **已解决**（2026-09-10）：归属会话管理域（其 ROADMAP U1），压缩域仅保留 US4.4 边界条款 |
| 10 | NFR5 内存上限当前仅 Rust 有实现（max_history_messages 10,000），TS 无对应 | M3 TS 对齐时补齐或显式豁免；随 TS 引擎退役处置，不在 rust-first 路线上处理（2026-09-10） |
| 11 | ~~手动压缩在无可压内容时 `beginCompaction` RPC 以错误冒泡~~ | **已解决**（2026-09-10）：用户裁决为 info 级提示跳过——US2.6 补验收条款（SDR-003），Rust 侧 `begin` 无合法切点改 `CompactionUnable`、手动 RPC 拦截返回 `false`（`begin(): boolean` 契约），客户端据布尔渲染提示；auto 路径维持静默跳过；TS 对齐随 M3 |
| 12 | M2 带病合并的次级项 | 终审分诊通过：溢出学习 f64 在 ≥2^53 理论上差 1（brief 指定公式，TS 同款）；`RetentionCut.tail_tokens` 为预算凑够时点值（无消费方，doc 措辞待澄清）；手动压缩 refusal 路径 `CompactionResult.tokens_after` 残留 0（wire 写点有 `applied` 守卫，无实际影响）；cron 派发每次构造 flow config（分钟级低频） |
| 13 | 跨引擎压缩记录兼容性：双向语义降级（Rust→TS 近似切片、TS→Rust summary-only） | ✅ 已落地（切根 + migrate-sessions + TS 两分支 + 2.0，ADR-006-R2/ADR-005-R1） |
| 14 | **SDR-006（待用户裁决）**：摘要调用输入缺「规则说明 + 工具清单」前缀——NFR3 折扣声明无实现支撑 | 背景：US3.1/架构 §5.1 规定摘要输入以与上轮真实请求逐字相同的前缀开头（折扣友好），引擎实际只发「旧段 + 指令」，工具置空（`round.rs:324-334`）。痛点：摘要调用按无缓存折扣全价计费，成本敏感用户承诺落空。选项：A 改需求措辞（NFR3 如实化）——零成本但承诺落空 / B 排实现（前缀 threading 进压缩路径）——兑现承诺但接线面跨模块。优劣详见 SDR-006 |
| 15 | **SDR-007（待用户裁决）**：压缩摘要的任务清单自动附着未接线 | 背景：US3.3 要求进行中任务清单从唯一事实源自动附在摘要后；调用点写死 `None`（`round.rs:434`，注释自认）。痛点：压缩后模型对未完成项只剩摘要自觉转述，无硬保证。选项：A 排实现（清单事实源注入压缩路径）——需新增注入面 / B 需求降级为可选——零成本但转述无保真。优劣详见 SDR-007 |
| 16 | **SDR-008（待用户裁决）**：保留预算 ≥ 触发量的启动拒绝未实现 | 背景：US2.3-C2 要求该组合拒绝启动；`config` 校验只查保留尾两键互斥（`config/mod.rs:237-305`），无跨键校验。痛点：用户可配出「触发 0.5 + 保留 0.6」这类永不压缩的组合且无任何报错，压缩功能整体静默失效。选项：A 排实现（加载期同制硬校验 + 窗口未知时运行期软提示）——早失败但两层设计 / B 需求放宽为文档提示——零成本但静默失效留存。优劣详见 SDR-008 |
| 17 | **SDR-009（待用户裁决）**：摘要指令缺「最近对话已原样保留」条款，且被 verbatim 冻结测试固化 | 背景：US3.2-C2 要求指令告知模型「最近对话已原样保留、无需转述」（SDR-002 既定方向）；指令模板（`round.rs:520-567`）通篇按整段总结表述，且 2026-09-10 加固批对指令全文做了字节级冻结——补条款必须同变更改测试。痛点：模型可能把保留尾内容也转述进摘要（浪费 token + 失真风险）。选项：A 排实现（修订冻结文本并同步测试，实现批）——兑现 SDR-002 但冻结基线变更需评审 / B 删该条款——零成本但推翻已裁决方向需明示。优劣详见 SDR-009 |
| 18 | **SDR-010（待用户裁决）**：遥测缺重试次数/耗时/失败类型与阶段字段 | 背景：US7.2 要求完成可见重试次数、失败可见类型与阶段；`compaction.completed` 事件（`event_projection.rs:431-462`）现只携带 tokens 前后值/droppedCount/编号区间。痛点：压缩失败对用户是笼统「失败了」，无法分诊（溢出耗尽 vs 变小守护拒绝 vs 竞态取消）。选项：A wire/事件扩展批次（可选字段追加，ADR-008 纪律）——可观测但字段过三层 / B 需求降为可选——零成本但失败仍黑盒。优劣详见 SDR-010 |
| 19 | ~~**SDR-011（待用户裁决，P0）**：摘要期间历史分歧的取消分支未接入 begin→block 落位面~~ | **已落地（2026-09-12，按选项 A）**：begin 快照 threading 至 `FullCompaction::pending_snapshot`，`block` 与 `apply_compaction_result` 双落位面在 live re-cut 前运行 `prefix_race_check`（`mirri-core/compaction/mod.rs`）；分歧 → `CompactionFailed("prefix-race detected")`、memory 字节不动、无 apply record、orchestrator 复位；仅追加真实用户输入仍走 re-cut 包含（continue 夹具不变绿门槛）。原 `#[ignore]` 夹具（`compaction_tests.rs` `should_cancel_the_round_when_history_diverges_during_the_summarizer_call`）转正为常绿回归 |
| 20 | ~~**SDR-012（待用户裁决，P0）**：turn-active 手动压缩拒绝不可达 + dispatch 锁序反转（ABBA）~~ | **已落地（2026-09-12，按选项 A 双修）**：① busy 预检——dispatch 在任何锁之前查 in-flight turn 注册表，命中即拒（"cannot compact while a turn is active"）；② flow 锁改 `try_lock` 非阻塞获取（`message.steer` 先例），任何争用按 turn-active 拒绝，RPC 不再驻留；③ 锁序对齐——`request_manual_compaction` 改收 `&Arc<Mutex<ContextMemory>>` 并在内部按文档序（flow→compaction→memory）取锁，dispatch 不再先持 memory；④ apply task 的 `compaction_arc` 在持 flow 期间克隆，消除第二次 flow 获取窗口。原 `#[ignore]` 夹具（`embedded tests.rs` `should_refuse_manual_compaction_when_a_turn_is_active`）转正常绿。注意：选项 B（改排队语义）若未来被裁决采纳，busy 预检需随之调整——锁序对齐在任何选项下都必须保留（本项即为此已修） |
| 21 | L3/L4 重试预算同常量两种解释（生产码裁决项，Task 2 结转） | 背景：US3.4/架构 §5.3 对 L3（截断/空丢最旧）与 L4（退避）都写「最多 5 次」，实现同用 `MAX_COMPACTION_RETRY_ATTEMPTS=5` 却是两种比较：L3 守卫 `>`（第 6 次失败才放弃，容忍 5 次）vs L4 守卫 `>=`（第 5 次即放弃，容忍 4 次）（`round.rs:341/363` vs `:417`）。痛点：需求「最多 5 次」语义未写明是 5 次尝试还是 5 次重试，两分支行为不对称且无文档解释，改常量或改需求都会只动一半。选项：A 统一为容忍 5 次、第 6 次放弃（改 L4 `>=`→`>`）——与「最多 5 次」字面贴合，L4 多扛一次 / B 统一为第 5 次失败即放弃（改 L3）——更保守省时，L3 少扛一次 / C 需求写明各自语义并显式接受不对称——零代码改动，但不对称无理由。待用户裁决 |
| 22 | S4/S5 fs-seam：EXDEV 回退体与 migrate source-twin 分支零执行（未来触点） | 背景：migrate-sessions 的跨设备 rename 失败回退（降级 copy）与源平铺会话 id 撞名分支（2026-09-10 加固批加的守护）在测试中零执行——触发 EXDEV 需两个真实文件系统、撞名需特定目录拓扑，现有测试无法到达。痛点：两分支为纯生产守护（防静默降级/防嵌套误搬），当前无任何测试覆盖，回归不可见。选项：A 注入 fs/rename seam（文件系统操作 trait 化）后写双文件系统夹具——覆盖彻底但要动生产面 / B 每次 migrate 触点人工验证——零改动但守护裸奔 / C CI 挂临时文件系统（tmpfs/loopback）做集成测试——不动生产码但环境依赖强、可移植性差。登记为未来触点（本测试批红线：不动生产码） |
| 23 | US4.3 崩溃恢复测试缺口：dangling `full_compaction.begin` 归位 | 背景：wire 侧 `full_compaction.begin/cancel/complete` 为不透明占位（`wire.rs:541-553`，`FullCompactionBegin { data: Value }`）；压缩中途崩溃会留下无闭合的 begin 记录，US4.3 要求重启后未闭合压缩归位为「无压缩进行」。痛点：无任何测试钉住「带 dangling begin 的日志重放 → 会话可用且无压缩进行」，占位形状下 reducer 对该形态的语义未被验证。选项：A 补重放测试（按现有 wire 记录形态构造 dangling 日志）——纯测试 / B 若构造时发现 reducer 未处理该形态，升级为生产修复项并挂 SDR。测试缺口登记（不实现） |
| 24 | US5.6 压缩会话 fork 保真测试缺口 | 背景：US5.6 要求压缩过的会话 fork 后新会话模型视图与源会话压缩后视图逐条一致（压缩记录随对话记录一同复制）；事件面已有 SessionForked 投影。痛点：无测试钉住「压缩 → fork → 新会话重放」链路的视图一致性与复制边界上压缩记录的折叠行为，fork 路径的压缩保真未经验证。选项：A 补端到端 fork 保真测试（复用 US5.4 往返闸门的逐条比对法）——纯测试。测试缺口登记（不实现） |
| 25 | **压缩文本化携带留档路径（跨域登记：多模态输入域 SDR-003/ADR-005，E8「压缩即退场」）** | 被摘要头部的媒体部件退场为携带原件留档路径的文本标记（含「既有结论是解释而非证据，核对请重读路径」认知声明），最近尾部媒体原样保留；两次压缩之间媒体在场、不引入逐轮改写机制。设计归属多模态输入域（其 architecture §5.5/ADR-005，验收其 US8.2），实现落点在本域摘要输入构造与摘要指令模板——该域核实的现状缺口：v1 `[image]` 标记无路径、v2 fullCompaction 无媒体专项处理（其 context §6）。实现批次须与多模态域对齐；L2 读图证据约定已在工具调用域 F-16 登记 |

### 长程批（2026-09-10）终审延后项归属

终审 whole-batch review 分诊通过的延后项及其承接方（承接计划立项时从此表取活）：

| 延后项 | 承接方 |
|---|---|
| `config.set` 更广性质保（失败 set 永不落盘的通用 property 测试）——当前仅保留尾互斥有回归测试 | 若更多跨键约束落上 `ServerConfig` 时一并立项 |
| migrate 残余加固：`copy_dir_recursive` 符号链接跟随行为（文档已自认）；`is_cross_device_error` 裸 18 未平台门控（Windows 上 18 = `ERROR_NO_MORE_FILES`，未来触点 `cfg(unix)`）；M-1 嵌套守护测试中一条 `is_dir` 同义反复断言（应 `is_file`）；blocked 桶全命中 id-skip 时不报碰撞；汇总行 "N bucket(s) refused" 计数口径按会话数累加而文案写桶数（单行措辞失真，下次触点顺手修）；M-1 反向碰撞（源平铺会话 id == 目标根既有桶目录名）无专用守护——现走 ENOTEMPTY 硬错误中止整个迁移（源保留、安全但中断），需逐桶跳过 + 专用提示 | 未来任意 migrate-sessions 触点（2026-09-10 加固批终审延后） |

已解决移除（2026-09-10 收尾，记录于批报告）：think "text" fallback 直接测试 + `flush_deferred` doc 对齐（收尾批 Task 7）；mirri-server restore 容量面 + `reduce_with_limits` 端到端 + close-time/resume 折叠上限对称（收尾批 Task 9 容量键激活）；ROADMAP 区间简写（本次收口重写后 M4 状态行不再含提交区间）；§5/§6 历史时态措辞（本次收口在 context 补落地状态后记，verbatim 项保留原貌）。

已解决移除（2026-09-10 加固批，记录于批报告）：wire 负向独立测试（Task 2，`faf68d00`——mirri-proto `wire_op_tests` 四例：负计数拒收/编号字段类型拒收/缺 `summary` 拒收/未知字段容忍）与 `handle_overflow_error` 配置化集成测试（Task 3，`954752f1`——mirri-embedded wiring 模块 TOML→strategy→阶梯全链路）——与收尾批落地的容量键 absent-keeps-default 同触点族一并闭账；apply 记录出处映射四写点重复（Task 1，`bbb1b28c`——`apply_compaction_op_fields` helper + 三生产路径恒等测试）；vacuous-step 编号消耗直测（Task 4，`ee73e413`——live/fold 双侧烧号钉住）；摘要指令 contains-assert（Task 6，`ffda630f`——含/不含丢弃清单两种形态的全文 verbatim 冻结）；migrate EXDEV 门控 + flat-id 碰撞守护（Task 5，`f92de89f`——非跨设备 rename 失败不再静默降级 copy、桶名与 v3 根 flat 会话 id 撞名拒搬防嵌套；残余见上表 migrate 行）。
