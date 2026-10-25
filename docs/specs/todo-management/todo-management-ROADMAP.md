# 任务清单域（todo）— ROADMAP

> 状态登记文档（按 `docs/specs/AGENTS.md` 治理约定）：只做开放项与执行状态管理，不含需求/设计内容。
> 需求见 [todo-management-requirements.md](todo-management-requirements.md)；设计见 [todo-management-architecture.md](todo-management-architecture.md)；现状缺口 G1–G7 见 [todo-management-context.md](todo-management-context.md)。

## 1. 开放项

| # | 项 | 需求锚点 | 设计锚点 | 状态 |
|---|---|---|---|---|
| F-1 | Rust 写入落 `tools.update_store` 记录（双写：记录 + 内存） | US2.1 / SDR-001 | arch §2.1 / ADR-1 | 已完成 |
| F-2 | Rust reducer 折叠分支 + `ToolsState.store` 字段 + restore 恢复清单 | US2.2 / US2.4 | arch §2.2 | 已完成 |
| F-3 | Rust undo 检查点（折叠态权威 + 内存重同步） | US2.3 | arch §1.3 / §2.4 / ADR-2 | 已完成 |
| F-4 | Rust 压缩摘要附录清单（接 `post_process_summary` 既有接缝） | US3.1 | arch §2.3 | 已完成 |
| F-5 | Rust 主代理 turn flow 补 `set_todo_list` 装配 | US4.1 | arch §2.5 | 已完成 |
| F-6 | Rust 提醒注入器激活门控（`isToolActive` 等价） | US4.2 | arch §2.5 | 已完成 |
| F-7 | glossary 登记 T7–T10 域内术语 | req 头部声明 | — | 已完成（同批登记） |
| F-8 | rust resume 响应组装 `toolStore['todo']`（终端恢复验收依赖） | US2.2 / US5.1 | arch §2.2 / §3 | 已完成 |
| F-9 | rust server 会话快照携带清单 + 网页端派生源切换（快照优先、消息派生兜底） | US3.2 / US5.2 | arch §3 / ADR-3 | 已完成 |

## 2. 执行依赖与建议顺序

- F-1 → F-2 严格先行（无记录则无折叠可恢复）；F-2 完成后 F-3（undo 依赖折叠态承载检查点）。
- F-5、F-6 与 F-1/F-2 无依赖，可并行。
- F-4 依赖 F-1 的会话级清单句柄注入通道形状（可先做，测试用直设句柄）。
- 建议批次：批 1 = F-1+F-2+F-8（持久化闭环，含 resume 响应面）；批 2 = F-5+F-6（提醒闭环）；批 3 = F-3（undo）；批 4 = F-4+F-9（压缩闭环，含网页数据源）。

## 3. 验收面提醒

- F-1/F-2 的用户可见验收在消费端：终端恢复后面板非空（US5.1）、网页冷快照含清单（US5.2）——实现时须含两端验证，不只测引擎单元。
- F-5 的回归风险点：提醒注入主代理后，确认与子代理注入互不干扰（同一代理去重）。
