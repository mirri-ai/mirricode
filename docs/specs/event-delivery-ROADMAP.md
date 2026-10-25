# 事件投递 — ROADMAP

> 状态/进度管理唯一入口。需求见 [event-delivery-requirements.md](event-delivery-requirements.md)；设计见 [event-delivery-architecture.md](event-delivery-architecture.md)。

## 批次规划

| 批次 | 内容 | 覆盖需求 | 状态 |
|---|---|---|---|
| W1 渲染层 | 选择性写回（ADR-002 §3.1）、批处理白名单扩展（§3.2）、排序键解耦前端部分 + 徽章派生拆分（ADR-003 §3.3 前端侧）、taskProgress 粒度（§3.4）、流式高亮降级（ADR-004） | US3.1/3.2/3.3、US2.1（部分）、US2.2/2.3 | 已完成（2026-10-24） |
| W2 协议层 | 分级目录 + digest 订阅模式 + ServerState 全局通道（ADR-001 §2.1-2.4）、新增 `event.turn.failed` 事件登记（approval/question/work_changed 等既有事件转全局路由）、rust lastUsedAt 产生点（ADR-003 §3.3.1：消息提交 handler + session.touch RPC）、digest 客户端适配（§2.4：seq-gap/watchdog/turn-end 兜底） | US1.1-1.4、US2.1（完整） | 未排期 |
| W3 退役 MRU 4 + 前端消费状态机（§2.5） | 前端订阅模式管理、MRU cap 移除、LRU 逻辑重写为 full/digest 双集合 | US1.2 完整闭环 | 未排期（依赖 W2） |

## 开放项

- 虚拟化（长会话/长输出渲染，US3.3 后半）——待 W1 落地后评估是否独立立项。
- TUI 多会话视图的分级消费——本域非目标，如裁决变更另行立项。
