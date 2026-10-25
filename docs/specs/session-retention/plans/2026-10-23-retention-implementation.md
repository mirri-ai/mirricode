# 会话归档存储管理 — 实施计划

> 对应设计：`docs/specs/session-retention-requirements.md`（SDR-001..008）与 `docs/specs/session-retention-architecture.md`（ADR-001..009）。
> 执行方式：worktree 隔离开发（`.worktrees/session-retention`），任务顺序即依赖顺序；每任务含验证命令，全部完成后统一验证与合并。
> 硬约束：rust 引擎改动全部落在 `rust/crates/`；TS 侧仅 web 前端（`apps/mirri-web`）与 TUI 标识；冻结的 TS 核心包不改。

## Task 1: 存储原语 — gzip 压缩/解压与双形态读（mirri-persistence）

**做什么**：在 `rust/crates/mirri-persistence` 新增压缩原语模块：
- `compress_wire_log(path) -> Result<u64>`：流式 gzip（固定缓冲区）、写临时文件 → fsync → 原子 rename 为 `wire.jsonl.gz`，返回压缩前字节数
- `decompress_wire_log(path) -> Result<()>`：`.gz` → `wire.jsonl`（同原子替换），完成后删除 `.gz`
- `probe_compressed(dir) -> CompressedShape`：磁盘形状判定（`.gz` 在且 `.jsonl` 不在 → 压缩态；`.jsonl` 在 → 非压缩态；并存 → 非压缩 + 残留信号）——**唯一真值源**（ADR-007），自愈消费方据此补写/清除标记
- `JsonlAppendLog` 读路径扩展：打开日志时按形状选择——未压缩走现有路径；压缩态包装流式 gzip reader 逐行读。全部现有读消费方（restore 折叠、replay、export、搜索）透明接入

**改哪里**：`rust/crates/mirri-persistence/src/append_log.rs`（读契约扩展）、新文件 `rust/crates/mirri-persistence/src/compress.rs`、`src/lib.rs` 导出。

**验证**：
```bash
cargo test -p mirri-persistence --lib compress
# 断言：压缩-解压往返逐字节等价；中断留原样（临时文件注入失败）；双形态读行序一致；
# 并存形状（还原中断残留 .gz）判定为非压缩 + 残留信号（ADR-007 崩溃矩阵自愈的判定基础）
cargo clippy -p mirri-persistence --all-targets -- -D warnings
```

## Task 2: 存储原语 — 目录 advisory lock（mirri-persistence）

**做什么**：新文件 `rust/crates/mirri-persistence/src/dir_lock.rs`：会话目录内固定名 `lock` 文件 + OS advisory lock（Unix `flock`、Windows `LockFileEx`）；API：`try_acquire(dir) -> Option<DirLockGuard>`（拿不到立即返回 None）；RAII guard 释放。锁文件常驻（首次创建后不删，存在性不是活跃信号）。

**验证**：
```bash
cargo test -p mirri-persistence --lib dir_lock
# 断言：同进程两次 acquire 第二次失败；跨进程（子进程持锁）失败；guard drop 后可重获
```

## Task 3: 数据模型 — state.json 字段与索引列

**做什么**：
- `rust/crates/mirri-core/src/session/persistence.rs` 的 `PersistedSessionMetadata` 新增 `archived_at` / `compressed` / `compressed_at` / `pre_compressed_size`（`#[serde(default)]`，camelCase alias，时间字段双拼法兼容）
- `set_archived(true)` 同时写 `archived_at`；`set_archived(false)` 清空全部四字段（含压缩字段——恢复归档必经还原）
- `rust/crates/mirri-session-index`：`sessions` 表新增 `archived_at` / `compressed` / `compressed_at` 列（migration，行从磁盘派生自愈覆盖）
- `SessionInfo` / `PersistedSessionInfo` 暴露压缩态与归档时点；写通点补充（§5.3 对账规则不变）

**验证**：
```bash
cargo test -p mirri-core --lib session::persistence
cargo test -p mirri-session-index
# 断言：归档写时点/恢复清空/老文件缺字段容忍/索引列自愈；
# 标记-形状不一致自愈（.gz 在标记缺 → 补写；标记在 .gz 无 → 清除）（ADR-007）
```

## Task 4: 配置段 [retention]（mirri-core）

**做什么**：`rust/crates/mirri-core/src/config/`：
- `RetentionConfig { enabled: bool, archive_grace_days: u32(≥1, default 30), interval_hours: u32(≥1, default 24) }`，serde 缺省容忍
- 逐项回退校验：非法值 → 默认；`enabled` 解析失败 → false；只留日志
- 登记两道工序：`KNOWN_CONFIG_TOP_LEVEL_KEYS` + wire round-trip（`config/wire.rs`）
- 暴露**每轮重读**的读取接口（任务周期不快照配置——US6.1"修改保存后下一轮按新配置判定"）

**验证**：
```bash
cargo test -p mirri-core --lib config
# 断言：缺段/缺项/非法项回退；wire round-trip 保真；改配置 → 下轮读到新值
```

## Task 5: 压缩引擎（mirri-core）

**做什么**：新文件 `rust/crates/mirri-core/src/session/retention.rs`：
- 判定纯函数 `should_compress(archived_at, now, grace_days) -> bool`（无删除分支）
- 执行编排 `compress_session(dir)`：拿目录锁 → 锁内重验（重读 state.json：`archived=1` 且形状非压缩）→ 调 Task 1 压缩原语 → 写 `compressed`/`compressed_at`/`pre_compressed_size`（压缩字段写入在原子替换成功后；标记写入失败由 ADR-007 自愈覆盖）
- 存量补写：候选缺 `archived_at` 时补写为检查时刻
- 恢复归档联动：`restore` 路径（embedded `dispatch_restore_session` 之下）检测压缩形状 → 拿锁 → 解压还原 → 清压缩字段 → 清 `archived_at` + 标记（顺序：还原完成前不清任何标记；幂等可重试）
- fork 拒斥：`fork_pipeline` 入口形状检查 → 明确业务错误（"compressed session must be restored before fork"）

**验证**：
```bash
cargo test -p mirri-core --lib session::retention
# 断言：到期/未到期边界；锁内重验（伪造并发状态）；存量补写；恢复还原幂等；
# 中断后自愈（残留 .gz 并存 → 重试成功清尾）；fork 拒斥
```

## Task 6: 附件引用清单（mirri-core）

**做什么**：新文件 `rust/crates/mirri-core/src/blob_ledger.rs`：
- 引用提取：流式扫描会话目录全部记录流（主 `wire.jsonl(.gz)`、`agents/*/wire.jsonl(.gz)`、`snapshot.json`）的 `blobref:` 引用
- 惰性反查清单：`HashMap<hash, {refcount, zeroed_since}>`；首次全量构建、删除时扣减、归零记录时刻；缓存可重建
- 老化窗口复核 `reclaim_aged_blobs(older_than)`：归零超 T（常量，小时级）→ 删附件文件（复核宿主：压缩检查周期尾段，Task 7 接线）
- 即时回收接口：只回收"归零且超 T"项（供删除执行体调用）

**验证**：
```bash
cargo test -p mirri-core --lib blob_ledger
# 断言：全记录流提取（漏 agents 流即误删的回归样例）；共享附件保留；
# 归零锚点与复核（归零后 T 内不删、超 T 删）；复用重置锚点
```

## Task 7: wire 类型 — retention RPC 协议（mirri-proto）

**做什么**：`rust/crates/mirri-proto`：
- 会话列表/条目响应新增压缩字段（`compressed`、`compressedAt`、`preCompressedSize`）
- 新 RPC wire 类型：`retention.list`（两视图列表：含占用/节省/已压缩天数）、`retention.stats`（视图统计）、`retention.compress_now`、`retention.delete`（单删）、`retention.delete_tiered`（档位批删，入参为档位天数）、`retention.get_policy`（当前策略读取，US3.5 策略提示载体）

**验证**：
```bash
cargo test -p mirri-proto
```

## Task 8: 接线 — 压缩任务（mirri-embedded）

**做什么**：`rust/crates/mirri-embedded/src/embedded_server/retention_task.rs`（孤儿治理同模式）：
- `[retention]` enabled 才装配（默认不装配）；延时启动（分钟级常量 + 随机抖动）；**低优先级后台线程**（US4.1 不阻塞用户操作）
- 每轮：重读配置（Task 4 热读取）→ 扫描候选（索引 `archived=1` + 磁核核实 + 运行态排除）→ 注入 core 引擎（ADR-008 分层：embedded 持索引）→ 尾段附件复核（Task 6 `reclaim_aged_blobs`）
- 压缩动作结构化日志（会话 id、动作、时刻）

**验证**：
```bash
cargo test -p mirri-embedded --lib embedded_server -- retention_task
# 断言：默认不装配；到期压缩端到端；改配置下轮生效；
# 关闭后已压缩会话保持现状（US1.2 第 4 条）；跳过非终局
```

## Task 9: 接线 — 删除执行体与管理页 RPC（mirri-embedded）

**做什么**：
- 删除执行体：先拿目录锁（ADR-002：压缩/还原/删除三类动作统一持锁）→ 锁内重验（会话存在）→ 先目录（`delete_session_files`）后索引行（`SessionIndex::delete_session`）→ 尾部即时附件回收（Task 6）→ 幂等（半删重试清尾）
- RPC handlers（Task 7 的 6 个方法）：单删、档位批删（按 `compressed_at` 起算筛选、逐条执行、部分失败逐条回报）、立即压缩（复用 Task 5 执行体）、两视图列表 + stats、策略读取
- **既有删除入口收敛**（SDR-004）：列表删除 dispatch、workspace purge、ACP 删除尾部统一接入同一附件回收
- 删除动作结构化日志

**验证**：
```bash
cargo test -p mirri-embedded --lib embedded_server -- retention_rpc
# 断言：RPC 单删/批删/立即压缩；部分失败回报；先目录后索引行；半删幂等收敛；
# US5.1 第 5 条（删除×恢复并发：持锁互斥，无半删半还原）；
# US5.1 第 6 条（删除×压缩并发：锁串行化，最多一个生效）；
# 所有既有删除入口尾部附件回收一致
```

## Task 10: 列表/搜索压缩态透传（mirri-embedded）

**做什么**：`merged_session_entries` 与列表/搜索响应透传压缩字段（消费 Task 7 wire 类型）。

**验证**：
```bash
cargo test -p mirri-embedded --lib session_index
# 只读等价回归（US2.2 / 架构 §9）：同一会话压缩前后的列表/搜索摘要逐字段一致；
# 压缩会话查看/导出/搜索输出与未压缩对照组一致（e2e 级断言放 Task 13）
```

## Task 11: Web 前端 — 归档管理页（apps/mirri-web）

**做什么**（遵循 `apps/mirri-web/AGENTS.md`，本地重实现 wire 类型）：
- 设置区新增"归档会话"管理页：两视图（默认落"归档"视图）、列（标题/工作区/归档时间/占用；压缩视图另有：压缩时间/已压缩天数/节省量）、默认排序（压缩视图占用降序；归档视图归档时间倒序）、可排序列封闭集合（压缩视图：占用/已压缩天数/归档时间；归档视图：占用/归档时间）
- 行操作：恢复归档、立即压缩、删除（确认框：条目、释放量口径 [会话本体立即 + 附件随后]、不可恢复、取消零动作）
- 档位批量按钮（30/90/180/365）+ 确认框（N 条、X MB）+ 空范围提示 + 执行中防重入与进度反馈 + 部分失败回报
- **列表刷新**（US3.2 第 4 条）：视图激活时周期拉取刷新（或复用既有推送通道，实现侧择一）
- 策略提示（读 `retention.get_policy`）；加载中/失败重试/空态
- 会话列表与归档视图"已压缩"标识
- **压缩会话的复制入口禁用 + 提示**（"需先恢复归档"）（US2.3 前端面）
- i18n：zh/en 两份文案

**验证**：
```bash
pnpm --filter @mirri-ai/mirri-web run typecheck && pnpm --filter @mirri-ai/mirri-web run test
# e2e（MIRRICODE_E2E_SKIP_RUST=1 + mock）：列表标识、两视图、默认视图、单删确认、
# 档位批删、复制禁用提示、刷新反映外部变化
```

## Task 12: TUI — 已压缩标识与最小操作（apps/mirri-code）

**做什么**（遵循 `write-tui` skill）：会话列表条目"已压缩"标识；归档会话逐条恢复归档/删除入口（最小能力，SDR-007 边界）。**先核实 SDK 面**（`@mirri-ai/mirri-code-sdk` 是否暴露压缩字段与删除/恢复 RPC）——若 SDK 面不暴露，本任务降级为"仅标识 + 既有恢复归档"，删除入口由 web 承担，并在任务内记录裁决依据（不动冻结 TS 核心包）。行为变化则显式跑该包 test 脚本。

**验证**：
```bash
pnpm --filter @mirri-ai/mirri-code run typecheck
pnpm --filter @mirri-ai/mirri-code run test   # 仅当任务降级判定为"有行为变化"时
```

## Task 13: 全量验证与收尾

```bash
cargo test --manifest-path rust/Cargo.toml --workspace          # ≥20min，后台跑
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
pnpm test        # 默认套件（mirri-web + mirri-desktop）
```
- 端到端等价回归（US2.2）：压缩会话恢复后继续干活，wire 日志与未压缩对照组逐字节一致
- 生成 changeset（`gen-changesets` skill，minor：用户可感知新功能）
- 文档：`gen-docs` skill 判断是否需要用户文档更新（配置项 + 管理页）

## 依赖图（修订：proto 前置于 embedded handlers；blob ledger 前置于接线）

```
Task 1 ─┬─► Task 5 ─────────────┐
Task 2 ─┘                       │
Task 3 ─► Task 5                ▼
Task 4 ──────────────────► Task 8（压缩任务）─► Task 10（透传）─► Task 11（web）
Task 6（blob ledger）─┬─► Task 8                      │
                      └─► Task 9（删除/RPC，依赖 Task 7 proto + Task 5）─┘
Task 7（proto，依赖 Task 3 的字段语义）─► Task 9
Task 12（TUI，依赖 Task 10 的 SDK/展示面）
Task 13（收尾，依赖全部）
```
