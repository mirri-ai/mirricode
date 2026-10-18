# 会话持久化与索引 — ROADMAP

> 本文只做**状态与批次管理**，不承载需求或设计内容（那些见 [session-persistence-requirements.md](session-persistence-requirements.md) / [session-persistence-architecture.md](session-persistence-architecture.md)）。
> 领域计划文档（gitignored）：`plan/rust-session-storage-upgrade.md`。

## 批次状态

| 批次 | 内容 | 状态 | 提交 |
|---|---|---|---|
| Phase 1 | 归档标记全链路（S2 语义对齐；起点缺陷：归档仅落盘会话报错） | ✅ 已合入 | `ef9d8ace` |
| Phase 2 | 工作区桶布局 + 一次性平铺迁移（S3；`workDir` 必记） | ✅ 已合入 | `0fd773fb` |
| Phase 3 | SQLite 会话/工作区索引（S4；读模型 + 写通 + 对账；`workspaces.json` 镜像） | ✅ 已合入 | `c683a73b` |
| 领域文档 | 需求/架构/context 三件套 + glossary 术语 S1–S4 | ✅ 本批 | — |
| 标题批 | 标题自动派生（N10）：`is_custom_title` + `session/title.rs`（脱敏/截断，TS 对齐）+ send/rename/fork 集成 + `session.meta.updated` 广播；基线缺口补录（需求 US10） | ✅ 已合入 | `56d81ca5` |
| Phase 4 | 置顶 + 拖拽排序（见下） | ⏳ 待启动 | — |

## Phase 4 前置：会话管理域立项（已完成 2026-09-09）

"会话管理"域已立项并完成基线裁决（置顶、工作区/会话排序；拖动即固化整组、新条目插普通区顶部、契约同定 Rust 先行）。后续执行见该域 ROADMAP 的 M1–M5 批次：

1. ~~立需求文档~~ ✅ `session-management-requirements.md`（SDR-001~003 已裁决）
2. wire 契约登记（M1）→ 引擎实现（M2）→ 网页交互（M3）→ TS 对齐（M4）
3. TS v2 侧是否同批实现——已裁决：**不同批**，按 SDR-003 Rust 先行

存储侧就绪度：`workspaces.sort_order` 列已预留（架构 §5.2，ADR-002）；排序真相文档（order.json / workspace-order.json）与 `pinned` 字段在 M2 落地（见会话管理域 ADR-001/002 的定位收窄说明）。

## 开放项（承接 context §3）

| # | 项 | 所属 | 备注 |
|---|---|---|---|
| 1 | `archivedOnly` 列表参数（Web 设置页在用） | 本域 | 小；与 Phase 4 契约评审合并做；**随多工作区计划 W5 过滤面落地（[rust-core-design plans/08](../rust-core-design/plans/08-multi-workspace.md) 跨域协调节）** |
| 2 | `childOf` 子会话过滤 | 本域 | 依赖会话管理域 E5 的父子关系建模 |
| 3 | 搜索扫描集与归档排除的语义张力 | 会话管理域 | 契约评审时裁决 |
| 4 | CLI 接入会话索引加速 | 本域 | 非阻塞优化 |
| 5 | `lastPrompt` 持久化 | 本域 | **需求级裁决已定（用户 2026-09-11）**：语义 = 最近一条用户输入无论成败；同批裁决惰性物理创建（从未对话不落物理目录与索引行，首条消息才物化——SDR-004，Rust 设计增强）。实现随多工作区计划 W5 brief（[rust-core-design plans/08](../rust-core-design/plans/08-multi-workspace.md)） |
| 6 | 大小写分裂桶合并读（TS 历史形态） | 本域 | ~~等 Windows 真实数据出现再评估~~ → **已裁决随多工作区计划本期移植**（用户 2026-09-11，否决缓办；实现与验收见 [rust-core-design plans/08](../rust-core-design/plans/08-multi-workspace.md) W5/W9 + D3） |
| 7 | Rust 加固：`SessionId` 合法性校验（US9.1） | 本域 | 存储层入口强制；`create_session_with_id` 面当前裸透传 |
| 8 | Rust 加固：排序活跃时间与元数据更新时间拆分 | 会话管理域 SDR-004 | `update_metadata` 无条件刷新 `updated_at` 待修 |
| 9 | TS 对齐批次：v1 索引只增不删 / v1 重建未接线 / v1 无锁并发 / 两代无自动归置 / v2 定时任务归属矛盾 | TS 引擎 | 见 context §5 差距表；若 v1/v2 继续服役需排期 |
| 10 | 收起策略对齐（已裁决，原待举证项）：Rust 补 `keepAliveOnExit` 配置面（默认真停止 + 可配置继续跑，含环境变量）与 `printBackgroundMode` 三态语义 | 子代理域 US8.4 细化 + 本域实现 | 事实见 context §5；回签第 4 项已修正 |
| 10a | v1 的 8 秒有界收尾窗口（先取消、再有界沉降）Rust 侧缺失——关闭时进行中回复的收尾对齐项 | 会话关闭行为 | v2 亦无此机制；是否补齐由子代理域 US8.4 评审定 |
| 11 | 附件全局内容寻址存储迁移（ADR-007 / SDR-003）：存储根上移 `<数据根>/blobs/`、fork 停止复制本体、删除不级联本体、引用感知延迟回收 + 溢出文件 30 天老化窗口、存量一次性上移合并 | 本域（实现）+ TS 引擎（对齐） | fork 痛点驱动（会话管理域 SDR-007 附件共享条款的底座）；现状矩阵见 context §7。**存量上移范围（2026-09-16 补充，多模态输入域 SDR-004 对齐）**：除各引擎会话域内 blobs 外，须显式纳入 TS `<会话目录>/media-originals/`（提示摄入的媒体原件留档——多模态输入域 E2/ADR-004 的恢复句柄来源），ADR-007 背景已提及但原迁移清单未点名 |
| 11 | 子代理记录载体落地（配合子代理域批次）：`agents/<id>/` 记录流 + state.json agents 注册表 + 混合日志拆分迁移 | 本域（架构 ADR-006） | 与子代理域 ADR-002 衔接；其 10 项确认已由本域回签 |
| 12 | 会话导出需求立项：打包/导入的显式需求（现仅在架构 ADR-006/007 影响面引用），须纳入"压缩记录随包、导入后按压缩域 ADR-005 计数重建且模型视图保真"的验收 | 本域（需求）+ 压缩域 | 跨域架构评审 B4；`session.export` 契约与 e2e 既有失败修复合流 |
| 13 | cross_backend 两个无关既有失败的修复合流 | 其他域 | `model.list` id 字段、`session.export` manifest |
| 14 | 恢复路径性能修复（SDR-005 / ADR-017，2026-10-15 立项）：就地折叠（纯函数接口保留为薄包装）→ 流式加载消多份副本 → 单一折叠结果（主日志只折叠一遍、转向缓冲复用结果）→ 子代理懒重放遵从（就绪不等待子代理、按需重建、保留回合标识上界扫描）。验收：同一记录流折叠结果与改造前逐字段等价；实测大会话冷重开亚秒级、峰值内存 < 500MB | 本域 | 依据：`docs/specs/reports/rust-v3-session-cold-open-perf-20261015.md`（双 reviewer 核验）；现状事实见 context §9。落地顺序按依赖：先机械项（流式加载）再就地折叠再单一折叠结果+懒重放。检查点频率（事实 4）与恢复不阻塞读面（事实 6）为独立开放项，**不在本批**：前者待修复后实测决定、后者改 API 契约需单独立项 |
| 15 | session.search 扫描集不含未唤醒子代理 journal（懒恢复的设计后果，ADR-017）：`read_only_histories` 只返回主代理历史，休眠子代理（已注册未水合）的 journal 在水合前不被扫描——旧急切恢复使其可搜。候选：扫描时对 roster children 做 journal 只读 replay，或接受并文档化缺口 | 本域 | review 2026-10-16 记录；代码边界注释已钉在 `read_only_histories` |
