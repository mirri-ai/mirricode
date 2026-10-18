# 用量可见性（usage-visibility）— ROADMAP

> 状态与批次推进的唯一源。需求与设计见 [usage-visibility-requirements.md](usage-visibility-requirements.md) / [usage-visibility-architecture.md](usage-visibility-architecture.md)；现状缺口编号（G1-G6）见 [usage-visibility-context.md](usage-visibility-context.md)。

## 批次划分

> B1-B4 已全部落地（2026-09-17 usage-visibility 计划，Task 1-9）；遗留口径项移入下方开放项。

| 批次 | 内容 | 覆盖缺口 | 前置 |
|---|---|---|---|
| ~~**B1 引擎地基**~~ | ~~活会话每步用量喂用量服务（修 G1/rust-ROADMAP-F5，已收口）；`agent.getUsage` 增加能力描述（是否上报缓存写入）；web 消费侧接线（修 G2/rust-ROADMAP-F9，含 klient 方言映射 G6）~~ | ~~G1 G2 G6~~ | 无 |
| ~~**B2 回合聚合器**~~ | ~~回合窗口四桶求和（ADR-002 锚点）+ 带回合归属的用量更新事件~~ | ~~G3~~ | ~~B1~~ |
| ~~**B3 格式化共享层**~~ | ~~anti-100 命中率、紧凑/精确两档计数、缺报省略的语义单源实现（TUI `usage-format` 与 web utils 各自落地，语义以本域架构 §2 为准）~~ | ~~G4~~ | 无（可与 B1 并行） |
| ~~**B4 呈现面**~~ | ~~TUI footer 统计段 + web 会话统计条（会话级）；web 回合胶囊 + 明细面板（回合级）；TUI `/usage` 面板 cache 桶列 + 命中率行；i18n 文案~~ | ~~G5~~ | ~~B1（会话级）/ B2+B3（回合级）~~ |

## 开放项

- **cacheWriteReported web 接线**：回合命中率分母已无条件含缓存写桶（SDR-001，终审修复）；write 行以非零写桶为门槛的过渡方案（模型→能力映射缺失前不渲染真零，见终审 I-2 修复），待 web 侧建立模型→能力映射并接到回合归属后激活（SDR-003 未上报口径）。
- ~~**服务端快照全聚合**~~：已收口（2026-09-17 服务端快照全聚合计划）——`SessionScope::runtime_extras` 对会话内全部 agent 的累计四桶求和，REST `session_view` 与 embedded `build_session_view` 同源受益；子代理 TurnFlow（`subagent/host.rs` spawn/resume 两面）补接 usage sink。
- ~~**resume 回填**~~：resume 会话不回填历史用量，统计从 resume 点重新累计（与服务端快照口径同族问题）；TUI 的 `sessionUsage` 与 `/usage` 面板同受此限，resume 后面板回退主 agent 口径。状态：已收口（2026-10-07）——用量总数已随 REST 快照回填（restore.rs → web seedUsage 同链），SDR-004 冷启动基线 `firstCall` 搭乘同一回填链（usages.restore 携带 first_call）。
- **/usage 面板错误路径**：`session.getUsage()` 失败或返回空 byModel 时，会话用量区只显示错误/占位行，不尝试渲染 live 口径命中率（终审修复波 re-review Minor，与 resume 回填同族的数据源优先级问题）；footer 不受影响。
- ~~**子代理全聚合**（SDR-002 终态）~~：已收口——live 事件口径与服务端快照口径均含子代理用量（快照侧见上条）；resume 后的用量回填仍开放（见下条「resume 回填」）。
- **BTW 侧聊口径**：BTW sidechat 的用量当前计入父会话的会话级统计（TUI footer/AppState 与 web 统计条同口径）；处理时需补一份 SDR-002 决策记录明确归属（终审 M-2）。
- **回合/步计数呈现**：「N turns · M steps」式回合与步计数尚未在任何呈现面露出（终审 M-1）。
- **推理子集行**：供应商单独上报推理 token 时才渲染（US2.2 条件行）；当前接入族均不单独上报，首个有上报的供应商接入时激活。
- **TUI 回合详情**：回合胶囊的终端等价物（回合摘要行/斜杠命令）仍开放，遵循 write-tui 技能的布局约束；本计划的 `/usage` 面板增强仅覆盖会话级。
- **用量更新事件粒度**：web 统计条与回合面板的刷新节流策略（避免流式 delta 触发全量重渲染，DSH 的 settled-node 模式可参照）。
- ~~**冷启动后命中率与刷新续算（SDR-004）**~~：`UsageState.first_call` 持久化 → resume 回填 → wire `firstCall` 暴露（SessionView / agent status）→ web 投影器 seedUsage 修刷新回归 → 统计条双指标组、会话级两位小数。状态：已收口（2026-10-07）。

