# 工作区管理 — ROADMAP

> 本文只做**状态与开放项管理**，不承载需求或设计内容（那些见 [workspace-management-requirements.md](workspace-management-requirements.md) / [workspace-management-architecture.md](workspace-management-architecture.md)）。
> Rust 引擎侧的实施批次与状态**唯一源**：[rust-core-design/plans/08-multi-workspace.md](rust-core-design/plans/08-multi-workspace.md)（W1-W9）与 [rust-core-design/ROADMAP.md](rust-core-design/ROADMAP.md) 1.8——本文只引用不重复。

## 覆盖状态

| 能力面 | 需求锚点 | 文档状态 | 备注 |
|---|---|---|---|
| 注册与身份（含隐式注册、幂等、别名折叠） | E1 | ✅ 本批（评审修正：编号派生双键结构，见架构 §2） | TS 基线完整盘点（context §1-§3） |
| 展示与信息（重命名/折叠/空态/移动端/撰写区选择器） | E2 | ✅ 本批（评审补：US2.5 选择器、桌面添加按钮隐藏、折叠记忆分端） | 排序置顶归会话管理域（边界已声明） |
| 移除（三选一 = SDR-001 设计增强） | E3 | ✅ 本批（评审修正：v2 不发射删除事件，如实披露） | TS 基线（本地隐藏+尽力删）如实记录 |
| 信任（入口补齐 + 门控扩大 = SDR-002 设计增强） | E4 | ✅ 本批（评审修正：跨进程感知需重启、跨客户端同步降为提案） | TS 现网无入口、门控仅 MCP——已披露 |
| 多工作区并发模型（D1/D5 裁决；钉扎标记设计增强） | E5 | ✅ 本批（评审修正：US5.3 钉扎改标设计增强——TS 嵌入宿主为 v1 栈） | 实现细节引用 rust-core-design 08 |
| 跨引擎并存（SDR-005 残余窗口） | E6 | ✅ 本批（评审修正：US6.1 剥离机制措辞） | |
| 手动刷新 | E7 | ✅ 本批 | |
| 非功能（索引读模型/原子写/数量级） | N1-N4 | ✅ 本批 | |

## 开放项

| # | 项 | 备注 |
|---|---|---|
| 1 | 根目录消失的注册表行为（启动时跳过+保留 vs 清理）| TS 侧行为未侦察（重建/合并路径跳过存在性检查是已知事实，根消失后的 list/count 行为待确认）；rust 侧设计已定（跳过+warn+保留行）——补 TS 侧事实后对齐 |
| 2 | v1/desktop 消费面盘点 | v1 `services/workspace` 与 desktop 的工作区消费面未盘点（本批 v2 主线已覆盖） |
| 3 | 未来增强提案五项 | 已登记 [proposals.md](proposals.md) P-001~P-005（唯一登记处），裁决后转域内 SDR |
| 4 | 排除空会话（exclude_empty）的数据源 | 归会话持久化域开放项 #5（lastPrompt）——**需求裁决已定（2026-09-11）**：lastPrompt = 最近一条用户输入无论成败 + 惰性物理创建（持久化域 SDR-004，原生新会话不产生空目录；过滤保留服务存量/TS 兼容桶），随 rust plans/08 W5 落地 |
| 5 | 删除模式参数的 web 确认框文案（三选一的措辞与"鼓励清理"分寸） | 随 rust W6 实现批次定稿；需求 US3.2 已定语义 |
| 6 | ~~workspace 生命周期事件（created/updated/deleted）的 Rust 发射面未在 plans/08 契约表中~~ **已裁决（2026-09-11）✅**：并入 plans/08 W2（注册/改名/删除落位即发射，含多客户端同步 e2e 验收） | 网页消费端已存在（`applyWorkspaceEvent`）；TS v2 现网仍不发射（仅 v1 发射），Rust 为补齐方 |
