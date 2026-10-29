# 会话归档存储管理 — ROADMAP

> 状态与进度唯一维护点（需求/设计见同目录规格文档；本文件不含需求内容）。
> 立项：2026-10-23（v1 基线）；2026-10-23 修订为 v2（自动压缩 + 手动删除 + 管理页，SDR-001/002/007）。

## 当前状态

- [x] 需求文档 v2 基线（session-retention-requirements.md，SDR-001 至 SDR-008）
- [x] 架构文档 v2 基线（session-retention-architecture.md，ADR-001 至 ADR-009）
- [x] 现状盘点快照（session-retention-context.md，2026-10-23）
- [ ] 需求/设计用户评审
- [ ] 实现批次（见下）

## 批次规划

### 批次 1：数据模型与存储原语

- `state.json` 新增 `archivedAt` / `compressed` / `compressedAt` / `preCompressedSize`（归档、恢复归档、压缩、还原四处的写入与清除；双拼法兼容；存量无字段容忍）
- 会话索引新增 `archived_at` / `compressed` / `compressed_at` 列（自愈规则覆盖）
- mirri-persistence：gzip 压缩/解压原语（流式、临时文件 + 原子替换、压缩态形状探测）
- mirri-persistence：`JsonlAppendLog` 双形态读（gzip 流式 reader，全部读消费方接入）
- mirri-persistence：目录级 `lock` 文件 + OS advisory lock 原语（ADR-002）
- 配置段 `[retention]`（enabled / archive_grace_days / interval_hours；解析、逐项回退校验、KNOWN_CONFIG_TOP_LEVEL_KEYS + wire round-trip 登记）
- 持久化域架构文档回写（ADR-006：字段登记、索引列登记、附件联动修订、目录锁原语登记）

### 批次 2：自动压缩引擎

- 判定纯函数（时间窗计算，无删除分支）
- embedded 扫描候选供给（索引起点 + 磁盘核实 + 运行态排除 + 目录锁探测）
- 压缩执行、存量补写 `archivedAt`（SDR-008）
- 接线：延时启动 + 周期调度 + 随机抖动 + 日志（孤儿治理任务同模式，ADR-008 分层）

### 批次 3：删除管线与附件回收

- 删除执行体（单删 + 档位批量收敛到同一执行体；先目录后索引行 + 既有孤儿收敛）
- 附件引用清单（全记录流提取：主/agents/snapshot；惰性反查；老化窗口复核）
- 附件联动回收接删除路径尾部

### 批次 4：恢复还原与并发安全

- 恢复归档还原流程（目录锁 → 解压还原 → 标记清除顺序，幂等）
- fork 拒斥（入口业务错误 + 提示语，SDR-003）
- 崩溃矩阵自愈收敛全覆盖（§7 七行）
- 并发集成测试（US5.1 四条验收、双压缩任务）

### 批次 5：界面（SDR-007：与核心同批交付）

- wire 协议 + `SessionInfo`：压缩态呈现（"已压缩"标识、压缩元数据）
- 管理页 API：归档/压缩两视图列表 RPC（占用、节省、已压缩天数）、立即压缩、单删、档位批删
- 网页端：归档管理页（两视图、排序、批量档位按钮、确认对话框）+ 会话列表"已压缩"标识
- 终端端：会话列表"已压缩"标识（管理页交互以网页端为准绳，终端最小呈现）

### 批次 6：增量——单入口合并（SDR-009）

- `retention.list` wire 增补 `created_at`（mirri-proto + embedded handler + web adapter）
- 管理页"归档"视图：标题搜索 + 工作区过滤（客户端过滤，并入原 Archived 页能力；含"无匹配"空态文案）
- "归档"视图排序扩集：占用、归档时间（archivedAt 权威口径）、创建时间、标题
- 桌面 SettingsView 在 v3 后端下移除 archived tab（状态与恢复处理器下线；`loadArchivedSessions` 管线保留供移动端；v1/v2 保留原 tab 降级）

（页内自动压缩设置卡为 SDR-010 独立增量，已随 `e3a6743a4` 交付，不属本批次。）
