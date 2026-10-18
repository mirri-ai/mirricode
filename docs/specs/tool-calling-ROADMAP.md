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
| E3 工具结果 | US3.1–3.7（形态/错误/截断/摘要/合成结果/去重/错误反馈自修正闭环） | ✅ 已文档化 | 两级截断 + loopEventFold（context §1.4/§1.8）；**US3.7 为 2026-09-10 新增**——错误四要素规约（架构 §4.1），个别兜底文案的指引要求超出 TS 现状（context §5 差异行） |
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
| F-1 | ✅ 已决策（2026-09-10）：网页端先补工具级耗时展示 | requirements US3.4 / context §6 | 决策 A：网页端优先补齐（时长数据已具备、成本低），终端后续跟进；待实现 |
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