# 用量可见性（usage-visibility）— ROADMAP

> 状态与批次推进的唯一源。需求与设计见 [usage-visibility-requirements.md](usage-visibility-requirements.md) / [usage-visibility-architecture.md](usage-visibility-architecture.md)；现状缺口编号（G1-G6）见 [usage-visibility-context.md](usage-visibility-context.md)。

## 批次划分

> B1-B4 已全部落地（2026-09-17 usage-visibility 计划，Task 1-9）；遗留口径项移入下方开放项。B5 见下表。

| 批次 | 内容 | 覆盖缺口 | 前置 |
|---|---|---|---|
| ~~**B1 引擎地基**~~ | ~~活会话每步用量喂用量服务（修 G1/rust-ROADMAP-F5，已收口）；`agent.getUsage` 增加能力描述（是否上报缓存写入）；web 消费侧接线（修 G2/rust-ROADMAP-F9，含 klient 方言映射 G6）~~ | ~~G1 G2 G6~~ | 无 |
| ~~**B2 回合聚合器**~~ | ~~回合窗口四桶求和（ADR-002 锚点）+ 带回合归属的用量更新事件~~ | ~~G3~~ | ~~B1~~ |
| ~~**B3 格式化共享层**~~ | ~~anti-100 命中率、紧凑/精确两档计数、缺报省略的语义单源实现（TUI `usage-format` 与 web utils 各自落地，语义以本域架构 §2 为准）~~ | ~~G4~~ | 无（可与 B1 并行） |
| ~~**B4 呈现面**~~ | ~~TUI footer 统计段 + web 会话统计条（会话级）；web 回合胶囊 + 明细面板（回合级）；TUI `/usage` 面板 cache 桶列 + 命中率行；i18n 文案~~ | ~~G5~~ | ~~B1（会话级）/ B2+B3（回合级）~~ |
| **B5 统计条按计价档位分桶**（2026-11-01，SDR-005/ADR-004） | web 会话统计条：输入构成拆成缓存读取/未缓存输入/缓存写入三段（写入段以累计非零为门槛）、计数改精确千分位、宽度不足时按优先级降级；命中率口径不变 | —（呈现口径调整） | B4 |

## 开放项

- **cacheWriteReported web 接线（回合级）**：回合命中率分母已无条件含缓存写桶（SDR-001，终审修复）；回合明细的写入行以非零写桶为门槛是过渡方案（模型→能力映射缺失前不渲染真零，见终审 I-2 修复），待 web 侧建立模型→能力映射并接到回合归属后、改为 SDR-003 的静态家族判定。
- **会话级写入段的门槛**：ADR-004 c) 的「累计非零才显示」是**终态**，不随上条映射接线而改变（显式族未发生写入时看不到 `写入 0` 是本域已接受的代价）；本项只是跟踪 B5 的接线落地。
- ~~**服务端快照全聚合**~~：已收口（2026-09-17 服务端快照全聚合计划）——`SessionScope::runtime_extras` 对会话内全部 agent 的累计四桶求和，REST `session_view` 与 embedded `build_session_view` 同源受益；子代理 TurnFlow（`subagent/host.rs` spawn/resume 两面）补接 usage sink。
- ~~**resume 回填**~~：resume 会话不回填历史用量，统计从 resume 点重新累计（与服务端快照口径同族问题）；TUI 的 `sessionUsage` 与 `/usage` 面板同受此限，resume 后面板回退主 agent 口径。状态：已收口（2026-10-07）——用量总数已随 REST 快照回填（restore.rs → web seedUsage 同链），SDR-004 冷启动基线 `firstCall` 搭乘同一回填链（usages.restore 携带 first_call）。
- **用量查询接口错误路径**：`session.getUsage()` 失败或返回空 byModel 时，用量区只显示错误/占位行，不尝试渲染 live 口径命中率（终审修复波 re-review Minor，与 resume 回填同族的数据源优先级问题）；会话统计条（走 live 事件口径）不受影响。
- ~~**子代理全聚合**（SDR-002 终态）~~：已收口——live 事件口径与服务端快照口径均含子代理用量（快照侧见上条）；resume 后的用量回填仍开放（见下条「resume 回填」）。
- **BTW 侧聊口径**：BTW sidechat 的用量当前计入父会话的会话级统计（会话用量状态与 web 统计条同口径）；处理时需补一份 SDR-002 决策记录明确归属（终审 M-2）。
- **回合/步计数呈现**：「N turns · M steps」式回合与步计数尚未在任何呈现面露出（终审 M-1）。
- **推理子集行**：供应商单独上报推理 token 时才渲染（US2.2 条件行）；当前接入族均不单独上报，首个有上报的供应商接入时激活。
- **终端呈现面**：TS CLI/TUI 壳已于 2026-10-28 退役（见 `ts-v1-engine-retirement-requirements.md`）；终端呈现面重建时，会话统计条须遵循 ADR-004 的组序与门槛，回合胶囊的终端等价物（回合摘要行/斜杠命令）按 `docs/specs/tui/` 交互套件设计（交互与呈现规范见 `tui/interaction-requirements.md` 与 `tui-architecture.md`）。
- **用量更新事件粒度**：web 统计条与回合面板的刷新节流策略（避免流式 delta 触发全量重渲染，DSH 的 settled-node 模式可参照）。
- ~~**冷启动后命中率与刷新续算（SDR-004）**~~：`UsageState.first_call` 持久化 → resume 回填 → wire `firstCall` 暴露（SessionView / agent status）→ web 投影器 seedUsage 修刷新回归 → 统计条双指标组、会话级两位小数。状态：已收口（2026-10-07）。

