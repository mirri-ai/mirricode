# 批次 6 实施计划：归档浏览单入口合并（SDR-009）

> 设计依据：`docs/specs/session-retention-requirements.md` SDR-009/SDR-010、US3.1/US3.2 修订；
> `docs/specs/session-retention-architecture.md` ADR-010。
> 分支：worktree `session-retention-merge`，基线 `latest`。

## 任务分解（每任务独立可验证）

### Task 1：`retention.list` wire 增补 `created_at`（rust 侧）

- `rust/crates/mirri-proto/src/retention.rs`：`RetentionListEntry` 加 `created_at: String`，**带 `#[serde(default)]`**（同 `tier_days` 模式，保持 legacy JSON 反序列化容忍；索引行有权威 `created_at`，语义上非 Option，但 serde 容忍是既有约定）。
- 同步更新既有 legacy 容忍测试 `should_default_every_new_field_to_none_when_deserializing_a_legacy_row`：构造的 legacy JSON 无 `created_at`，断言其反序列化为 `String::new()`（而非失败）。
- `rust/crates/mirri-embedded/src/embedded_server/retention_handlers.rs`：行组装填充 `created_at`——索引路径用 `row.created_at`；**无索引降级路径**（`list_persisted_sessions` → `PersistedSessionInfo` 不含 created_at）经 `read_persisted_info` 的 `metadata.created_at` 取，读失败回退 `String::new()`。
- rust 测试：既有 `retention.list` 形状测试（`should_serve_real_handler_shapes_when_the_retention_management_methods_are_called` 一族）断言 `created_at` 随行返回；补一条 Given-When-Then：归档视图行含创建时间且与 state.json 的 created_at 一致。
- 验证：`cargo test -p mirri-proto -p mirri-embedded --lib`。

### Task 2：web adapter 与类型面透传 `createdAt`

- `apps/mirri-web/src/api/daemon/wire.ts`：`WireRetentionListEntry`（或等价 wire 类型）加 `created_at: string`。
- `apps/mirri-web/src/api/daemon/ws-rpc-api.ts`：`toAppRetentionEntry` 映射 `createdAt: entry.created_at`。
- `apps/mirri-web/src/api/types.ts`：`AppRetentionEntry` 加 `createdAt: string`。
- web 单测：`toAppRetentionEntry` 非导出无既有测试——在 `apps/mirri-web/src/api/daemon/` 既有 mapper 测试文件（如 `ws-rpc-models.test.ts`）为 `toAppSessionFromRpcView` 同族补映射断言（Given-When-Then 命名）。
- 验证：`pnpm --filter @mirri-ai/mirri-web run typecheck`。

### Task 3：管理页"归档"视图搜索 + 工作区过滤 + 空态

- `apps/mirri-web/src/components/settings/RetentionPanel.vue`：
  - 工具栏（仅归档视图渲染）：标题搜索框（ToolbarSearch，复用原 Archived 页组件）+ 工作区下拉（从当前归档行集合派生去重排序，值 `all` | workspace bucket key，**过滤对 `row.workspace` 生效**——retention 行携带的是 workspace 而非 AppSession.cwd）。
  - `filteredRows` computed：搜索（title 小写包含）→ 工作区过滤 → 现有排序管线；已压缩视图不受搜索/过滤影响（ADR-010）。
  - 空态区分：`archivedRows` 为空 → "没有归档会话"；非空但过滤后为空 → "没有匹配的会话"（i18n key 复用原页 `archivedNoMatch` 语义，retention 域新增 key）。
- i18n：zh/en 各加 `retentionSearch`、`retentionAllWorkspaces`、`retentionNoMatch`。
- web 测试：过滤/排序逻辑抽到 `apps/mirri-web/src/lib/retentionManagement.ts`（纯函数 `filterRetentionEntries`），单测覆盖搜索命中/未命中/工作区过滤/组合。
- 验证：typecheck + vitest。

### Task 4：归档视图排序扩集（创建时间 + 标题）

- `apps/mirri-web/src/lib/retentionManagement.ts`：`RetentionSortKey` 归档视图集合扩为 `size | archivedAt | createdAt | title`（US3.2 封闭集合）；`sortRetentionEntries` 补两个比较器（createdAt ISO 字典序，**空串按未知沉底**——沿用 `sortRankIsUnknown` 模式；title `localeCompare('zh')`，对齐原 Archived 页）；`sortableKeysForView('archived')` 与 `defaultSortForView` 更新（默认仍 archivedAt 降序）。
- `RetentionPanel.vue`：可排序列头渲染扩集（表头文案 i18n）。
- web 测试：`retentionManagement` 排序单测扩两键（升/降序、zh 排序）。
- 验证：typecheck + vitest。

### Task 5：桌面 SettingsView archived tab 按 v3 降级下线

- `apps/mirri-web/src/components/settings/SettingsView.vue`：
  - tabs 列表：`archived` 条目仅在 `backend !== 'v3'` 时保留（复用既有 `visibleTabs` 过滤模式，retention tab 已同构 gated）。
  - archived tab 的全部状态/加载管线（`archivedItems` 等 5 refs、`loadAllArchived`、`filteredArchived`、`groupedArchived`、`archiveTime`、`onRestore`）与模板段仅在 tab 可见时可达——v3 下不可达即视为下线；为避免死代码，用 `v3` 分支条件包裹或提炼（与现有 `visibleTabs` 模式一致的最小改法）。
  - 注意 `loadArchivedSessions`（composable 层）**保留**——`MobileSettingsSheet` 仍在用（ADR-010）。
- web 测试：tab 过滤逻辑提炼为纯函数（放 `lib/`，输入 `backend` 与全量 tab 集，输出可见集）后单测——本仓库无组件测试 harness，**不在 SettingsView 上直接断言**；断言 v3 下 archived 不在、retention 在，非 v3 反之。
- 验证：typecheck + vitest。

### Task 6：回归收尾

- 全量验证：rust workspace 测试 + clippy、web typecheck + vitest 全量、（TUI 不动）。
- 变更集：minor（用户可见的设置信息架构变化 + 能力合并）——若判定为既有功能改进则 patch。
- SDR-009 的验收对照自查（US3.1/US3.2 新增验收条目逐条核对）。

## 任务依赖

1 → 2 → (3, 4 可并行，都依赖 2 的 createdAt) → 5（独立，可与 3/4 并行）→ 6。

## 风险与边界

- `retention.list` 增字段是 wire 兼容增量（serde 默认容忍旧客户端缺字段——`created_at` 非 Option 时旧客户端**收**新字段无害；rust 端 `#[serde(default)]` 不适用于必填出参，出参多字段对旧客户端只是忽略）。ACP/TSV 权威方法表无新方法（不改方法集，只改响应字段），路由计数 pin 不动。
- v1/v2 降级路径的判断源：`SettingsView` 已有 `backend` prop（'v1' | 'v2' | 'v3'）。
- 移动端不动（ADR-010 范围限定）。
