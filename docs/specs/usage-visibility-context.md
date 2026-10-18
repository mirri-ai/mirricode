# 用量可见性（usage-visibility）— 现状快照

> 快照日期：2026-09-17。本文档只记录现状代码事实、缺口与参考产品差异，不含目标与决策（见 requirements / architecture）。所有 file:line 以快照日主干为准。

## 1. 既有底座（可直接复用）

### 1.1 计量与传输（rust 内核）

- **每步四桶已落盘**：每个模型请求完成写 `usage.record` + `step.end` 两个 wire op，字段 `inputTokens` / `outputTokens` / `cacheReadTokens` / `cacheCreationTokens`（camelCase TS 方言；写入点 `rust/crates/mirri-core/src/context_memory/mod.rs:167-182`）。
- **排他口径已归一**：全部供应商适配器把命中从输入中减出，三桶互斥不重复计价（契约见 `rust/crates/mirri-kosong/src/provider.rs:75-83`，OpenAI 侧 `openai.rs:268-291`）。
- **重放折叠已累计**：reducer 对每个用量记录做四桶累计（`rust/crates/mirri-proto/src/reducer.rs:422-444`），状态字段 `total_input_tokens` 等——恢复即重建，这即 ADR-001 说的"重放折叠即持久投影"。
- **Anthropic 系已注入缓存断点**：system、最后一个工具定义、最后消息共 3 个 `cache_control` 断点（`rust/crates/mirri-embedded/src/providers/anthropic.rs:190/211/216`），因此该族 `cacheCreation` 有真实非零值。
- **用量服务已存在**：`AgentUsageService`（`rust/crates/mirri-core/src/usage.rs`，record/status + `on_did_record` 观察者）；`agent.getUsage` RPC 已实现，输出 klient 方言 `inputOther`/`inputCacheRead`/`inputCacheCreation`（ADR-012，`rust/crates/mirri-embedded/src/embedded_server/responses.rs:343-352`）。
- **活会话已接 usage_sink**：turn 流 `usage_sink` 把每步 `LlmUsage` 录进 `AgentUsageService`，未恢复会话 `agent.getUsage` 也有实时总量（`rust/crates/mirri-core/src/turn_flow/mod.rs:204-207`，注释明写 "F5, P7c"；接线点 `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs:550/1465`、`task_driver.rs:673`、`cron_driver.rs:484`）。
- **回合步事件已带用量**：`SessionEvent::TurnStepCompleted` 携带 `input/output/cache_read/cache_creation` 四桶与 `turn_id`（`rust/crates/mirri-core/src/session/events.rs:497-503`），投影为 wire `event.turn.step.completed` 的 `usage` 对象（`event_projection.rs:716-722`）。
- **Web 侧已累计会话级用量**：agentEventProjector 对 `turn.step.completed` 做四桶累计，并经 `sessionUsageUpdated` 推送会话用量快照（`apps/mirri-web/src/api/daemon/agentEventProjector.ts:921-929`）。

### 1.2 客户端

- **Web 数据契约已建模**：`AppSessionUsage`（四桶 + `contextTokens`/`contextLimit`/`turnCount`，`apps/mirri-web/src/api/types.ts:44-53`）；`sessionUsageUpdated` 事件类型已定义（`types.ts:615`）。
- **TUI footer 已有上下文行**：`context: N% (tokens/max)`（`apps/mirri-code/src/tui/components/chrome/footer.ts:162-172`）——即"上下文占用"的既有家，命中率段将加在其旁。

## 2. 缺口（落地前必须补）

> G1 已于快照日收口（下行留档编号，供 ROADMAP 引用）；其余缺口仍待落地。

| 缺口 | 内容 | 已有登记 |
|---|---|---|
| **G1 live 会话用量恒零（已收口）** | 已收口：活会话每步用量经 turn 流 `usage_sink` 喂 `AgentUsageService`，未恢复会话 `agent.getUsage` 也有实时总量（`rust/crates/mirri-core/src/turn_flow/mod.rs:204-207`，注释明写 "F5, P7c"；接线点 `rust/crates/mirri-embedded/src/embedded_server/message_handlers.rs:550/1465`、`task_driver.rs:673`、`cron_driver.rs:484`） | rust ROADMAP F5（已收口） |
| **G2 web 用量硬编码 0** | `toAppSession` 的 usage 全零填充，待 `agent.getUsage` 接线 | rust ROADMAP F9（agent-77） |
| **G3 无回合级聚合器** | wire 上无"回合窗口四桶求和"的派生；回合边界事件族本身有归属缺口（turn.started/completed 缺 agentId 等） | F-P7（agent-110） |
| **G4 无命中率格式化** | 无 anti-100 规则、无紧凑/精确两档实现 | — |
| **G5 无呈现面** | TUI footer 无统计段；web 无统计条与回合胶囊 | — |
| **G6 klient 字段方言** | rust wire camelCase vs klient `inputOther` 族，严格校验需 TS 侧映射 | F6（agent-77） |

## 3. 供应商缓存桶矩阵（快照日代码核实）

| 供应商族 | 适配器 | 缓存读取来源 | 缓存写入来源 | 备注 |
|---|---|---|---|---|
| Anthropic 系 | `mirri-kosong/src/streaming.rs:289/293`（流式）+ embedded `providers/anthropic.rs` | `cache_read_input_tokens` | `cache_creation_input_tokens` | 显式缓存；我们注入 3 断点，写入有真实 1.25× 计费含义 |
| OpenAI 兼容（rakuten/GLM/DeepSeek 网关） | `mirri-kosong/src/openai.rs:268-291` | `prompt_tokens_details.cached_tokens` | 协议不存在 | 隐式缓存，写入免费不上报——`cacheCreation` 结构性恒 0（session-bd620139 实测确认） |
| OpenAI Responses | `openai_responses.rs:395-407` | `cached_tokens` | 协议不存在 | 同上 |
| Google 系 | `google.rs:156-176` | `cachedContentTokenCount` | 硬编码 0 | 同上 |
| Kimi | 按其供应商类型走上述两通道之一 | 视通道 | 视通道 | Anthropic 兼容端点则两桶全有 |

## 4. 实测证据（2026-09-17 两会话对照）

用于"命中率指标能反映前缀稳定性工程质量"的产品论证，原始数据在会话 wire：

- **坏样本 session-bd620139**（micro-compaction 打穿前缀缓存，已修复）：历史跨过 20 条消息后 cacheRead 从 61504 塌回 ~47.7k，每步 re-bill ~8.5k token（17 步多付 ~119k），模型因 marker 重读已读文件；整体增量内容命中率趋近 0。
- **好样本 session-6fa37b28**（修复后）：20 请求，cacheRead 47.9k→70.1k 与上下文同步单调增长，稳态未缓存占比 2.1%，无上下文回落。

## 5. 参考产品（DeepSeek Harness）差异分析

参考实现位于 `../deepseek-harness`，调研全文见用户侧记录。与我们的关键映射与差异：

| 维度 | DSH | mirricode | 采纳差异的理由 |
|---|---|---|---|
| 持久投影 | 专门的 sessionProjections 单元（防分页/压缩改写窗口） | wire 重放折叠（ADR-001：不新建层） | 重放机制已覆盖，避免双份真相 |
| 子代理 | 无此概念（单代理会话） | 有；SDR-002 裁决会话级全聚合 | DSH 无先例，自行裁决 |
| 回合取数 | 回合窗口事件的严格 fold（deriveTurnTokenUsage） | 锚定步求和（ADR-002） | 我们的回合边界事件族有已知缺口，wire 步记录更可靠 |
| 缓存写入行 | 可选字段缺报即省略 | 供应商族静态门控（SDR-003） | 我们 wire 字段缺省写 0，无法运行时区分缺报与真 0 |
| 用量映射 | dsh-llm-deepseek mapUsage 减出命中 | kosong 适配器归一（既有） | 等价，无需改动 |
| UI 形态 | StatsLine（composer dock 文本条）+ TurnUsagePanel（胶囊+锚定弹窗） | 同构映射到 TUI footer 段 + web 统计条/回合胶囊 | 直接采纳其组序、显示门槛、无障碍与 locale 模式 |
| 反假全命中 | formatCacheHitPercent 自动加深精度 | 直接采纳（§2.2） | 产品原则一致：宁可 99.9% 不显示假 100% |

DSH 关键源文件（供实现期对照）：`packages/client/ui-chat/src/client/chat/StatsLine.tsx`（组序与显示门槛）、`TurnUsagePanel.tsx`（胶囊+弹窗+无障碍）、`token-format.ts`（anti-100 与两档计数）、`packages/llm/token-meter/src/usage-projection.ts`（替换式累计）、`turn-usage.ts`（回合严格 fold 与缺报省略）。
