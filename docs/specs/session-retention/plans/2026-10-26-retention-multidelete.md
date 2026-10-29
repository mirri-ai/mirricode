# 批次 7 实施计划：归档视图多选删除（SDR-011）

> 设计依据：`docs/specs/session-retention-requirements.md` SDR-011、US3.3a（含 reviewer 修订）。
> 范围：web 管理页纯前端编排，无 rust 改动。
> 分支：worktree `session-retention-multidelete`，基线 `latest`。

## 任务分解

### Task 1：选择状态纯函数（lib）

`apps/mirri-web/src/lib/retentionManagement.ts` 新增：

- `RetentionSelectionState`：`{ ids: ReadonlySet<string> }`（纯数据，ref 里包 Set）
- `toggleSelection(state, id): RetentionSelectionState` — 单行切换
- `toggleVisibleAll(state, visibleIds): RetentionSelectionState` — 可见集切换开关（全选↔全不选，只动可见 id）
- `selectionIntersect(state, currentRows): { rows: AppRetentionEntry[]; goneIds: string[] }` — 确认时刻的交集计算（rows = 仍在列表的选中行；goneIds = 已离开的选中 id，报"已离开列表"）
- `selectionBytes(rows): number` — 交集行当前占用之和（确认框释放量口径的会话本体部分）

单测（retentionManagement.test.ts 追加）：四个函数的 Given-When-Then——跨过滤保留（用 ids 模拟行集变化）、全选只动可见集、交集把消失 id 归入 goneIds、字节数求和。

### Task 2：面板接线（RetentionPanel.vue）

- `selectedIds = ref<Set<string>>(new Set())`
- `ui/Checkbox.vue` 补 `indeterminate?: boolean` prop（映射到原生 checkbox 的 indeterminate DOM 属性；partial 态渲染需要）——表头全选用 `:model-value` = 可见集全部选中 + `:indeterminate` = 部分选中；行首 Checkbox `:model-value` = selectedIds.has(id)
- 复选框列（仅归档视图）：表头与行首各一格；归档视图的 `grid-template-columns`（基础 6 列模板）与 720px 响应式布局各加一列前导 checkbox 列（compressed-cols 模板不动——已压缩视图无复选框）
- 工具栏追加"删除选中（N）"按钮：`v-if="view === 'archived' && selectedIds.size > 0"`，danger-soft
- `onDeleteSelected`：
  - `selectionIntersect(selectedIds, archivedRows.value)` 求交——**行源是未过滤的 archivedRows**（过滤隐藏的选中 id 仍计入，US3.3a 第 1 条），非 filteredRows/sortedRows
  - 交集为空且 goneIds 非空：**不弹确认框**，仅出 gone-only 结果横幅（"已离开列表"），清空选择，结束
  - 交集非空：confirm 弹窗插值复用**档位确认文案 key**（`retentionTierConfirmBody` 的 {count}/{size} 形态，非单删的 {title} 形态），size = 交集行占用之和 + 附件随后回收口径
  - 确认后逐条 `api.deleteRetentionSession`（**顺序**执行，避免目录锁竞争）→ 失败收集
  - 结果横幅扩展 `tierOutcome` 形态：deletedCount / reclaimedBytes / failures / **goneIds**（goneIds 在**结果横幅内**报"已离开列表（被压缩/删除/恢复），未计入本次操作"，非前置横幅——US3.3a 第 3 条措辞"在结果中报"）；渲染条件改为 anyAction = 有删除或有失败或**有 gone**
  - 完成 → `load({silent:true})` → 清空选择
- 防重入：`tierBusy` 同款 gate（批量执行中禁用全部档位按钮、行操作、复选框——US3.4 口径）
- `onRestore` 加固：`restoreSession` 返回 false（失败已在 composable 层 pushOperationFailure，非静默）时**跳过 reload**，避免失败后刷掉本地状态造成误判成功

### Task 3：i18n + 空态

- zh/en：`retentionDeleteSelected`（"删除选中（{count}）"，对齐文件既有 {count} 插值惯例）、`retentionSelectedGone`（"{count} 条已离开列表（被压缩/删除/恢复），未计入本次操作"）
- 空选择时按钮不渲染（`selectedIds.size > 0` 已覆盖）

### Task 4：验证收尾

- typecheck + vitest 全量（web）
- 验收对照：US3.3a 六条逐条核对（勾选保留/全选范围/交集剔除/确认口径/成败回报/防重入）
- changeset：minor（新用户能力）

## 依赖

1 → 2 → 3 → 4（严格线性，Task 2 依赖 Task 1 的纯函数）。

## 边界与风险

- 选择跨视图切换保留（SDR-011：跟 session id 走）——但"删除选中"按钮只在归档视图渲染，已压缩视图下选择"沉默"存在无入口，符合规格
- 顺序执行逐条删除（非并行）：避免目录锁竞争放大失败率；N 大时慢但可接受（档位批删 rust 侧同为逐条）
- 不新增 wire 面：`retention.delete` 单删循环
