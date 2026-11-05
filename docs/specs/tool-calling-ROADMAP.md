# 工具调用域 Roadmap — 执行状态与开放项

> 状态: 活文档（随执行推进更新） · 建立于 2026-09-10 ·
> **本文是工具调用域执行状态的唯一源**（按 `docs/specs/AGENTS.md` 治理约定：设计文档不持有进度，阶段状态、开放项在这里维护；本文不含需求与设计内容）。
> 设计事实源: [tool-calling-requirements.md](tool-calling-requirements.md)（域需求）· [tool-calling-architecture.md](tool-calling-architecture.md)（机制）· 现状参考: [tool-calling-context.md](tool-calling-context.md)（代码事实与差距）。
> **Rust 对齐索引（一句话）**：Rust 内核工具域的对齐执行状态唯一源在 [rust-core-design/ROADMAP.md](rust-core-design/ROADMAP.md)（RPC `tool.list/call`、用户工具 `tool.listUserTools/setUserToolEnabled`、事件 `tool.call.delta`/`tool.progress` 转正、`ToolManager.enabled_tools_explicit`、`WorkspacePaths` 沙箱 allowlist、`tool_result_truncation` 等）与 [plan/rust-v2-method-parity.md](../../plan/rust-v2-method-parity.md)；本文不复制彼处内容，只在本域侧维护"TS 现状基线化"的覆盖状态。

## 1. 覆盖状态（TS 基线文档化进度）

| 需求域 | 条目 | 状态 | 说明 |
|---|---|---|---|
| E1 工具可用面 | US1.1–1.3（元信息/命名/角色激活） | ✅ 已文档化 | v2 契约 + 注册 + 激活全链有代码事实（context §1.1–1.3） |
| E2 调用与执行 | US2.1–2.11（生命周期/校验/并发/流式/取消/超时/回合停止/断连恢复/流中断未执行/数字形态宽容/后台自动通知与再开） | ✅ 已文档化 | executor 全链有代码事实（context §1.4）；US2.10 为用户拍板新增的基线需求；**US2.11 为 2026-09-10 复核补录**——原 US4.6"读取指引"措辞与真实机制（自动通知、勿轮询）相反，已修正，自动通知闭环详见 context §1.11；**2026-09-10 架构师审查澄清 8 处可实现性歧义**（取消作用域=回合级、坏文本全可选仍执行、MCP 重试不保证恰好一次、去重=排序序列化判定、通知"无活动回合"定义与快照回退尾部 3,000 字节、goal 完成为模型声明、超时基线值交叉引用），需求与 context §1.11 已同步明确 |
| E3 工具结果 | US3.1–3.8（形态/错误/截断/摘要/合成结果/去重/错误反馈自修正闭环/进行中等待时长可见） | ✅ 已文档化 | 两级截断 + loopEventFold（context §1.4/§1.8）；**US3.7 为 2026-09-10 新增**——错误四要素规约（架构 §4.1），个别兜底文案的指引要求超出 TS 现状（context §5 差异行）；**US3.8 为 2026-11-02 新增**（SDR-005）——服务端权威时间戳裁决见 architecture ADR-7 |
| E4 内建工具集 | US4.1–4.20（27 个注册工具 + MCP/media 面 + 认证合成工具） | ✅ 已文档化 | 清单见 context §1.8；**2026-09-10 新增逐工具详细规格** [tool-calling-builtin-tools.md](tool-calling-builtin-tools.md)（描述原文/参数契约/输出格式/错误面/上限/界面呈现，27 工具全覆盖，经双评审修正）；「Think 渲染器」残留见 F-3 |
| E5 用户自定义工具 | US5.1–5.4 | ✅ 已文档化 | userToolService + wire op 持久化（context §1.7） |
| E6 渐进披露 | US6.1–6.4 | ✅ 已文档化 | toolSelect 全链；实验旗标默认关（F-7） |
| E7 工具策略 | US7.1–7.4 | ✅ 已文档化 | toolPolicy 四层 + preflight 强制点（context §1.6） |
| E8 非功能/可观测 | US8.1–8.4 | ✅ 已文档化 | 上限清单 + 事件面 + 跨端不一致点记录（context §6） |
| 未来增强提案 | P-1 参数名笔误宽容（非基线承诺） | 📋 提案 + 架构约定 | 需求文档 §9；命名规约落位 architecture §1.1；数字形态宽容已转正为 US2.10；相关现状见 context §2、ROADMAP F-6 |
| 跨域术语 | T1 工具 / T2 工具调用 / T3 工具输入展示 / T4 工具结果 | ✅ 已登记 glossary §T | 与本文档同批变更 |

## 2. 开放项清单

| # | 项 | 出处/依据 | 说明 |
|---|---|---|---|
| F-1 | ✅ 已实现（2026-11-02，ADR-7 全链落地） | requirements US3.4+US3.8 / architecture ADR-7 / context §6 | 引擎时间戳（事件 + 快照）+ web 呈现（卡片实时计时/转球旁最晚调用/完成定格/中断冻结/刷新延续）已合入；TUI 呈现待 F-18 |
| F-2 | web 端 `tool.call.delta` 显式忽略 | context §6 | 参数已在 started 事件全量，无损；流式参数形态为可选项，不做不算缺口 |
| F-3 | ✅ 已调查关闭（2026-09-10 调查结论） | context §5 差异行 | `case 'Think'` 自 fork 上游 kimi-code 首个提交即存在，本仓引擎历史（v1/v2）从未有过 Think 工具——为上游继承的转录兼容残留；`thinkSummary` 为无操作渲染，仅影响旧转录回放呈现，保留/删除均无风险（建议保留以兼容旧会话记录） |
| F-4 | ✅ 已决策（2026-09-10）：暂不提供 | context §6 | 决策 A：维持现状，用户侧不加工具集合变化通知（模型侧公告不受影响）；未来有需求再评估 |
| F-5 | ✅ 已澄清关闭（2026-09-10 评审核实） | context §6 | v1 live 路径 `registerMcpServer` 与 v2 同构（均为 `mcp__<server>__<tool>`）；`mcp:` 单冒号命名仅存在于已退役 services/tool 层，无跨引擎映射负担 |
| F-6 | ✅ 已决策（2026-09-10）；同日经后续裁决修订 | requirements §9 P-1→P-2 / architecture §1.1+ADR-1 / context §2 | 初判"笔误宽容不引入"；同日用户改判：别名升格为**独立设计决策**（ADR-1，两类来源：改名过渡 + 实证幻觉，另承担单项迁移兼容），P-1 残余兜底的无歧义归位原则被采纳进 P-2；治本仍为命名规约 |
| F-7 | ✅ 已决策（2026-09-10）：维持默认关闭 | requirements US6.4 | 决策 A：渐进披露继续实验旗标默认关；默认开启需先回归评估公告对既有会话的扰动 |
| F-8 | 审批/approval 域联动验收 | requirements E2/E7 边界 | approval 域文档未立项；本域保证强制点（`onBeforeExecuteTool` 检查点）与拒绝文案，审批 UX 验收待 approval 域文档立项后交叉复核 |
| F-9 | ✅ 已闭环（2026-09-10 与 mcp 域交叉核实） | requirements US2.4 / context §1.8 | mcp 域四件套已建立（2026-09-08）并覆盖 OAuth 与合成认证工具（会话内路径经其 ADR-011 设计认可）；本域 US4.20 的工具面行为与 mcp 域记载一致，无冲突 |
| F-10 | web 悬空 running 沉降为成功（R4 失真信号） | requirements US8.3 | ✅ 已决策（2026-09-10）：如实呈现"已中断"、两端对齐（US8.3 验收已按终态要求表述）；网页端待实现 |
| F-11 | ✅ 已决策（2026-09-10）：随会话生命周期，不自动清理 | requirements US3.3 / architecture §5 | 决策 A：落盘文件长期保留、随会话数据一并处置，不设上限/过期/自动清理 |
| F-12 | 内建工具规格待复核项 | builtin-tools §6 | FetchURL 外层网络超时、ReadMediaFile 压缩配置键名（256 KB/2000px）、select_tools names 上限。已核实无本地限制：WebSearch 结果条数（上限在远端）、TodoList 条数（四层无检查） |
| F-13 | ✅ 已裁决（2026-09-10）：参数名别名机制（独立设计决策，Rust 执行，超出 TS 基线）；首例 Read `n_lines` 改名 `limit` | requirements §9 P-2 / architecture §1.1+ADR-1 / builtin-tools §1.1 | 已裁决，机制与护栏见 ADR-1；`line_offset` 维持权威名；Rust/TS 会话隔离但支持单项迁移（别名承担单项迁移兼容）；幻觉类首例 `Read.offset→Read.line_offset` 已裁决（同日实证：v1 历史 + TS 实录报错） |
| F-14 | 边界声明的未立项域汇总 | requirements §1 边界 | 本域边界指向的域文档未立项：background-tasks / execution-environment / skills / goal / plan / cron / hooks / media（approval 已由 F-8 跟踪）；各域立项后交叉复核。另：跨域术语「工具结果落盘/两级截断」为 glossary 登记候选（与 compaction 域同批） |
| F-15 | ✅ 已落地（2026-09-12，rust `03c3ecb1`/`929983d9`） | context §7 增补 | 启用工具集稳定有序（字典序，初始与再启用一致）——公告（US6.2）跨重启稳定、provider 提示词缓存不失效；协作工具模式门控归属协作域 SDR-003（本域仅工具面索引） |
| F-16 | 读图证据约定（L2，跨域登记自多模态输入域 SDR-003） | 多模态输入域 SDR-003 / requirements E8 | ReadMediaFile 工具说明追加行为约定：对含文字的图片（报错截图、日志、代码片段）应把决定性内容**逐字引用**进回复——证据在第一时间沉淀为文本，媒体退场（压缩文本化）后替代物完备。待落地；落地时更新 builtin-tools §ReadMediaFile 描述原文 |
| F-17 | Bash 前台超时自动转后台（Rust 面）：`bashAutoBackgroundOnTimeout` 配置项接线 | architecture ADR-4 / builtin-tools §2.1 | 行为本体已裁决（ADR-4，2026-10-13，撤销 Rust 实现层既有杀灭行为，先落常开，含第二时限强制路径与交接并发互斥两项实现约束）；配置开关（杀灭逃生舱）与任务三件套不可用时的杀灭回退归本项，待后续批次接入 config 域 |
| F-18 | 终端（TUI）工具级耗时呈现排期 | requirements US3.4+US3.8 / architecture ADR-7 | 数据裁决已定（ADR-7 服务端权威时间戳）；终端卡片呈现待排期，呈现语义与 web 端一致（US3.4 各端一致要求） |
| F-19 | 工具耗时的参照系呈现（后续增强候选） | Echo 走查 E1（2026-11-02，synthetic hypothesis） | 绝对数字无"是否合理"参照（如该工具历史耗时、超时上限进度）；按 YAGNI 本期不做，待真实用户验证有需求再评估 |
| F-20 | 审批等待期间的用户侧时长可见 | Echo 走查（2026-11-02 发现）/ 评审 M-2 暴露 | 审批卡片目前只有过期倒计时、无已等待时长；属 approval 域潜在需求，本域 ADR-7 已明确不计入工具等待，approval 域立项时评估 |
| F-21 | 实现评审遗留 Low 项（2026-11-02 落地批次） | PHASE 4 代码评审 | ① tick gate 建议用 `working`（inFlight ∨ turnActive）而非仅 turnActive（submit→turn.started 窗口显示冻结数字的边角）；② DelegationCard 暂停态（成员 awaiting）显示 0ms——成员等待不应计入工具等待（ADR-7 语义）且应显式登记；③ 子代理成员卡（AgentStep/MemberFocus 面）未接时间字段；④ live 断言是源码字符串匹配非行为断言；⑤ in_flight_shadow 同 id 重复 started 无守卫（重放幂等性既有问题，与新字段无因果）；⑥ 非 final group + sessionActive 的冻结组合无独立测试。均不阻断已交付验收，按需排期 |
| F-22 | Bash 工具硬化设计（命令级授权/前台输出内存保护/凭证卫生/后台输出增量/交错合并/运行中日志可观察性闭环/**执行目录三层防线/受保护目录检测**）待实施 | [tools/bash-design.md](tools/bash-design.md) v1.3 / [tool-calling-bash-hardening-context.md](tool-calling-bash-hardening-context.md) | 2026-11-03 完成设计（三轮调研 + 评审 + 用户拍板）；批次 1 P0：US-BH1/2/3/6/7/10；批次 2 P1：US-BH4/5/8/9/11；11 个 US + 9 个 ADR 已裁决；PowerShell 同款硬化为后续跟进项；开放项：Agent kind 静默语义待核实、doctor 的 TCC 预授权检查项 |

### 评审记录（2026-11-03 Bash 工具硬化设计批次）

双 reviewer（需求完整性/R1-R9/反向幻觉抽查 + 架构一致性/file:line 抽查/相邻域冲突），全部 16+ 条代码事实抽查零失实；评审意见已全部回填（high 4 / medium 9 / low 9）：

- **修复要点**：① 命令匹配谓词收敛为「整条逐字 OR 分段 glob 全段命中」，deny/ask/allow/会话记忆四类规则对称生效，刻面 glob 元字符与 `!` 前缀转义（B-ADR-1 决策增量）；② 后台输出增量改走 `append_output` 路线（复用任务输出既有面，不走回合作用域 progress 通道）；③ 环形缓冲统一为 merged 总量 1 MiB；④ 四禁违规清理（US-BH5/US-BH6 迁移叙事措辞、§1.6 压缩为指针）；⑤ US-BH6 改写为模型视角契约真实性需求；⑥ US-BH3 披露点改为确定性的工具描述文案；⑦ 补 B-ADR-5（disable_timeout 前台生效终态裁决）；⑧ context 补 D-4 偏差行（后台 description 必填性）、借鉴表理由列改指针；⑨ US-BH1 增补面板明示记忆范围验收；⑩ `tools/*-design.md` 模式补登 AGENTS/AGENTS_CN 治理清单。
- **观察闭环轮（同日二轮，产品+架构双 reviewer + 用户拍板）**：新增 US-BH7（运行中可观察性：TaskOutput 字节分页 + 运行/静默时长信号 + TaskList 信号 + 「禁查询」文案改写为「有理由的诊断检查」）、US-BH8（静默预警：通道与终态通知账本分离、每静默周期 edge-triggered 一次、阈值 config 化、限 Process kind）、US-BH9（取消保全：先红后绿 bug-fix）；B-ADR-6（分页内置不走沙箱白名单——opencode 撤销路线与 deepseek「只围写不围读」哲学均不采纳，后者另立项 approval 域）、B-ADR-7（完成靠通知、检查是诊断的划线原则）。reviewer 补强：静默信号与心跳正交（不得复用 is_stale/heartbeat）、砍掉引擎侧增长率计算、TaskList 层信号并入 P1（多任务定位入口）、文案改写须连测试与 SDR 一起改。
- **执行目录轮（同日三轮，双产品调研 + 用户拍板）**：新增 US-BH10（执行目录三层防线：反 cd 引导 + cwd 参数过边界 + 分段器提取 cd 目标与 cwd 平权送审；静态可解 cd 段从授权匹配串剔除）、US-BH11（macOS 受保护目录字面模式检测 + 事前 Ask，治「系统 TCC 弹窗绕过产品权限体系」）；B-ADR-8（不硬禁 cd——三家无一先例；cd 提取 + 平权合并，opencode 路线适配）、B-ADR-9（字符串级检测定位事前知情而非强制边界，诚实边界写入描述）。开放项：doctor 的 TCC 预授权检查项。

### 评审记录（2026-11-02 US3.8/ADR-7 批次）

- **评审发现与修复**：① context 幻觉路径（`turn_flow/activity_view.rs` → 实际 `activity_view.rs`，已修）；② ADR-7 失实论据（审批卡片并无耗时显示，改为中性表述，F-20 登记）；③ 影响范围漏列 conversation-rendering 域快照形状同批更新义务（已补）；④ 授权点行号偏差 `execution.rs:268 → :288`（已修）；⑤ `durationMs` 跨事件族同名登记 glossary T11（v37，已补）；⑥ F-1 复述过长与 SDR/ADR 备选数不同步（已收敛）。
- **Echo 走查发现与修复**：① Universal 级——中断后悬空卡片计时行为未定义（字面解读=永远跳表，R4 失真），已补 US3.8 第 6 条验收 + ADR-7 消费端冻结规则；② Segment 级——参照系缺口，F-19 登记。
- **认知路径评估**：无新增阻断项（感知/理解/行动路径通过；"最晚开始"启发式经引擎批间阻塞语义核验恒成立；双计时器位置分离无误读风险）。